use std::fmt;

use protocol::{decode, Message, HEADER_LEN, MAX_PAYLOAD_LEN};
use snow::{HandshakeState, StatelessTransportState};
use transport::Transport;

use crate::{ReplayWindow, SecurityError, KEY_LEN};

const NONCE_LEN: usize = 8;
const TAG_LEN: usize = 16;

/// Größte mögliche Wire-Nachricht: Nonce + innerer Frame + Authentifizierungstag.
pub const MAX_WIRE_LEN: usize = NONCE_LEN + HEADER_LEN + MAX_PAYLOAD_LEN + TAG_LEN;

/// Verschlüsselte Sitzung mit einem authentifizierten Partner.
pub struct SecureSession {
    state: StatelessTransportState,
    remote: [u8; KEY_LEN],
    send_nonce: u64,
    replay: ReplayWindow,
}

impl fmt::Debug for SecureSession {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SecureSession").field("remote", &self.remote).finish_non_exhaustive()
    }
}

impl SecureSession {
    pub(crate) fn from_handshake(hs: HandshakeState) -> Result<Self, SecurityError> {
        let remote: [u8; KEY_LEN] =
            hs.get_remote_static().ok_or(SecurityError::Noise)?.try_into().map_err(|_| SecurityError::Noise)?;
        Ok(Self {
            state: hs.into_stateless_transport_mode()?,
            remote,
            send_nonce: 0,
            replay: ReplayWindow::new(),
        })
    }

    pub fn remote_public(&self) -> &[u8; KEY_LEN] {
        &self.remote
    }

    /// Verschlüsselt `plaintext` nach `out` und gibt die Wire-Länge zurück.
    pub fn seal(&mut self, plaintext: &[u8], out: &mut [u8]) -> Result<usize, SecurityError> {
        if self.send_nonce == u64::MAX {
            return Err(SecurityError::Exhausted);
        }
        if out.len() < NONCE_LEN + plaintext.len() + TAG_LEN {
            return Err(SecurityError::TooLarge);
        }
        let written = self.state.write_message(self.send_nonce, plaintext, &mut out[NONCE_LEN..])?;
        out[..NONCE_LEN].copy_from_slice(&self.send_nonce.to_be_bytes());
        self.send_nonce += 1;
        Ok(NONCE_LEN + written)
    }

    /// Prüft und entschlüsselt eine Wire-Nachricht. `out` muss mindestens so groß sein wie der Chiffretext.
    /// Der Replay-Zustand ändert sich erst nach erfolgreicher Authentifizierung.
    pub fn open(&mut self, wire: &[u8], out: &mut [u8]) -> Result<usize, SecurityError> {
        if wire.len() < NONCE_LEN + TAG_LEN {
            return Err(SecurityError::Decrypt);
        }
        if wire.len() > MAX_WIRE_LEN {
            return Err(SecurityError::TooLarge);
        }
        let (nonce, ciphertext) = wire.split_at(NONCE_LEN);
        let seq = u64::from_be_bytes(nonce.try_into().map_err(|_| SecurityError::Decrypt)?);
        if !self.replay.check(seq) {
            return Err(SecurityError::Replay);
        }
        if out.len() < ciphertext.len() {
            return Err(SecurityError::TooLarge);
        }
        let n = self.state.read_message(seq, ciphertext, out).map_err(|_| SecurityError::Decrypt)?;
        self.replay.update(seq);
        Ok(n)
    }
}

/// Verbindet einen `Transport` mit einer `SecureSession` und tauscht typisierte Nachrichten aus.
///
/// Fehler wie `Replay`, `Decrypt` oder `Message` betreffen nur ein einzelnes Paket; der Kanal bleibt
/// nutzbar und der Aufrufer verwirft das Paket. `Transport`-Fehler beenden dagegen die Verbindung.
pub struct SecureChannel<T: Transport> {
    transport: T,
    session: SecureSession,
}

impl<T: Transport> SecureChannel<T> {
    pub fn new(transport: T, session: SecureSession) -> Self {
        Self { transport, session }
    }

    pub fn remote_public(&self) -> &[u8; KEY_LEN] {
        self.session.remote_public()
    }

    pub fn transport_mut(&mut self) -> &mut T {
        &mut self.transport
    }

    /// Gibt den Transport zurück, z. B. um die Verbindung neu aufzubauen. Die Sitzung wird verworfen.
    pub fn into_transport(self) -> T {
        self.transport
    }

    pub async fn send(&mut self, message: &Message) -> Result<(), SecurityError> {
        let mut inner = [0u8; HEADER_LEN + MAX_PAYLOAD_LEN];
        let inner_len = message.encode_into(&mut inner)?;
        let mut wire = [0u8; MAX_WIRE_LEN];
        let wire_len = self.session.seal(&inner[..inner_len], &mut wire)?;
        self.transport.send(&wire[..wire_len]).await?;
        Ok(())
    }

    pub async fn receive(&mut self) -> Result<Message, SecurityError> {
        let mut wire = [0u8; MAX_WIRE_LEN];
        let wire_len = self.transport.receive(&mut wire).await?;
        let mut plain = [0u8; MAX_WIRE_LEN];
        let plain_len = self.session.open(&wire[..wire_len], &mut plain)?;
        let frame = decode(&plain[..plain_len])?;
        Ok(Message::parse(&frame)?)
    }
}

#[cfg(test)]
mod tests {
    use protocol::{encode_into, MessageKind, MouseMove, PayloadError};
    use transport::MemoryTransport;

    use super::*;
    use crate::testutil::sessions;

    fn seal(session: &mut SecureSession, plaintext: &[u8]) -> Vec<u8> {
        let mut out = [0u8; MAX_WIRE_LEN];
        let n = session.seal(plaintext, &mut out).unwrap();
        out[..n].to_vec()
    }

    #[tokio::test]
    async fn seal_and_open_roundtrip() {
        let (mut a, mut b) = sessions().await;
        let wire = seal(&mut a, b"hello");
        let mut out = [0u8; 64];
        let n = b.open(&wire, &mut out).unwrap();
        assert_eq!(&out[..n], b"hello");
    }

    #[tokio::test]
    async fn rejects_replay() {
        let (mut a, mut b) = sessions().await;
        let wire = seal(&mut a, b"x");
        let mut out = [0u8; 64];
        assert!(b.open(&wire, &mut out).is_ok());
        assert_eq!(b.open(&wire, &mut out), Err(SecurityError::Replay));
    }

    #[tokio::test]
    async fn rejects_tampered_data_and_keeps_state() {
        let (mut a, mut b) = sessions().await;
        let wire = seal(&mut a, b"secret");
        let mut out = [0u8; 64];

        for index in [0, NONCE_LEN - 1, NONCE_LEN, wire.len() - 1] {
            let mut bad = wire.clone();
            bad[index] ^= 0x01;
            assert_eq!(b.open(&bad, &mut out), Err(SecurityError::Decrypt), "index {index}");
        }
        // Fehlgeschlagene Prüfungen verbrauchen die Nonce nicht.
        assert!(b.open(&wire, &mut out).is_ok());
    }

    #[tokio::test]
    async fn tolerates_loss_and_reordering() {
        let (mut a, mut b) = sessions().await;
        let w1 = seal(&mut a, b"1");
        let w2 = seal(&mut a, b"2");
        let w3 = seal(&mut a, b"3");
        let mut out = [0u8; 64];
        assert!(b.open(&w3, &mut out).is_ok());
        assert!(b.open(&w1, &mut out).is_ok());
        assert!(b.open(&w2, &mut out).is_ok());
        assert_eq!(b.open(&w2, &mut out), Err(SecurityError::Replay));
    }

    #[tokio::test]
    async fn rejects_reflected_packets() {
        let (mut a, _b) = sessions().await;
        let wire = seal(&mut a, b"x");
        let mut out = [0u8; 64];
        assert_eq!(a.open(&wire, &mut out), Err(SecurityError::Decrypt));
    }

    #[tokio::test]
    async fn rejects_malformed_wire_input() {
        let (mut a, _) = sessions().await;
        let mut out = [0u8; 64];
        assert_eq!(a.open(&[], &mut out), Err(SecurityError::Decrypt));
        assert_eq!(a.open(&[0u8; NONCE_LEN + TAG_LEN - 1], &mut out), Err(SecurityError::Decrypt));
        assert_eq!(a.open(&vec![0u8; MAX_WIRE_LEN + 1], &mut out), Err(SecurityError::TooLarge));
        let mut tiny = [0u8; 2];
        assert_eq!(a.seal(b"abc", &mut tiny), Err(SecurityError::TooLarge));
    }

    #[tokio::test]
    async fn refuses_to_reuse_exhausted_nonce() {
        let (mut a, _) = sessions().await;
        a.send_nonce = u64::MAX;
        let mut out = [0u8; 64];
        assert_eq!(a.seal(b"x", &mut out), Err(SecurityError::Exhausted));
    }

    async fn channel_pair() -> (SecureChannel<MemoryTransport>, MemoryTransport, SecureSession) {
        let (ta, tb) = MemoryTransport::pair(2048);
        let (sa, sb) = sessions().await;
        (SecureChannel::new(tb, sb), ta, sa)
    }

    #[tokio::test]
    async fn channel_exchanges_typed_messages() {
        let (ta, tb) = MemoryTransport::pair(2048);
        let (sa, sb) = sessions().await;
        let (mut a, mut b) = (SecureChannel::new(ta, sa), SecureChannel::new(tb, sb));
        let msg = Message::MouseMove(MouseMove { dx: -5, dy: 7 });
        a.send(&msg).await.unwrap();
        assert_eq!(b.receive().await, Ok(msg));
        b.send(&Message::Keepalive).await.unwrap();
        assert_eq!(a.receive().await, Ok(Message::Keepalive));
    }

    #[tokio::test]
    async fn channel_survives_garbage_and_replay() {
        let (mut channel, mut raw, mut peer) = channel_pair().await;

        raw.send(&[0xAB; 40]).await.unwrap();
        assert_eq!(channel.receive().await, Err(SecurityError::Decrypt));

        let mut inner = [0u8; 16];
        let n = Message::Keepalive.encode_into(&mut inner).unwrap();
        let wire = seal(&mut peer, &inner[..n]);
        raw.send(&wire).await.unwrap();
        raw.send(&wire).await.unwrap();
        assert_eq!(channel.receive().await, Ok(Message::Keepalive));
        assert_eq!(channel.receive().await, Err(SecurityError::Replay));
    }

    #[tokio::test]
    async fn channel_rejects_authentic_but_invalid_messages() {
        let (mut channel, mut raw, mut peer) = channel_pair().await;
        let mut inner = [0u8; 16];

        let n = encode_into(MessageKind::MouseButton, &[9, 1], &mut inner).unwrap();
        raw.send(&seal(&mut peer, &inner[..n])).await.unwrap();
        assert_eq!(channel.receive().await, Err(SecurityError::Message(PayloadError::InvalidValue)));

        let n = encode_into(MessageKind::Hello, &[], &mut inner).unwrap();
        raw.send(&seal(&mut peer, &inner[..n])).await.unwrap();
        assert_eq!(
            channel.receive().await,
            Err(SecurityError::Message(PayloadError::Unsupported(MessageKind::Hello)))
        );

        raw.send(&seal(&mut peer, &[1, 2, 3])).await.unwrap();
        assert!(matches!(channel.receive().await, Err(SecurityError::Protocol)));
    }
}
