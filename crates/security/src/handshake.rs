use std::{fmt, time::Duration};

use protocol::{decode, encode_into, MessageKind, HEADER_LEN, MAX_PAYLOAD_LEN, PROTOCOL_VERSION};
use snow::HandshakeState;
use tokio::time::timeout;
use transport::Transport;

use crate::{Identity, SecureSession, SecurityError, TrustStore, KEY_LEN};

const PATTERN_PAIRING: &str = "Noise_XX_25519_ChaChaPoly_BLAKE2s";
const PATTERN_RECONNECT: &str = "Noise_IK_25519_ChaChaPoly_BLAKE2s";
const MODE_PAIR: u8 = 1;
const MODE_RECONNECT: u8 = 2;
const STEP_TIMEOUT: Duration = Duration::from_secs(10);
const FRAME_BUF: usize = HEADER_LEN + MAX_PAYLOAD_LEN;

// Bindet die Protokollversion an den Handshake; abweichende Versionen schlagen dort fehl.
const _: () = assert!(PROTOCOL_VERSION == 1);
static PROLOGUE: &[u8; 16] = b"shared-control/\x01";

/// Gekoppelte Sitzung, die erst nach Bestätigung des Codes durch den Benutzer nutzbar ist.
///
/// Wird sie verworfen (`drop`), ist der Partner abgelehnt und nichts wird gespeichert.
pub struct PendingPairing {
    session: SecureSession,
    code: u32,
}

impl fmt::Debug for PendingPairing {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PendingPairing").finish_non_exhaustive()
    }
}

impl PendingPairing {
    /// Sechsstelliger Vergleichscode (0..=999999), auf beiden Geräten identisch anzuzeigen.
    pub fn code(&self) -> u32 {
        self.code
    }

    pub fn code_text(&self) -> String {
        format!("{:06}", self.code)
    }

    pub fn remote_public(&self) -> &[u8; KEY_LEN] {
        self.session.remote_public()
    }

    /// Benutzer hat bestätigt, dass die Codes übereinstimmen: Partner wird dauerhaft vertraut.
    pub fn confirm(self, store: &mut impl TrustStore) -> SecureSession {
        store.trust(*self.session.remote_public());
        self.session
    }
}

#[derive(Debug)]
pub enum Accepted {
    /// Neuer Partner: Code muss bestätigt werden.
    Pairing(PendingPairing),
    /// Bereits vertrauter Partner hat sich erfolgreich wieder verbunden.
    Session(SecureSession),
}

fn builder<'a>(pattern: &str, local: &'a Identity) -> Result<snow::Builder<'a>, SecurityError> {
    Ok(snow::Builder::new(pattern.parse()?).local_private_key(local.private())?.prologue(PROLOGUE)?)
}

async fn send_handshake<T: Transport>(
    t: &mut T,
    kind: MessageKind,
    mode: Option<u8>,
    noise: &[u8],
) -> Result<(), SecurityError> {
    let mut payload = [0u8; MAX_PAYLOAD_LEN];
    let mut len = 0;
    if let Some(mode) = mode {
        payload[0] = mode;
        len = 1;
    }
    if len + noise.len() > MAX_PAYLOAD_LEN {
        return Err(SecurityError::TooLarge);
    }
    payload[len..len + noise.len()].copy_from_slice(noise);
    let mut out = [0u8; FRAME_BUF];
    let n = encode_into(kind, &payload[..len + noise.len()], &mut out)?;
    t.send(&out[..n]).await?;
    Ok(())
}

async fn receive_handshake<T: Transport>(t: &mut T) -> Result<(MessageKind, Vec<u8>), SecurityError> {
    let mut buf = [0u8; FRAME_BUF];
    let n = timeout(STEP_TIMEOUT, t.receive(&mut buf)).await.map_err(|_| SecurityError::Timeout)??;
    let frame = decode(&buf[..n])?;
    Ok((frame.kind, frame.payload.to_vec()))
}

fn hello_parts(kind: MessageKind, payload: &[u8]) -> Result<(u8, &[u8]), SecurityError> {
    if kind != MessageKind::Hello {
        return Err(SecurityError::Protocol);
    }
    let (&mode, noise) = payload.split_first().ok_or(SecurityError::Protocol)?;
    Ok((mode, noise))
}

fn finish_pairing(hs: HandshakeState) -> Result<PendingPairing, SecurityError> {
    let hash = hs.get_handshake_hash();
    let [a, b, c, d, ..] = *hash else {
        return Err(SecurityError::Noise);
    };
    let code = u32::from_be_bytes([a, b, c, d]) % 1_000_000;
    Ok(PendingPairing { session: SecureSession::from_handshake(hs)?, code })
}

/// Startet das Pairing mit einem neuen Partner (Noise XX).
pub async fn initiate_pairing<T: Transport>(t: &mut T, local: &Identity) -> Result<PendingPairing, SecurityError> {
    let mut hs = builder(PATTERN_PAIRING, local)?.build_initiator()?;
    let mut buf = [0u8; MAX_PAYLOAD_LEN];
    let mut scratch = [0u8; MAX_PAYLOAD_LEN];

    let n = hs.write_message(&[], &mut buf)?;
    send_handshake(t, MessageKind::Hello, Some(MODE_PAIR), &buf[..n]).await?;

    let (kind, payload) = receive_handshake(t).await?;
    let (mode, noise) = hello_parts(kind, &payload)?;
    if mode != MODE_PAIR {
        return Err(SecurityError::Protocol);
    }
    hs.read_message(noise, &mut scratch)?;

    let n = hs.write_message(&[], &mut buf)?;
    send_handshake(t, MessageKind::Auth, None, &buf[..n]).await?;
    finish_pairing(hs)
}

/// Verbindet sich mit einem bereits gekoppelten Partner, dessen öffentlicher Schlüssel bekannt ist (Noise IK).
pub async fn initiate_reconnect<T: Transport>(
    t: &mut T,
    local: &Identity,
    remote_public: &[u8; KEY_LEN],
) -> Result<SecureSession, SecurityError> {
    let mut hs = builder(PATTERN_RECONNECT, local)?.remote_public_key(remote_public)?.build_initiator()?;
    let mut buf = [0u8; MAX_PAYLOAD_LEN];
    let mut scratch = [0u8; MAX_PAYLOAD_LEN];

    let n = hs.write_message(&[], &mut buf)?;
    send_handshake(t, MessageKind::Hello, Some(MODE_RECONNECT), &buf[..n]).await?;

    let (kind, payload) = receive_handshake(t).await?;
    let (mode, noise) = hello_parts(kind, &payload)?;
    if mode != MODE_RECONNECT {
        return Err(SecurityError::Protocol);
    }
    hs.read_message(noise, &mut scratch)?;
    SecureSession::from_handshake(hs)
}

/// Nimmt eine eingehende Verbindung an: neues Pairing oder Wiederverbindung eines vertrauten Partners.
///
/// Unbekannte Partner bei einer Wiederverbindung werden ohne Antwort abgewiesen.
pub async fn accept<T: Transport, S: TrustStore>(
    t: &mut T,
    local: &Identity,
    store: &S,
) -> Result<Accepted, SecurityError> {
    let (kind, payload) = receive_handshake(t).await?;
    let (mode, msg1) = hello_parts(kind, &payload)?;
    match mode {
        MODE_PAIR => accept_pairing(t, local, msg1).await.map(Accepted::Pairing),
        MODE_RECONNECT => accept_reconnect(t, local, store, msg1).await.map(Accepted::Session),
        _ => Err(SecurityError::Protocol),
    }
}

async fn accept_pairing<T: Transport>(t: &mut T, local: &Identity, msg1: &[u8]) -> Result<PendingPairing, SecurityError> {
    let mut hs = builder(PATTERN_PAIRING, local)?.build_responder()?;
    let mut buf = [0u8; MAX_PAYLOAD_LEN];
    let mut scratch = [0u8; MAX_PAYLOAD_LEN];

    hs.read_message(msg1, &mut scratch)?;
    let n = hs.write_message(&[], &mut buf)?;
    send_handshake(t, MessageKind::Hello, Some(MODE_PAIR), &buf[..n]).await?;

    let (kind, payload) = receive_handshake(t).await?;
    if kind != MessageKind::Auth {
        return Err(SecurityError::Protocol);
    }
    hs.read_message(&payload, &mut scratch)?;
    finish_pairing(hs)
}

async fn accept_reconnect<T: Transport, S: TrustStore>(
    t: &mut T,
    local: &Identity,
    store: &S,
    msg1: &[u8],
) -> Result<SecureSession, SecurityError> {
    let mut hs = builder(PATTERN_RECONNECT, local)?.build_responder()?;
    let mut buf = [0u8; MAX_PAYLOAD_LEN];
    let mut scratch = [0u8; MAX_PAYLOAD_LEN];

    hs.read_message(msg1, &mut scratch)?;
    let remote: [u8; KEY_LEN] =
        hs.get_remote_static().ok_or(SecurityError::Noise)?.try_into().map_err(|_| SecurityError::Noise)?;
    if !store.is_trusted(&remote) {
        return Err(SecurityError::UnknownPeer);
    }
    let n = hs.write_message(&[], &mut buf)?;
    send_handshake(t, MessageKind::Hello, Some(MODE_RECONNECT), &buf[..n]).await?;
    SecureSession::from_handshake(hs)
}

#[cfg(test)]
mod tests {
    use protocol::{Message, MouseMove};
    use transport::MemoryTransport;

    use super::*;
    use crate::{MemoryTrustStore, SecureChannel};

    fn identity() -> Identity {
        Identity::generate().unwrap()
    }

    async fn pair_devices(
        ta: &mut MemoryTransport,
        tb: &mut MemoryTransport,
        a: &Identity,
        b: &Identity,
        store_a: &mut MemoryTrustStore,
        store_b: &mut MemoryTrustStore,
    ) -> (SecureSession, SecureSession) {
        let (pa, pb) = tokio::join!(initiate_pairing(ta, a), accept(tb, b, store_b));
        let pa = pa.unwrap();
        let Accepted::Pairing(pb) = pb.unwrap() else { panic!("Pairing erwartet") };
        assert_eq!(pa.code(), pb.code());
        assert_eq!(pa.code_text().len(), 6);
        assert!(pa.code() < 1_000_000);
        (pa.confirm(store_a), pb.confirm(store_b))
    }

    #[tokio::test]
    async fn pairing_exchanges_keys_and_codes() {
        let (mut ta, mut tb) = MemoryTransport::pair(512);
        let (a, b) = (identity(), identity());
        let (mut store_a, mut store_b) = (MemoryTrustStore::default(), MemoryTrustStore::default());

        let (mut sa, mut sb) = pair_devices(&mut ta, &mut tb, &a, &b, &mut store_a, &mut store_b).await;

        assert_eq!(sa.remote_public(), b.public());
        assert_eq!(sb.remote_public(), a.public());
        assert!(store_a.is_trusted(b.public()));
        assert!(store_b.is_trusted(a.public()));

        let mut wire = [0u8; 64];
        let mut out = [0u8; 64];
        let n = sa.seal(b"ping", &mut wire).unwrap();
        let m = sb.open(&wire[..n], &mut out).unwrap();
        assert_eq!(&out[..m], b"ping");
    }

    #[tokio::test]
    async fn rejected_pairing_stores_nothing() {
        let (mut ta, mut tb) = MemoryTransport::pair(512);
        let (a, b) = (identity(), identity());
        let store_b = MemoryTrustStore::default();

        let (pa, pb) = tokio::join!(initiate_pairing(&mut ta, &a), accept(&mut tb, &b, &store_b));
        drop((pa.unwrap(), pb.unwrap()));
        assert!(!store_b.is_trusted(a.public()));
    }

    #[tokio::test]
    async fn reconnect_works_for_trusted_peers() {
        let (a, b) = (identity(), identity());
        let (mut store_a, mut store_b) = (MemoryTrustStore::default(), MemoryTrustStore::default());
        {
            let (mut ta, mut tb) = MemoryTransport::pair(512);
            pair_devices(&mut ta, &mut tb, &a, &b, &mut store_a, &mut store_b).await;
        }

        let (ta, tb) = MemoryTransport::pair(512);
        let (mut ta, mut tb) = (ta, tb);
        let (sa, sb) = tokio::join!(initiate_reconnect(&mut ta, &a, b.public()), accept(&mut tb, &b, &store_b));
        let sa = sa.unwrap();
        let Accepted::Session(sb) = sb.unwrap() else { panic!("Sitzung erwartet") };
        assert_eq!(sb.remote_public(), a.public());

        let (mut ca, mut cb) = (SecureChannel::new(ta, sa), SecureChannel::new(tb, sb));
        let msg = Message::MouseMove(MouseMove { dx: 3, dy: -4 });
        ca.send(&msg).await.unwrap();
        assert_eq!(cb.receive().await, Ok(msg));
    }

    #[tokio::test(start_paused = true)]
    async fn reconnect_from_unknown_peer_is_rejected() {
        let (a, b) = (identity(), identity());
        let (mut ta, mut tb) = MemoryTransport::pair(512);
        let store_b = MemoryTrustStore::default();

        let (ra, rb) = tokio::join!(initiate_reconnect(&mut ta, &a, b.public()), accept(&mut tb, &b, &store_b));
        assert_eq!(rb.unwrap_err(), SecurityError::UnknownPeer);
        assert_eq!(ra.unwrap_err(), SecurityError::Timeout);
    }

    #[tokio::test(start_paused = true)]
    async fn reconnect_with_wrong_responder_key_fails() {
        let (a, b, other) = (identity(), identity(), identity());
        let (mut ta, mut tb) = MemoryTransport::pair(512);
        let mut store_b = MemoryTrustStore::default();
        store_b.trust(*a.public());

        let (ra, rb) = tokio::join!(initiate_reconnect(&mut ta, &a, other.public()), accept(&mut tb, &b, &store_b));
        assert_eq!(rb.unwrap_err(), SecurityError::Noise);
        assert_eq!(ra.unwrap_err(), SecurityError::Timeout);
    }

    #[tokio::test]
    async fn rejects_unexpected_first_frame() {
        let (mut ta, mut tb) = MemoryTransport::pair(512);
        let b = identity();
        let store = MemoryTrustStore::default();

        let mut buf = [0u8; 16];
        let n = Message::Keepalive.encode_into(&mut buf).unwrap();
        ta.send(&buf[..n]).await.unwrap();
        assert_eq!(accept(&mut tb, &b, &store).await.unwrap_err(), SecurityError::Protocol);
    }

    #[tokio::test]
    async fn rejects_unknown_mode_and_malformed_hello() {
        let (mut ta, mut tb) = MemoryTransport::pair(512);
        let b = identity();
        let store = MemoryTrustStore::default();
        let mut buf = [0u8; 64];

        let n = encode_into(MessageKind::Hello, &[99, 1, 2, 3], &mut buf).unwrap();
        ta.send(&buf[..n]).await.unwrap();
        assert_eq!(accept(&mut tb, &b, &store).await.unwrap_err(), SecurityError::Protocol);

        let n = encode_into(MessageKind::Hello, &[], &mut buf).unwrap();
        ta.send(&buf[..n]).await.unwrap();
        assert_eq!(accept(&mut tb, &b, &store).await.unwrap_err(), SecurityError::Protocol);

        // Zu kurze Noise-Nachricht (weniger als ein Schlüssel).
        let n = encode_into(MessageKind::Hello, &[MODE_PAIR, 1, 2, 3], &mut buf).unwrap();
        ta.send(&buf[..n]).await.unwrap();
        assert_eq!(accept(&mut tb, &b, &store).await.unwrap_err(), SecurityError::Noise);
    }

    #[tokio::test]
    async fn rejects_forged_final_pairing_message() {
        let (mut ta, mut tb) = MemoryTransport::pair(512);
        let (a, b) = (identity(), identity());
        let store = MemoryTrustStore::default();

        // Initiator spielt Nachricht 1 korrekt, schickt aber Nachricht 3 mit zufälligem Inhalt.
        let mut hs = builder(PATTERN_PAIRING, &a).unwrap().build_initiator().unwrap();
        let mut buf = [0u8; MAX_PAYLOAD_LEN];
        let n = hs.write_message(&[], &mut buf).unwrap();
        send_handshake(&mut ta, MessageKind::Hello, Some(MODE_PAIR), &buf[..n]).await.unwrap();

        let attacker = async {
            let _ = receive_handshake(&mut ta).await.unwrap();
            send_handshake(&mut ta, MessageKind::Auth, None, &[0x42; 64]).await.unwrap();
        };
        let (_, result) = tokio::join!(attacker, accept(&mut tb, &b, &store));
        assert_eq!(result.unwrap_err(), SecurityError::Noise);
    }

    #[tokio::test]
    async fn rejects_wrong_frame_kind_for_final_message() {
        let (mut ta, mut tb) = MemoryTransport::pair(512);
        let (a, b) = (identity(), identity());
        let store = MemoryTrustStore::default();

        let mut hs = builder(PATTERN_PAIRING, &a).unwrap().build_initiator().unwrap();
        let mut buf = [0u8; MAX_PAYLOAD_LEN];
        let n = hs.write_message(&[], &mut buf).unwrap();
        send_handshake(&mut ta, MessageKind::Hello, Some(MODE_PAIR), &buf[..n]).await.unwrap();

        let attacker = async {
            let _ = receive_handshake(&mut ta).await.unwrap();
            send_handshake(&mut ta, MessageKind::Keepalive, None, &[]).await.unwrap();
        };
        let (_, result) = tokio::join!(attacker, accept(&mut tb, &b, &store));
        assert_eq!(result.unwrap_err(), SecurityError::Protocol);
    }
}
