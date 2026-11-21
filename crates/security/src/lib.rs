//! Sicherheitsschicht: Geräteidentitäten, Pairing, Sitzungen und Replay-Schutz auf Basis von Noise.
//!
//! - Pairing: `Noise_XX_25519_ChaChaPoly_BLAKE2s`, beide Seiten zeigen einen sechsstelligen Code aus dem
//!   Handshake-Hash; erst nach Bestätigung durch den Benutzer wird der Partner vertraut.
//! - Wiederverbindung: `Noise_IK_25519_ChaChaPoly_BLAKE2s` mit bekanntem Schlüssel des Partners.
//! - Sitzung: Noise-Transport mit expliziten Nonces, damit einzelne Paketverluste die Verbindung nicht
//!   zerstören; ein Schiebefenster verwirft Wiederholungen.
//!
//! Wire-Format im Betrieb: `[nonce: u64 BE][Noise-Chiffretext des inneren Protokoll-Frames]`.
//! Handshake-Nachrichten laufen als `HELLO`/`AUTH`-Frames des Protokolls.

mod error;
mod handshake;
mod identity;
mod replay;
mod session;
mod trust;

#[cfg(test)]
mod testutil;

pub use error::SecurityError;
pub use handshake::{accept, initiate_pairing, initiate_reconnect, Accepted, PendingPairing};
pub use identity::{Identity, KEY_LEN};
pub use replay::ReplayWindow;
pub use session::{SecureChannel, SecureSession, MAX_WIRE_LEN};
pub use trust::{MemoryTrustStore, TrustStore};
