//! Transport über einen USB-HID-Dongle. Frames laufen über den Funk zwischen zwei Dongles.

mod framing;

use std::ffi::{CStr, CString};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use hidapi::{HidApi, HidDevice};
use tokio::sync::mpsc;
use tokio::task;
use transport::{ConnectionState, Transport, TransportError, TransportStatus};

use framing::{fragment, Reassembler, REPORT_LEN, TYPE_LINK, TYPE_STATUS};
pub use framing::MAX_FRAME_LEN;

const VENDOR_ID: u16 = 0x1915;
const PRODUCT_ID: u16 = 0x5211;
const LINK_WAIT: Duration = Duration::from_secs(10);
const READ_POLL_MS: i32 = 100;
const SLOW_WRITE: Duration = Duration::from_millis(250);

enum Incoming {
    Link(bool),
    Report([u8; REPORT_LEN]),
    Failed(String),
}

/// Alle Dongle-Pfade am Rechner. Zwei Dongles haben dieselbe VID/PID, deshalb über den Pfad unterscheiden.
pub fn find_paths(api: &HidApi) -> Vec<CString> {
    api.device_list()
        .filter(|d| d.vendor_id() == VENDOR_ID && d.product_id() == PRODUCT_ID)
        .map(|d| d.path().to_owned())
        .collect()
}

pub struct UsbDongleTransport {
    writer: Arc<Mutex<HidDevice>>,
    incoming: mpsc::UnboundedReceiver<Incoming>,
    link_up: bool,
    closed: bool,
    next_frame_id: u8,
    reassembler: Reassembler,
    reports: Arc<AtomicU32>,
}

impl UsbDongleTransport {
    /// Öffnet den Dongle unter `path`. Das Lesen läuft in einem eigenen Thread, weil `hidapi` blockiert.
    pub fn open(api: &HidApi, path: &CStr) -> Result<Self, TransportError> {
        let reader = api.open_path(path).map_err(backend)?;
        let writer = api.open_path(path).map_err(backend)?;
        let (tx, incoming) = mpsc::unbounded_channel();
        let reports = Arc::new(AtomicU32::new(0));
        let reader_reports = Arc::clone(&reports);
        std::thread::spawn(move || read_loop(reader, tx, reader_reports));
        Ok(Self {
            writer: Arc::new(Mutex::new(writer)),
            incoming,
            link_up: false,
            closed: false,
            next_frame_id: 0,
            reassembler: Reassembler::default(),
            reports,
        })
    }

    /// Anzahl aller bisher vom Dongle gelesenen Berichte; zeigt, ob überhaupt etwas ankommt.
    pub fn reports_received(&self) -> u32 {
        self.reports.load(Ordering::Relaxed)
    }

    /// Setzt auf dem Dongle die LED auf Grün (`true`) oder zurück auf Blau (`false`).
    pub async fn set_authenticated(&mut self, authenticated: bool) -> Result<(), TransportError> {
        let mut out = [0u8; REPORT_LEN + 1];
        out[1] = TYPE_STATUS;
        out[2] = authenticated as u8;
        self.write(out.to_vec()).await
    }

    fn ensure_open(&self) -> Result<(), TransportError> {
        if self.closed || !self.link_up {
            Err(TransportError::NotConnected)
        } else {
            Ok(())
        }
    }

    fn apply(&mut self, msg: Incoming) -> Result<Option<Vec<u8>>, TransportError> {
        match msg {
            Incoming::Link(up) => {
                if self.link_up && !up {
                    self.link_up = false;
                    self.reassembler.reset();
                    return Err(TransportError::Closed);
                }
                self.link_up = up;
                Ok(None)
            }
            Incoming::Report(report) => Ok(self.reassembler.push(&report)),
            Incoming::Failed(reason) => {
                self.link_up = false;
                self.reassembler.reset();
                Err(TransportError::Backend(reason))
            }
        }
    }

    async fn write(&self, out: Vec<u8>) -> Result<(), TransportError> {
        let started = Instant::now();
        let writer = Arc::clone(&self.writer);
        let joined = task::spawn_blocking(move || -> Result<(), TransportError> {
            let device = writer.lock().map_err(|_| TransportError::Backend("Schreibsperre vergiftet".into()))?;
            device.write(&out).map(|_| ()).map_err(backend)
        })
        .await;
        if started.elapsed() > SLOW_WRITE {
            eprintln!("Slow HID write: {} ms", started.elapsed().as_millis());
        }
        joined.map_err(|e| TransportError::Backend(e.to_string()))?
    }

    async fn wait_for_link(&mut self) -> Result<(), TransportError> {
        let wait = async {
            while !self.link_up {
                let msg = self.incoming.recv().await.ok_or(TransportError::Closed)?;
                self.apply(msg)?;
            }
            Ok(())
        };
        tokio::time::timeout(LINK_WAIT, wait).await.map_err(|_| TransportError::Timeout)?
    }
}

impl Transport for UsbDongleTransport {
    async fn connect(&mut self) -> Result<(), TransportError> {
        self.closed = false;
        self.wait_for_link().await
    }

    async fn disconnect(&mut self) -> Result<(), TransportError> {
        self.closed = true;
        self.link_up = false;
        self.reassembler.reset();
        Ok(())
    }

    async fn send(&mut self, frame: &[u8]) -> Result<(), TransportError> {
        self.ensure_open()?;
        let frame_id = self.next_frame_id;
        self.next_frame_id = self.next_frame_id.wrapping_add(1);
        for report in fragment(frame_id, frame)? {
            // Erstes Byte ist die Report-ID (0), danach der Bericht.
            let mut out = [0u8; REPORT_LEN + 1];
            out[1..].copy_from_slice(&report);
            self.write(out.to_vec()).await?;
        }
        Ok(())
    }

    async fn receive(&mut self, buf: &mut [u8]) -> Result<usize, TransportError> {
        self.ensure_open()?;
        let frame = loop {
            let msg = self.incoming.recv().await.ok_or(TransportError::Closed)?;
            if let Some(frame) = self.apply(msg)? {
                break frame;
            }
        };
        if frame.len() > buf.len() {
            return Err(TransportError::BufferTooSmall { needed: frame.len() });
        }
        buf[..frame.len()].copy_from_slice(&frame);
        Ok(frame.len())
    }

    fn get_status(&self) -> TransportStatus {
        let state = if self.link_up && !self.closed {
            ConnectionState::Connected
        } else {
            ConnectionState::Disconnected
        };
        TransportStatus { state, max_frame_len: MAX_FRAME_LEN }
    }

    async fn reconnect(&mut self) -> Result<(), TransportError> {
        self.connect().await
    }
}

fn read_loop(device: HidDevice, tx: mpsc::UnboundedSender<Incoming>, reports: Arc<AtomicU32>) {
    let mut report = [0u8; REPORT_LEN];
    while !tx.is_closed() {
        match device.read_timeout(&mut report, READ_POLL_MS) {
            Ok(REPORT_LEN) => {
                reports.fetch_add(1, Ordering::Relaxed);
                let msg = if report[0] == TYPE_LINK { Incoming::Link(report[1] == 1) } else { Incoming::Report(report) };
                if tx.send(msg).is_err() {
                    break;
                }
            }
            Ok(_) => {}
            Err(e) => {
                let _ = tx.send(Incoming::Failed(e.to_string()));
                break;
            }
        }
    }
}

fn backend(e: hidapi::HidError) -> TransportError {
    TransportError::Backend(e.to_string())
}
