//! Hardwaretest: Pairing, verschlüsselter Nachrichtenaustausch und Wiederverbindung über echtes BLE.
//!
//! Auf dem einen Rechner `bridge_check server`, auf dem anderen `bridge_check client` starten.
//!
//! - `client`: Pairing, Austausch, dann Trennen und Neuverbinden im selben Prozess.
//! - `client-once`: Pairing und Austausch, dann Programmende. Zweimal gestartet entspricht das einem
//!   Neustart des Clients (neue Schlüssel, daher neues Pairing, aber gespeicherte Bluetooth-Kopplung).
//! - `server` bedient nacheinander beliebig viele Verbindungen (Pairing oder Wiederverbindung) bis zum Abbruch.
//!
//! Schlüssel und Vertrauen liegen nur im Speicher und gehen beim Beenden verloren.

use std::{error::Error, time::Duration};

use protocol::{Message, MouseMove};
use security::{
    accept, initiate_pairing, initiate_reconnect, Accepted, Identity, MemoryTrustStore, PendingPairing, SecureChannel,
    SecureSession,
};
use tokio::time::{sleep, timeout};
use transport::Transport;
use transport_bluetooth::BluetoothTransport;

type AnyResult<T> = Result<T, Box<dyn Error>>;

const MESSAGES: i16 = 5;

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

async fn exchange(channel: &mut SecureChannel<BluetoothTransport>) -> AnyResult<()> {
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
    let mut transport = BluetoothTransport::server("SharedCtl").await?;

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

async fn run_client_once() -> AnyResult<()> {
    let identity = Identity::generate()?;
    let mut store = MemoryTrustStore::default();
    let mut transport = BluetoothTransport::client(None).await?;

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

async fn run_client() -> AnyResult<()> {
    let identity = Identity::generate()?;
    let mut store = MemoryTrustStore::default();
    let mut transport = BluetoothTransport::client(None).await?;

    log::info!("Suche Server ...");
    transport.connect().await?;
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

#[tokio::main]
async fn main() -> AnyResult<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info,transport_bluetooth=debug")).init();
    let result = match std::env::args().nth(1).as_deref() {
        Some("server") => run_server().await,
        Some("client") => run_client().await,
        Some("client-once") => run_client_once().await,
        _ => {
            eprintln!("Aufruf: bridge_check <server|client|client-once>");
            Ok(())
        }
    };
    if let Err(e) = &result {
        log::error!("Fehler: {e:?}");
    }
    result
}
