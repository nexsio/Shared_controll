//! Verbindungsaufbau: Dongle öffnen, auf Funkverbindung warten, Noise-Handshake mit Pairing-Bestätigung.

use std::time::Duration;

use hidapi::HidApi;
use security::{accept, initiate_pairing, initiate_reconnect, Accepted, Identity, SecureChannel, SecureSession, SecurityError};
use tokio::{
    io::{AsyncBufReadExt, BufReader},
    sync::watch,
    time::{sleep, timeout},
};
use transport::Transport;
use transport_dongle::{find_paths, UsbDongleTransport};

use crate::{
    config::{partner_label, FileTrustStore},
    dialog,
    session::Ended,
    tray::{LinkStatus, UiState},
    Options, Role,
};

const CONFIRM_TIMEOUT: Duration = Duration::from_secs(120);

pub fn open_dongle(api: &HidApi, index: usize) -> Result<UsbDongleTransport, Ended> {
    let mut paths = find_paths(api);
    paths.sort();
    let path = paths.get(index).ok_or(Ended::NoDongle)?;
    UsbDongleTransport::open(api, path).map_err(|e| Ended::Link(e.to_string()))
}

async fn confirm_in_terminal(code: &str) -> bool {
    println!("Pairing code: {code}");
    println!("Does the code match on both computers? [y/N]");
    let mut line = String::new();
    let mut stdin = BufReader::new(tokio::io::stdin());
    let answered = matches!(timeout(CONFIRM_TIMEOUT, stdin.read_line(&mut line)).await, Ok(Ok(_)));
    answered && matches!(line.trim().to_lowercase().as_str(), "y" | "yes" | "j" | "ja")
}

async fn confirm_code(code: &str) -> bool {
    let text = format!("Pairing code: {code}\n\nDoes the other computer show the same code?");
    match timeout(CONFIRM_TIMEOUT, dialog::yes_no(&text)).await {
        Ok(Some(answer)) => answer,
        Ok(None) => confirm_in_terminal(code).await,
        Err(_) => false,
    }
}

/// `Ok(None)`: Pairing wurde vom Benutzer abgelehnt.
async fn handshake_once(
    transport: &mut UsbDongleTransport,
    role: Role,
    force_pair: bool,
    identity: &Identity,
    trust: &mut FileTrustStore,
) -> Result<Option<SecureSession>, SecurityError> {
    if role == Role::Master {
        match trust.latest().filter(|_| !force_pair) {
            Some(remote) => initiate_reconnect(transport, identity, &remote).await.map(Some),
            None => {
                let pending = initiate_pairing(transport, identity).await?;
                Ok(confirm_code(&pending.code_text()).await.then(|| pending.confirm(trust)))
            }
        }
    } else {
        match accept(transport, identity, &*trust).await? {
            Accepted::Session(session) => Ok(Some(session)),
            Accepted::Pairing(pending) => {
                Ok(confirm_code(&pending.code_text()).await.then(|| pending.confirm(trust)))
            }
        }
    }
}

pub async fn establish(
    mut transport: UsbDongleTransport,
    opts: &Options,
    identity: &Identity,
    trust: &mut FileTrustStore,
    force_pair: bool,
    state: &watch::Sender<UiState>,
) -> Result<SecureChannel<UsbDongleTransport>, Ended> {
    transport.connect().await.map_err(|e| Ended::Link(format!("no radio link ({e})")))?;
    state.send_modify(|s| s.status = LinkStatus::Radio);
    let session = loop {
        match handshake_once(&mut transport, opts.role, force_pair, identity, trust).await {
            Ok(Some(session)) => break session,
            Ok(None) => return Err(Ended::Fatal("Pairing rejected".into())),
            Err(SecurityError::Transport(e)) => return Err(Ended::Link(format!("radio link lost ({e})"))),
            Err(e) => {
                eprintln!("Handshake failed ({e}), retrying");
                sleep(Duration::from_secs(1)).await;
            }
        }
    };
    let partner = partner_label(session.remote_public());
    let mut channel = SecureChannel::new(transport, session);
    channel.transport_mut().set_authenticated(true).await.map_err(|e| Ended::Link(e.to_string()))?;
    state.send_modify(|s| {
        s.status = LinkStatus::Authenticated;
        s.partner = Some(partner);
    });
    Ok(channel)
}
