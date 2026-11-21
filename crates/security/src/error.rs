use std::fmt;

use protocol::{FrameError, PayloadError};
use transport::TransportError;

/// Fehler der Sicherheitsschicht; enthalten bewusst keine Schlüssel oder Nutzdaten.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SecurityError {
    Transport(TransportError),
    /// Unerwarteter oder ungültiger Frame während des Handshakes.
    Protocol,
    /// Fehler im Noise-Handshake (z. B. falscher Schlüssel).
    Noise,
    /// Der Partner ist nicht als vertrauenswürdig gespeichert.
    UnknownPeer,
    Timeout,
    /// Nachricht wurde bereits empfangen oder ist zu alt.
    Replay,
    /// Entschlüsselung oder Integritätsprüfung fehlgeschlagen.
    Decrypt,
    TooLarge,
    /// Nonce-Zähler ausgeschöpft; neue Sitzung nötig.
    Exhausted,
    Message(PayloadError),
}

impl fmt::Display for SecurityError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Transport(e) => write!(f, "{e}"),
            Self::Protocol => write!(f, "ungültiger Handshake-Frame"),
            Self::Noise => write!(f, "Handshake fehlgeschlagen"),
            Self::UnknownPeer => write!(f, "unbekannter Partner"),
            Self::Timeout => write!(f, "Zeitüberschreitung"),
            Self::Replay => write!(f, "wiederholte oder zu alte Nachricht"),
            Self::Decrypt => write!(f, "Entschlüsselung fehlgeschlagen"),
            Self::TooLarge => write!(f, "Nachricht zu groß"),
            Self::Exhausted => write!(f, "Nonce-Zähler ausgeschöpft"),
            Self::Message(e) => write!(f, "ungültige Nachricht: {e:?}"),
        }
    }
}

impl std::error::Error for SecurityError {}

impl From<TransportError> for SecurityError {
    fn from(e: TransportError) -> Self {
        Self::Transport(e)
    }
}

impl From<snow::Error> for SecurityError {
    fn from(_: snow::Error) -> Self {
        Self::Noise
    }
}

impl From<FrameError> for SecurityError {
    fn from(_: FrameError) -> Self {
        Self::Protocol
    }
}

impl From<PayloadError> for SecurityError {
    fn from(e: PayloadError) -> Self {
        Self::Message(e)
    }
}
