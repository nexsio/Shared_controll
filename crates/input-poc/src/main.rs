//! Phase-0-Prototyp: Eingaben über das RemoteDesktop-Portal und libei (reis) einspeisen.
//!
//! Zeigt den Berechtigungsdialog, bewegt die Maus 100 px nach rechts und zurück
//! und tippt kurz die Shift-Taste (erzeugt keinen Text).

use std::{
    collections::HashMap,
    fs,
    io::Write,
    os::unix::{fs::OpenOptionsExt, net::UnixStream},
    path::PathBuf,
    time::{Duration, Instant},
};

use ashpd::desktop::{
    remote_desktop::{ConnectToEISOptions, DeviceType, RemoteDesktop, SelectDevicesOptions, StartOptions},
    CreateSessionOptions, PersistMode,
};
use futures_util::StreamExt;
use reis::{ei, tokio::EiEventStream, PendingRequestResult};
use tokio::time::{sleep, timeout};

type AnyResult<T> = Result<T, Box<dyn std::error::Error>>;

const KEY_LEFTSHIFT: u32 = 42;

fn token_path() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
    Some(base.join("shared-control").join("input-poc-token"))
}

fn save_token(token: &str) -> AnyResult<()> {
    let path = token_path().ok_or("Kein Konfigurationsverzeichnis gefunden")?;
    fs::create_dir_all(path.parent().ok_or("Ungültiger Pfad")?)?;
    let mut file = fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(path)?;
    file.write_all(token.as_bytes())?;
    Ok(())
}

struct Dev {
    device: ei::Device,
    pointer: Option<ei::Pointer>,
    keyboard: Option<ei::Keyboard>,
    resumed: bool,
}

#[derive(Default)]
struct Seat {
    capabilities: HashMap<String, u64>,
}

struct State {
    context: ei::Context,
    seats: HashMap<ei::Seat, Seat>,
    devices: HashMap<ei::Device, Dev>,
    last_serial: u32,
}

impl State {
    fn handle(&mut self, event: ei::Event) {
        match event {
            ei::Event::Connection(_, ei::connection::Event::Seat { seat }) => {
                self.seats.insert(seat, Seat::default());
            }
            ei::Event::Connection(_, ei::connection::Event::Ping { ping }) => ping.done(0),
            ei::Event::Seat(seat, request) => match request {
                ei::seat::Event::Capability { mask, interface } => {
                    if let Some(s) = self.seats.get_mut(&seat) {
                        s.capabilities.insert(interface, mask);
                    }
                }
                ei::seat::Event::Done => {
                    if let Some(s) = self.seats.get(&seat) {
                        seat.bind(s.capabilities.values().fold(0, |a, b| a | b));
                    }
                }
                ei::seat::Event::Device { device } => {
                    self.devices.insert(
                        device.clone(),
                        Dev { device, pointer: None, keyboard: None, resumed: false },
                    );
                }
                _ => {}
            },
            ei::Event::Device(device, request) => {
                let Some(dev) = self.devices.get_mut(&device) else { return };
                match request {
                    ei::device::Event::Interface { object } => {
                        if let Some(p) = object.clone().downcast::<ei::Pointer>() {
                            dev.pointer = Some(p);
                        } else if let Some(k) = object.downcast::<ei::Keyboard>() {
                            dev.keyboard = Some(k);
                        }
                    }
                    ei::device::Event::Resumed { serial } => {
                        self.last_serial = serial;
                        dev.resumed = true;
                    }
                    _ => {}
                }
            }
            _ => {}
        }
        let _ = self.context.flush();
    }

    fn ready(&self) -> bool {
        let has = |f: fn(&Dev) -> bool| self.devices.values().any(|d| d.resumed && f(d));
        has(|d| d.pointer.is_some()) && has(|d| d.keyboard.is_some())
    }

    async fn emulate(&self) -> AnyResult<()> {
        let t0 = Instant::now();
        let ts = || t0.elapsed().as_micros() as u64;
        let serial = self.last_serial;
        let mut seq = 0u32;

        for dev in self.devices.values().filter(|d| d.resumed) {
            if let Some(pointer) = &dev.pointer {
                println!("Maus: 100 px nach rechts und zurück");
                dev.device.start_emulating(seq, serial);
                seq += 1;
                for dx in std::iter::repeat(5.0_f32).take(20).chain(std::iter::repeat(-5.0_f32).take(20)) {
                    pointer.motion_relative(dx, 0.0);
                    dev.device.frame(serial, ts());
                    self.context.flush()?;
                    sleep(Duration::from_millis(20)).await;
                }
                dev.device.stop_emulating(serial);
                self.context.flush()?;
            }
            if let Some(keyboard) = &dev.keyboard {
                println!("Tastatur: Shift antippen");
                dev.device.start_emulating(seq, serial);
                seq += 1;
                keyboard.key(KEY_LEFTSHIFT, ei::keyboard::KeyState::Press);
                dev.device.frame(serial, ts());
                self.context.flush()?;
                sleep(Duration::from_millis(50)).await;
                keyboard.key(KEY_LEFTSHIFT, ei::keyboard::KeyState::Released);
                dev.device.frame(serial, ts());
                dev.device.stop_emulating(serial);
                self.context.flush()?;
            }
        }
        Ok(())
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> AnyResult<()> {
    println!("Fordere Zugriff über das RemoteDesktop-Portal an. Bitte den KDE-Dialog bestätigen ...");
    let remote_desktop = RemoteDesktop::new().await?;
    let session = remote_desktop.create_session(CreateSessionOptions::default()).await?;
    let stored = token_path().and_then(|p| fs::read_to_string(p).ok());
    println!("Gespeicherter Token vorhanden: {}", stored.is_some());
    let options = SelectDevicesOptions::default()
        .set_devices(DeviceType::Keyboard | DeviceType::Pointer)
        .set_restore_token(stored.as_deref())
        .set_persist_mode(PersistMode::ExplicitlyRevoked);
    remote_desktop.select_devices(&session, options).await?;

    let selected = remote_desktop.start(&session, None, StartOptions::default()).await?.response()?;
    println!(
        "Freigabe erteilt. Restore-Token für dauerhafte Freigabe erhalten: {}",
        selected.restore_token().is_some()
    );
    if let Some(token) = selected.restore_token() {
        save_token(token)?;
    }

    let fd = remote_desktop.connect_to_eis(&session, ConnectToEISOptions::default()).await?;
    let context = ei::Context::new(UnixStream::from(fd))?;
    let mut events = EiEventStream::new(context.clone())?;
    let handshake = reis::tokio::ei_handshake(
        &mut events,
        "shared-control-input-poc",
        ei::handshake::ContextType::Sender,
    )
    .await?;

    let mut state = State {
        context: context.clone(),
        seats: HashMap::new(),
        devices: HashMap::new(),
        last_serial: handshake.serial,
    };

    let wait = async {
        while let Some(result) = events.next().await {
            if let PendingRequestResult::Request(event) = result? {
                state.handle(event);
                if state.ready() {
                    return Ok::<_, reis::Error>(true);
                }
            }
        }
        Ok(false)
    };
    match timeout(Duration::from_secs(10), wait).await {
        Ok(Ok(true)) => state.emulate().await?,
        Ok(Ok(false)) => println!("Verbindung vom Compositor beendet, bevor Geräte bereit waren."),
        Ok(Err(e)) => println!("Fehler im EI-Protokoll: {e}"),
        Err(_) => {
            println!("Zeitüberschreitung. Gefundene Geräte:");
            for d in state.devices.values() {
                println!(
                    "  Zeiger: {}, Tastatur: {}, aktiv: {}",
                    d.pointer.is_some(),
                    d.keyboard.is_some(),
                    d.resumed
                );
            }
        }
    }

    sleep(Duration::from_millis(300)).await;
    println!("Fertig.");
    Ok(())
}
