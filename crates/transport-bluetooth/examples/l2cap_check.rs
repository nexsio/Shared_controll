//! Prototyp: Wiederverbindung über einen direkten L2CAP-Kanal (LE) statt GATT.
//!
//! Gleicher Ablauf wie `bridge_check` (Pairing, Austausch, Trennen, Neuverbinden), aber mit einem
//! eigenen L2CAP-Transport. Es gibt keine GATT-Dienste und keine Benachrichtigungs-Einstellung.
//!
//! - `server`: wirbt, wartet auf Verbindungen (Pairing oder Wiederverbindung) bis zum Abbruch.
//! - `client`: Pairing, Austausch, Trennen und Neuverbinden im selben Prozess.
//! - `client-once`: Pairing und Austausch, dann Programmende (entspricht einem Neustart des Clients).
//!
//! Schlüssel und Vertrauen liegen nur im Speicher und gehen beim Beenden verloren.

use std::{error::Error, future::Future, time::Duration};

use bluer::{
    adv::{Advertisement, AdvertisementHandle},
    l2cap::{Security, SecurityLevel, SeqPacket, SeqPacketListener, Socket, SocketAddr},
    Adapter, AdapterEvent, Address, AddressType, Session,
};
use futures::{pin_mut, StreamExt};
use protocol::{Message, MouseMove};
use security::{
    accept, initiate_pairing, initiate_reconnect, Accepted, Identity, MemoryTrustStore, PendingPairing, SecureChannel,
    SecureSession,
};
use tokio::time::{sleep, timeout};
use transport::{ConnectionState, Transport, TransportError, TransportStatus};
use transport_bluetooth::SERVICE_UUID;

type AnyResult<T> = Result<T, Box<dyn Error>>;

/// Fester LE-PSM im dynamischen Bereich (ab 0x80), dafür sind keine Zusatzrechte nötig.
const PSM: u16 = 0x00C1;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(60);
const MESSAGES: i16 = 5;

fn backend(e: impl std::fmt::Display) -> TransportError {
    TransportError::Backend(e.to_string())
}

fn link_security() -> Security {
    Security { level: SecurityLevel::High, key_size: 0 }
}

/// Zustand aller BlueZ-Geräte als Text, nur zur Diagnose.
async fn device_states(adapter: &Adapter) -> String {
    let Ok(addresses) = adapter.device_addresses().await else {
        return "Geräteliste nicht lesbar".to_string();
    };
    let mut parts = Vec::new();
    for address in addresses {
        let Ok(device) = adapter.device(address) else { continue };
        let connected = device.is_connected().await.ok();
        let paired = device.is_paired().await.ok();
        let trusted = device.is_trusted().await.ok();
        parts.push(format!("{address}[verbunden={connected:?} gekoppelt={paired:?} vertraut={trusted:?}]"));
    }
    parts.join(" ")
}

/// Führt `fut` aus und protokolliert dabei jede Änderung des BlueZ-Gerätezustands.
async fn watched<F: Future>(adapter: &Adapter, label: &str, fut: F) -> F::Output {
    tokio::pin!(fut);
    let mut tick = tokio::time::interval(Duration::from_secs(1));
    let mut last = String::new();
    loop {
        tokio::select! {
            out = &mut fut => return out,
            _ = tick.tick() => {
                let now = device_states(adapter).await;
                if now != last {
                    log::debug!("{label}: BlueZ-Geräte {now}");
                    last = now;
                }
            }
        }
    }
}

#[derive(Clone, Copy)]
enum Role {
    Server,
    Client,
}

struct Link {
    socket: SeqPacket,
    peer: Address,
    rx: Vec<u8>,
    max_frame_len: usize,
}

impl Link {
    fn new(socket: SeqPacket, peer: Address) -> Self {
        let send_mtu = socket.send_mtu();
        let recv_mtu = socket.recv_mtu();
        let inner = AsRef::<Socket<SeqPacket>>::as_ref(&socket);
        log::debug!(
            "L2CAP-Kanal offen: lokal {:?}, Gegenstelle {:?}, MTU senden {send_mtu:?}, empfangen {recv_mtu:?}, Sicherheit {:?}",
            inner.local_addr(),
            socket.peer_addr(),
            inner.security()
        );
        let send_mtu = send_mtu.unwrap_or(512);
        let recv_mtu = recv_mtu.unwrap_or(2048).max(64);
        Self { socket, peer, rx: vec![0; recv_mtu], max_frame_len: send_mtu }
    }
}

struct L2capTransport {
    role: Role,
    adapter: Adapter,
    _session: Session,
    known_peer: Option<(Address, AddressType)>,
    listener: Option<SeqPacketListener>,
    _adv: Option<AdvertisementHandle>,
    link: Option<Link>,
    state: ConnectionState,
}

impl L2capTransport {
    async fn new(role: Role) -> Result<Self, TransportError> {
        let session = Session::new().await.map_err(backend)?;
        let adapter = session.default_adapter().await.map_err(backend)?;
        adapter.set_powered(true).await.map_err(backend)?;
        log::debug!("Adapter {} ({:?})", adapter.name(), adapter.address().await);
        Ok(Self {
            role,
            adapter,
            _session: session,
            known_peer: None,
            listener: None,
            _adv: None,
            link: None,
            state: ConnectionState::Disconnected,
        })
    }

    async fn accept_link(&mut self) -> Result<Link, TransportError> {
        if self.listener.is_none() {
            let advertisement = Advertisement {
                service_uuids: [SERVICE_UUID].into_iter().collect(),
                discoverable: Some(true),
                local_name: Some("SharedCtl".to_string()),
                ..Default::default()
            };
            self._adv = Some(self.adapter.advertise(advertisement).await.map_err(backend)?);

            let socket = Socket::<SeqPacket>::new_seq_packet().map_err(backend)?;
            socket.set_security(link_security()).map_err(backend)?;
            socket.bind(SocketAddr::new(Address::any(), AddressType::LePublic, PSM)).map_err(backend)?;
            self.listener = Some(socket.listen(1).map_err(backend)?);
            log::debug!("wirbt und lauscht auf PSM {PSM:#06x}");
        }
        let listener = self.listener.as_ref().ok_or(TransportError::Closed)?;
        log::debug!("warte auf eingehende Verbindung");
        let (socket, peer) = watched(&self.adapter, "Server wartet", listener.accept()).await.map_err(backend)?;
        log::debug!("Verbindung von {peer:?} angenommen");
        Ok(Link::new(socket, peer.addr))
    }

    async fn discover(&self) -> Result<(Address, AddressType), TransportError> {
        let events = self.adapter.discover_devices().await.map_err(backend)?;
        pin_mut!(events);
        while let Some(event) = events.next().await {
            let AdapterEvent::DeviceAdded(address) = event else { continue };
            let device = self.adapter.device(address).map_err(backend)?;
            if device.uuids().await.map_err(backend)?.unwrap_or_default().contains(&SERVICE_UUID) {
                log::debug!("Dienst gefunden bei {address}");
                return Ok((device.address(), device.address_type().await.map_err(backend)?));
            }
        }
        Err(TransportError::Closed)
    }

    async fn connect_link(&mut self) -> Result<Link, TransportError> {
        let (address, address_type) = match self.known_peer {
            Some(peer) => peer,
            None => self.discover().await?,
        };
        self.known_peer = Some((address, address_type));
        log::debug!("verbinde mit {address} ({address_type:?}), PSM {PSM:#06x}");

        let socket = Socket::<SeqPacket>::new_seq_packet().map_err(backend)?;
        socket.set_security(link_security()).map_err(backend)?;
        socket.bind(SocketAddr::any_le()).map_err(backend)?;
        log::debug!("Socket vorbereitet, lokal {:?}", socket.local_addr());
        let target = SocketAddr::new(address, address_type, PSM);
        let socket = watched(&self.adapter, "Client verbindet", socket.connect(target)).await.map_err(|e| {
            log::warn!("connect fehlgeschlagen: {e} ({:?})", e.kind());
            backend(e)
        })?;
        Ok(Link::new(socket, address))
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
            log::debug!("schließe Kanal zu {peer}");
            drop(link);
            if let Ok(device) = self.adapter.device(peer) {
                let result = device.disconnect().await;
                log::debug!("BlueZ-Trennen von {peer}: {result:?}");
            }
        }
        self.state = ConnectionState::Disconnected;
    }
}

impl Transport for L2capTransport {
    async fn connect(&mut self) -> Result<(), TransportError> {
        self.establish(ConnectionState::Connecting).await
    }

    async fn disconnect(&mut self) -> Result<(), TransportError> {
        self.drop_link().await;
        Ok(())
    }

    async fn send(&mut self, frame: &[u8]) -> Result<(), TransportError> {
        let Some(link) = &self.link else {
            return Err(TransportError::NotConnected);
        };
        if frame.len() > link.max_frame_len {
            return Err(TransportError::FrameTooLarge { max: link.max_frame_len });
        }
        match link.socket.send(frame).await {
            Ok(n) => log::debug!("gesendet: {n} von {} Bytes", frame.len()),
            Err(e) => {
                log::warn!("Senden fehlgeschlagen: {e} ({:?})", e.kind());
                self.drop_link().await;
                return Err(TransportError::Closed);
            }
        }
        Ok(())
    }

    async fn receive(&mut self, buf: &mut [u8]) -> Result<usize, TransportError> {
        let Some(link) = &mut self.link else {
            return Err(TransportError::NotConnected);
        };
        let received = watched(&self.adapter, "Empfang", link.socket.recv(&mut link.rx)).await;
        let n = match received {
            Ok(0) => {
                log::warn!("Gegenstelle hat den Kanal geschlossen (0 Bytes empfangen)");
                self.drop_link().await;
                return Err(TransportError::Closed);
            }
            Ok(n) => {
                log::debug!("empfangen: {n} Bytes");
                n
            }
            Err(e) => {
                log::warn!("Empfangen fehlgeschlagen: {e} ({:?})", e.kind());
                self.drop_link().await;
                return Err(TransportError::Closed);
            }
        };
        if n > buf.len() {
            return Err(TransportError::BufferTooSmall { needed: n });
        }
        buf[..n].copy_from_slice(&link.rx[..n]);
        Ok(n)
    }

    fn get_status(&self) -> TransportStatus {
        TransportStatus { state: self.state, max_frame_len: self.link.as_ref().map_or(0, |l| l.max_frame_len) }
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

async fn run_server() -> AnyResult<()> {
    let identity = Identity::generate()?;
    let mut store = MemoryTrustStore::default();
    let mut transport = L2capTransport::new(Role::Server).await?;

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
        let session = match accept(&mut transport, &identity, &store).await? {
            Accepted::Pairing(pairing) => confirm_code(pairing, &mut store).await?,
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

async fn run_client() -> AnyResult<()> {
    let identity = Identity::generate()?;
    let mut store = MemoryTrustStore::default();
    let mut transport = L2capTransport::new(Role::Client).await?;

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

async fn run_client_once() -> AnyResult<()> {
    let identity = Identity::generate()?;
    let mut store = MemoryTrustStore::default();
    let mut transport = L2capTransport::new(Role::Client).await?;

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
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info,l2cap_check=debug")).init();
    let result = match std::env::args().nth(1).as_deref() {
        Some("server") => run_server().await,
        Some("client") => run_client().await,
        Some("client-once") => run_client_once().await,
        _ => {
            eprintln!("Aufruf: l2cap_check <server|client|client-once>");
            Ok(())
        }
    };
    if let Err(e) = &result {
        log::error!("Fehler: {e:?}");
    }
    result
}
