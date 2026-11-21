//! Prototyp: Bluetooth Classic (RFCOMM) statt LE-L2CAP oder GATT.
//!
//! Gleicher Ablauf wie `bridge_check` (Pairing, Austausch, Trennen, Neuverbinden). Die Verschlüsselung
//! liegt in der Security-Schicht (Noise), nicht in BlueZ.
//!
//! - `server`: registriert das RFCOMM-Profil und nimmt Verbindungen bis zum Abbruch an.
//! - `client`: Pairing, Austausch, Trennen und Neuverbinden im selben Prozess.
//! - `client-once`: Pairing und Austausch, dann Programmende.
//!
//! Optionales zweites Argument für Clients: Bluetooth-Adresse des Servers. Ohne Argument wird per
//! Dienst-UUID gesucht.
//!
//! Schlüssel und Vertrauen liegen nur im Speicher und gehen beim Beenden verloren.

use std::{error::Error, time::Duration};

use bluer::{
    rfcomm::{Profile, ProfileHandle, Socket, SocketAddr, Stream},
    Adapter, Address, Session, Uuid,
};
use futures::StreamExt;
use protocol::{Message, MouseMove};
use security::{
    accept, initiate_pairing, initiate_reconnect, Accepted, Identity, MemoryTrustStore, PendingPairing, SecureChannel,
    SecureSession,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    time::{sleep, timeout, Instant},
};
use transport::{ConnectionState, Transport, TransportError, TransportStatus};

type AnyResult<T> = Result<T, Box<dyn Error>>;

const RFCOMM_UUID: Uuid = Uuid::from_u128(0x6e1d_3a52_0b7c_4f9e_8a13_2c5d_9f04_b7e1);
const CHANNEL: u8 = 11;
const MAX_FRAME_LEN: usize = 1024;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(60);
const DISCOVERY_TIMEOUT: Duration = Duration::from_secs(60);
const MESSAGES: i16 = 5;

fn backend(e: impl std::fmt::Display) -> TransportError {
    TransportError::Backend(e.to_string())
}

#[derive(Clone, Copy)]
enum Role {
    Server,
    Client,
}

struct Link {
    stream: Stream,
    peer: Address,
    rx: Vec<u8>,
}

struct RfcommTransport {
    role: Role,
    adapter: Adapter,
    _session: Session,
    known_peer: Option<Address>,
    profile: Option<ProfileHandle>,
    link: Option<Link>,
    state: ConnectionState,
}

impl RfcommTransport {
    async fn new(role: Role, known_peer: Option<Address>) -> Result<Self, TransportError> {
        let session = Session::new().await.map_err(backend)?;
        let adapter = session.default_adapter().await.map_err(backend)?;
        adapter.set_powered(true).await.map_err(backend)?;
        let profile = match role {
            Role::Server => {
                adapter.set_discoverable_timeout(0).await.map_err(backend)?;
                adapter.set_discoverable(true).await.map_err(backend)?;
                let profile = Profile {
                    uuid: RFCOMM_UUID,
                    name: Some("SharedCtl".to_string()),
                    channel: Some(u16::from(CHANNEL)),
                    require_authentication: Some(false),
                    require_authorization: Some(false),
                    ..Default::default()
                };
                Some(session.register_profile(profile).await.map_err(backend)?)
            }
            Role::Client => None,
        };
        log::debug!("Adapter {} ({:?})", adapter.name(), adapter.address().await);
        Ok(Self {
            role,
            adapter,
            _session: session,
            known_peer,
            profile,
            link: None,
            state: ConnectionState::Disconnected,
        })
    }

    async fn accept_link(&mut self) -> Result<Link, TransportError> {
        let profile = self.profile.as_mut().ok_or(TransportError::Closed)?;
        log::debug!("warte auf Verbindung auf Kanal {CHANNEL}");
        let request = profile.next().await.ok_or(TransportError::Closed)?;
        let peer = request.device();
        log::debug!("Verbindungsanfrage von {peer}");
        let stream = request.accept().map_err(backend)?;
        Ok(Link { stream, peer, rx: vec![0; MAX_FRAME_LEN] })
    }

    async fn discover(&self) -> Result<Address, TransportError> {
        let _discovery = self.adapter.discover_devices().await.map_err(backend)?;
        let deadline = Instant::now() + DISCOVERY_TIMEOUT;
        loop {
            for address in self.adapter.device_addresses().await.map_err(backend)? {
                let device = self.adapter.device(address).map_err(backend)?;
                if device.uuids().await.map_err(backend)?.unwrap_or_default().contains(&RFCOMM_UUID) {
                    log::debug!("Dienst gefunden bei {address}");
                    return Ok(address);
                }
            }
            if Instant::now() >= deadline {
                return Err(TransportError::Timeout);
            }
            sleep(Duration::from_secs(2)).await;
        }
    }

    async fn connect_link(&mut self) -> Result<Link, TransportError> {
        let peer = match self.known_peer {
            Some(peer) => peer,
            None => self.discover().await?,
        };
        self.known_peer = Some(peer);
        log::debug!("verbinde mit {peer}, Kanal {CHANNEL}");
        let socket = Socket::new().map_err(backend)?;
        let stream = socket.connect(SocketAddr::new(peer, CHANNEL)).await.map_err(|e| {
            log::warn!("connect fehlgeschlagen: {e} ({:?})", e.kind());
            backend(e)
        })?;
        Ok(Link { stream, peer, rx: vec![0; MAX_FRAME_LEN] })
    }

    async fn establish(&mut self, during: ConnectionState) -> Result<(), TransportError> {
        self.drop_link().await;
        self.state = during;
        let role = self.role;
        let result = timeout(CONNECT_TIMEOUT, async {
            match role {
                Role::Server => self.accept_link().await,
                Role::Client => self.connect_link().await,
            }
        })
        .await;
        match result {
            Ok(Ok(link)) => {
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
            let peer = link.peer;
            drop(link);
            log::debug!("Kanal zu {peer} geschlossen");
            if let Ok(device) = self.adapter.device(peer) {
                let result = device.disconnect().await;
                log::debug!("BlueZ-Trennen von {peer}: {result:?}");
            }
        }
        self.state = ConnectionState::Disconnected;
    }
}

impl Transport for RfcommTransport {
    async fn connect(&mut self) -> Result<(), TransportError> {
        self.establish(ConnectionState::Connecting).await
    }

    async fn disconnect(&mut self) -> Result<(), TransportError> {
        self.drop_link().await;
        Ok(())
    }

    async fn send(&mut self, frame: &[u8]) -> Result<(), TransportError> {
        if frame.len() > MAX_FRAME_LEN {
            return Err(TransportError::FrameTooLarge { max: MAX_FRAME_LEN });
        }
        let Some(link) = &mut self.link else {
            return Err(TransportError::NotConnected);
        };
        let len = u16::try_from(frame.len()).map_err(|_| TransportError::FrameTooLarge { max: MAX_FRAME_LEN })?;
        let mut wire = Vec::with_capacity(frame.len() + 2);
        wire.extend_from_slice(&len.to_be_bytes());
        wire.extend_from_slice(frame);
        match link.stream.write_all(&wire).await {
            Ok(()) => {
                log::debug!("gesendet: {} Bytes", frame.len());
                Ok(())
            }
            Err(e) => {
                log::warn!("Senden fehlgeschlagen: {e} ({:?})", e.kind());
                self.drop_link().await;
                Err(TransportError::Closed)
            }
        }
    }

    async fn receive(&mut self, buf: &mut [u8]) -> Result<usize, TransportError> {
        let Some(link) = &mut self.link else {
            return Err(TransportError::NotConnected);
        };
        let read = async {
            let mut header = [0u8; 2];
            link.stream.read_exact(&mut header).await?;
            let len = usize::from(u16::from_be_bytes(header));
            if len > MAX_FRAME_LEN {
                return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "Frame zu groß"));
            }
            link.stream.read_exact(&mut link.rx[..len]).await?;
            Ok(len)
        };
        let n = match read.await {
            Ok(0) => {
                log::warn!("Gegenstelle hat den Kanal geschlossen");
                self.drop_link().await;
                return Err(TransportError::Closed);
            }
            Ok(n) => n,
            Err(e) => {
                log::warn!("Empfangen fehlgeschlagen: {e} ({:?})", e.kind());
                self.drop_link().await;
                return Err(TransportError::Closed);
            }
        };
        let link = self.link.as_ref().ok_or(TransportError::NotConnected)?;
        if n > buf.len() {
            return Err(TransportError::BufferTooSmall { needed: n });
        }
        buf[..n].copy_from_slice(&link.rx[..n]);
        log::debug!("empfangen: {n} Bytes");
        Ok(n)
    }

    fn get_status(&self) -> TransportStatus {
        let max_frame_len = if self.link.is_some() { MAX_FRAME_LEN } else { 0 };
        TransportStatus { state: self.state, max_frame_len }
    }

    async fn reconnect(&mut self) -> Result<(), TransportError> {
        self.establish(ConnectionState::Reconnecting).await
    }
}

async fn confirm_code(pairing: PendingPairing, store: &mut MemoryTrustStore) -> AnyResult<SecureSession> {
    println!("\nVergleichscode: {}", pairing.code_text());
    println!("Zeigt der andere Rechner denselben Code? [j/N]");
    let answer = tokio::task::spawn_blocking(|| {
        let mut line = String::new();
        std::io::stdin().read_line(&mut line).map(|_| line)
    })
    .await??;
    if !answer.trim().eq_ignore_ascii_case("j") {
        return Err("Pairing vom Benutzer abgelehnt".into());
    }
    Ok(pairing.confirm(store))
}

async fn exchange<T: Transport>(channel: &mut SecureChannel<T>) -> AnyResult<()> {
    for i in 0..MESSAGES {
        channel.send(&Message::MouseMove(MouseMove { dx: i, dy: -i })).await?;
    }
    for i in 0..MESSAGES {
        let message = timeout(Duration::from_secs(10), channel.receive()).await??;
        if message != Message::MouseMove(MouseMove { dx: i, dy: -i }) {
            return Err(format!("unerwartete Nachricht an Position {i}").into());
        }
    }
    log::info!("{MESSAGES} Nachrichten gesendet und {MESSAGES} korrekt empfangen.");
    Ok(())
}

async fn run_server(peer: Option<Address>) -> AnyResult<()> {
    let identity = Identity::generate()?;
    let mut store = MemoryTrustStore::default();
    let transport = RfcommTransport::new(Role::Server, None).await?;
    let adapter = transport.adapter.clone();

    if let Some(peer) = peer {
        adapter.device(peer)?.set_trusted(true).await.map_err(backend)?;
        log::info!("{peer} ist vertraut, nur bis Programmende.");
    }
    let result = tokio::select! {
        r = serve(transport, &identity, &mut store) => r,
        _ = tokio::signal::ctrl_c() => Ok(()),
    };
    if let Some(peer) = peer {
        adapter.device(peer)?.set_trusted(false).await.map_err(backend)?;
        log::info!("Vertrauen zu {peer} entzogen.");
    }
    result
}

async fn serve(mut transport: RfcommTransport, identity: &Identity, store: &mut MemoryTrustStore) -> AnyResult<()> {
    let mut first = true;
    loop {
        log::info!("Warte auf Client ...");
        if first {
            transport.connect().await?;
            first = false;
        } else {
            transport.reconnect().await?;
        }
        log::info!("Verbunden, führe Handshake aus ...");
        let session = match accept(&mut transport, identity, &*store).await? {
            Accepted::Pairing(pairing) => confirm_code(pairing, store).await?,
            Accepted::Session(session) => session,
        };
        let mut channel = SecureChannel::new(transport, session);
        exchange(&mut channel).await?;
        log::info!("Durchlauf in Ordnung.");

        transport = channel.into_transport();
        transport.disconnect().await?;
        log::info!("Getrennt.");
    }
}

async fn run_client(peer: Option<Address>) -> AnyResult<()> {
    let identity = Identity::generate()?;
    let mut store = MemoryTrustStore::default();
    let mut transport = RfcommTransport::new(Role::Client, peer).await?;

    log::info!("Suche Server ...");
    transport.connect().await?;
    log::info!("Verbunden, führe Handshake aus ...");
    let pairing = initiate_pairing(&mut transport, &identity).await?;
    let session = confirm_code(pairing, &mut store).await?;
    let remote = *session.remote_public();
    let mut channel = SecureChannel::new(transport, session);
    exchange(&mut channel).await?;

    log::info!("Trenne und verbinde neu ...");
    let mut transport = channel.into_transport();
    transport.disconnect().await?;
    log::info!("Getrennt. Warte 3 s ...");
    sleep(Duration::from_secs(3)).await;
    log::info!("Verbinde erneut ...");
    transport.reconnect().await?;
    log::info!("Verbunden, führe Handshake aus ...");
    let session = initiate_reconnect(&mut transport, &identity, &remote).await?;
    let mut channel = SecureChannel::new(transport, session);
    exchange(&mut channel).await?;
    log::info!("Alles in Ordnung.");
    Ok(())
}

async fn run_client_once(peer: Option<Address>) -> AnyResult<()> {
    let identity = Identity::generate()?;
    let mut store = MemoryTrustStore::default();
    let mut transport = RfcommTransport::new(Role::Client, peer).await?;

    log::info!("Suche Server ...");
    transport.connect().await?;
    log::info!("Verbunden, führe Handshake aus ...");
    let pairing = initiate_pairing(&mut transport, &identity).await?;
    let session = confirm_code(pairing, &mut store).await?;
    let mut channel = SecureChannel::new(transport, session);
    exchange(&mut channel).await?;
    log::info!("Durchlauf in Ordnung, beende Programm.");
    Ok(())
}

#[tokio::main]
async fn main() -> AnyResult<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info,rfcomm_check=debug")).init();
    let mode = std::env::args().nth(1);
    let peer = match std::env::args().nth(2) {
        Some(text) => Some(text.parse::<Address>().map_err(|e| format!("ungültige Adresse {text}: {e}"))?),
        None => None,
    };
    let result = match mode.as_deref() {
        Some("server") => run_server(peer).await,
        Some("client") => run_client(peer).await,
        Some("client-once") => run_client_once(peer).await,
        _ => {
            eprintln!("Aufruf: rfcomm_check <server|client|client-once> [Bluetooth-Adresse]");
            Ok(())
        }
    };
    if let Err(e) = &result {
        log::error!("Fehler: {e:?}");
    }
    result
}
