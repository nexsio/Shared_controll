//! `Transport` über Bluetooth LE (BlueZ, GATT) für Linux.
//!
//! Ein Gerät tritt als Server auf (wirbt und bietet den GATT-Dienst an), das andere als Client (sucht,
//! verbindet, koppelt). Die Characteristic verlangt authentifizierte Verschlüsselung. Pro Frame wird genau
//! ein GATT-Paket übertragen.
//!
//! Dieser Crate kennt weder Protokollinhalte noch Sicherheitsschicht.

use std::{pin::Pin, time::Duration};

use bluer::{
    adv::{Advertisement, AdvertisementHandle},
    gatt::{
        local::{
            characteristic_control, Application, ApplicationHandle, Characteristic, CharacteristicControl,
            CharacteristicControlEvent, CharacteristicNotify, CharacteristicNotifyMethod, CharacteristicWrite,
            CharacteristicWriteMethod, Service,
        },
        remote, CharacteristicReader, CharacteristicWriter,
    },
    Adapter, AdapterEvent, Address, Device, Session, Uuid,
};
use futures::{pin_mut, StreamExt};
use tokio::time::{sleep, timeout, Instant};
use transport::{ConnectionState, Transport, TransportError, TransportStatus};

pub const SERVICE_UUID: Uuid = Uuid::from_u128(0x5f0c_a1b2_7d3e_4c11_9a52_0e6b_8d41_c7a0);
pub const CHARACTERISTIC_UUID: Uuid = Uuid::from_u128(0x5f0c_a1b3_7d3e_4c11_9a52_0e6b_8d41_c7a0);

const DEFAULT_CONNECT_TIMEOUT: Duration = Duration::from_secs(60);
const CONNECT_RETRIES: u32 = 5;
const SERVICES_TIMEOUT: Duration = Duration::from_secs(15);

fn backend(e: impl std::fmt::Display) -> TransportError {
    TransportError::Backend(e.to_string())
}

enum Mode {
    Server { local_name: String },
    Client,
}

struct Link {
    reader: CharacteristicReader,
    writer: CharacteristicWriter,
    max_frame_len: usize,
    peer: Address,
}

// Werbung und GATT-Dienst bleiben über Wiederverbindungen hinweg registriert.
struct ServerResources {
    control: Pin<Box<CharacteristicControl>>,
    _adv: AdvertisementHandle,
    _app: ApplicationHandle,
}

pub struct BluetoothTransport {
    mode: Mode,
    adapter: Adapter,
    _session: Session,
    link: Option<Link>,
    server: Option<ServerResources>,
    state: ConnectionState,
    connect_timeout: Duration,
    peer: Option<Address>,
}

impl BluetoothTransport {
    /// Server: wirbt unter `local_name` und wartet auf einen Client.
    pub async fn server(local_name: &str) -> Result<Self, TransportError> {
        Self::new(Mode::Server { local_name: local_name.to_string() }, None).await
    }

    /// Client: verbindet sich mit `peer`, oder sucht den Dienst, wenn noch kein Partner bekannt ist.
    pub async fn client(peer: Option<Address>) -> Result<Self, TransportError> {
        Self::new(Mode::Client, peer).await
    }

    async fn new(mode: Mode, peer: Option<Address>) -> Result<Self, TransportError> {
        let session = Session::new().await.map_err(backend)?;
        let adapter = session.default_adapter().await.map_err(backend)?;
        adapter.set_powered(true).await.map_err(backend)?;
        Ok(Self {
            mode,
            adapter,
            _session: session,
            link: None,
            server: None,
            state: ConnectionState::Disconnected,
            connect_timeout: DEFAULT_CONNECT_TIMEOUT,
            peer,
        })
    }

    pub fn with_connect_timeout(mut self, timeout: Duration) -> Self {
        self.connect_timeout = timeout;
        self
    }

    /// Adresse des Partners (beim Client nach dem ersten erfolgreichen Verbinden), zum Speichern.
    pub fn peer_address(&self) -> Option<Address> {
        self.peer
    }

    async fn establish(&mut self) -> Result<(Link, Option<Address>), TransportError> {
        if let Mode::Server { local_name } = &self.mode {
            let name = local_name.clone();
            return Ok((self.establish_server(&name).await?, None));
        }
        let (link, address) = self.establish_client().await?;
        Ok((link, Some(address)))
    }

    async fn start_server(&self, local_name: &str) -> Result<ServerResources, TransportError> {
        let advertisement = Advertisement {
            service_uuids: [SERVICE_UUID].into_iter().collect(),
            discoverable: Some(true),
            local_name: Some(local_name.to_string()),
            ..Default::default()
        };
        let adv_handle = self.adapter.advertise(advertisement).await.map_err(backend)?;

        let (control, control_handle) = characteristic_control();
        let app = Application {
            services: vec![Service {
                uuid: SERVICE_UUID,
                primary: true,
                characteristics: vec![Characteristic {
                    uuid: CHARACTERISTIC_UUID,
                    write: Some(CharacteristicWrite {
                        write_without_response: true,
                        encrypt_authenticated_write: true,
                        method: CharacteristicWriteMethod::Io,
                        ..Default::default()
                    }),
                    notify: Some(CharacteristicNotify {
                        notify: true,
                        method: CharacteristicNotifyMethod::Io,
                        ..Default::default()
                    }),
                    control_handle,
                    ..Default::default()
                }],
                ..Default::default()
            }],
            ..Default::default()
        };
        let app_handle = self.adapter.serve_gatt_application(app).await.map_err(backend)?;
        log::debug!("Werbung und GATT-Dienst registriert");
        Ok(ServerResources { control: Box::pin(control), _adv: adv_handle, _app: app_handle })
    }

    async fn establish_server(&mut self, local_name: &str) -> Result<Link, TransportError> {
        if self.server.is_none() {
            self.server = Some(self.start_server(local_name).await?);
        }
        let Some(server) = self.server.as_mut() else {
            return Err(TransportError::Closed);
        };

        log::debug!("warte auf Write- und Notify-Zugriff eines Clients");
        let mut reader = None;
        let mut writer = None;
        while reader.is_none() || writer.is_none() {
            match server.control.next().await {
                Some(CharacteristicControlEvent::Write(request)) => {
                    log::debug!("Write-Zugriff akzeptiert");
                    reader = Some(request.accept().map_err(backend)?);
                }
                Some(CharacteristicControlEvent::Notify(notifier)) => {
                    log::debug!("Notify-Zugriff akzeptiert");
                    writer = Some(notifier);
                }
                None => return Err(TransportError::Closed),
            }
        }
        let (Some(reader), Some(writer)) = (reader, writer) else {
            return Err(TransportError::Closed);
        };
        Ok(Link { max_frame_len: writer.mtu(), peer: reader.device_address(), reader, writer })
    }

    async fn discover(&self) -> Result<Device, TransportError> {
        let events = self.adapter.discover_devices().await.map_err(backend)?;
        pin_mut!(events);
        while let Some(event) = events.next().await {
            let AdapterEvent::DeviceAdded(address) = event else { continue };
            let device = self.adapter.device(address).map_err(backend)?;
            if device.uuids().await.map_err(backend)?.unwrap_or_default().contains(&SERVICE_UUID) {
                return Ok(device);
            }
        }
        Err(TransportError::Closed)
    }

    async fn establish_client(&self) -> Result<(Link, Address), TransportError> {
        let device = match self.peer {
            Some(address) => self.adapter.device(address).map_err(backend)?,
            None => self.discover().await?,
        };

        let mut attempt = 0;
        while !device.is_connected().await.map_err(backend)? {
            match device.connect().await {
                Ok(()) => break,
                Err(_) if attempt < CONNECT_RETRIES => {
                    attempt += 1;
                    sleep(Duration::from_secs(1)).await;
                }
                Err(e) => return Err(backend(e)),
            }
        }
        log::debug!("verbunden");

        // Kopplung vor dem ersten Schreiben, damit das erste Paket nicht verloren geht.
        if !device.is_paired().await.map_err(backend)? {
            log::debug!("koppele");
            device.pair().await.map_err(backend)?;
        }
        device.set_trusted(true).await.map_err(backend)?;

        log::debug!("suche GATT-Dienst");
        let characteristic = find_characteristic(&device).await?;
        log::debug!("fordere Write- und Notify-Kanal an");
        let writer = characteristic.write_io().await.map_err(backend)?;
        let reader = characteristic.notify_io().await.map_err(backend)?;
        log::debug!("Kanäle bereit");
        let address = device.address();
        Ok((Link { max_frame_len: writer.mtu(), reader, writer, peer: address }, address))
    }

    async fn establish_link(&mut self, during: ConnectionState) -> Result<(), TransportError> {
        self.drop_link().await;
        self.state = during;
        match timeout(self.connect_timeout, self.establish()).await {
            Ok(Ok((link, address))) => {
                if address.is_some() {
                    self.peer = address;
                }
                self.link = Some(link);
                self.state = ConnectionState::Connected;
                Ok(())
            }
            Ok(Err(e)) => {
                self.state = ConnectionState::Disconnected;
                Err(e)
            }
            Err(_) => {
                self.state = ConnectionState::Disconnected;
                Err(TransportError::Timeout)
            }
        }
    }

    async fn drop_link(&mut self) {
        if let Some(link) = self.link.take() {
            if let Ok(device) = self.adapter.device(link.peer) {
                let _ = device.disconnect().await;
            }
        }
        self.state = ConnectionState::Disconnected;
    }
}

async fn lookup_characteristic(device: &Device) -> Result<Option<remote::Characteristic>, TransportError> {
    for service in device.services().await.map_err(backend)? {
        if service.uuid().await.map_err(backend)? != SERVICE_UUID {
            continue;
        }
        for characteristic in service.characteristics().await.map_err(backend)? {
            if characteristic.uuid().await.map_err(backend)? == CHARACTERISTIC_UUID {
                return Ok(Some(characteristic));
            }
        }
    }
    Ok(None)
}

// BlueZ löst die Dienste nach dem Verbinden erst auf; bis dahin liefert `services()` einen Fehler.
async fn find_characteristic(device: &Device) -> Result<remote::Characteristic, TransportError> {
    let deadline = Instant::now() + SERVICES_TIMEOUT;
    loop {
        let last_error = match lookup_characteristic(device).await {
            Ok(Some(characteristic)) => return Ok(characteristic),
            Ok(None) => TransportError::Backend("GATT-Dienst nicht gefunden".to_string()),
            Err(e) => e,
        };
        if Instant::now() >= deadline {
            return Err(last_error);
        }
        sleep(Duration::from_millis(500)).await;
    }
}

impl Transport for BluetoothTransport {
    async fn connect(&mut self) -> Result<(), TransportError> {
        self.establish_link(ConnectionState::Connecting).await
    }

    async fn disconnect(&mut self) -> Result<(), TransportError> {
        self.drop_link().await;
        Ok(())
    }

    async fn send(&mut self, frame: &[u8]) -> Result<(), TransportError> {
        if self.state != ConnectionState::Connected {
            return Err(TransportError::NotConnected);
        }
        let Some(link) = &self.link else {
            return Err(TransportError::NotConnected);
        };
        if frame.len() > link.max_frame_len {
            return Err(TransportError::FrameTooLarge { max: link.max_frame_len });
        }
        if link.writer.send(frame).await.is_err() {
            self.drop_link().await;
            return Err(TransportError::Closed);
        }
        Ok(())
    }

    async fn receive(&mut self, buf: &mut [u8]) -> Result<usize, TransportError> {
        if self.state != ConnectionState::Connected {
            return Err(TransportError::NotConnected);
        }
        let Some(link) = &self.link else {
            return Err(TransportError::NotConnected);
        };
        let frame = match link.reader.recv().await {
            Ok(frame) if !frame.is_empty() => frame,
            _ => {
                self.drop_link().await;
                return Err(TransportError::Closed);
            }
        };
        if frame.len() > buf.len() {
            return Err(TransportError::BufferTooSmall { needed: frame.len() });
        }
        buf[..frame.len()].copy_from_slice(&frame);
        Ok(frame.len())
    }

    /// `max_frame_len` ist 0, solange keine Verbindung besteht.
    fn get_status(&self) -> TransportStatus {
        TransportStatus {
            state: self.state,
            max_frame_len: self.link.as_ref().map_or(0, |l| l.max_frame_len),
        }
    }

    async fn reconnect(&mut self) -> Result<(), TransportError> {
        self.establish_link(ConnectionState::Reconnecting).await
    }
}
