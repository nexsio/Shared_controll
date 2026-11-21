//! Transport-Schnittstelle: überträgt opake Frames zwischen zwei Endpunkten.
//!
//! Ein Aufruf von `send` bzw. `receive` entspricht genau einem Frame (Nachrichtengrenzen bleiben
//! erhalten). Der Transport kennt weder Protokollinhalte noch Input-, Switching- oder
//! Clipboard-Logik. Verschlüsselung und Authentifizierung liegen in einer Schicht darüber.

mod memory;

use std::{fmt, future::Future};

pub use memory::MemoryTransport;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectionState {
    Disconnected,
    Connecting,
    Connected,
    Reconnecting,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TransportStatus {
    pub state: ConnectionState,
    /// Größter Frame, den dieser Transport in einem Stück übertragen kann.
    pub max_frame_len: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TransportError {
    NotConnected,
    /// Gegenstelle oder Verbindung wurde beendet.
    Closed,
    Timeout,
    FrameTooLarge { max: usize },
    /// Der übergebene Empfangspuffer ist für den eingetroffenen Frame zu klein.
    BufferTooSmall { needed: usize },
    /// Fehler der konkreten Implementierung (ohne Nutzdaten).
    Backend(String),
}

impl fmt::Display for TransportError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotConnected => write!(f, "nicht verbunden"),
            Self::Closed => write!(f, "Verbindung beendet"),
            Self::Timeout => write!(f, "Zeitüberschreitung"),
            Self::FrameTooLarge { max } => write!(f, "Frame größer als {max} Bytes"),
            Self::BufferTooSmall { needed } => write!(f, "Empfangspuffer zu klein, benötigt {needed} Bytes"),
            Self::Backend(msg) => write!(f, "Transportfehler: {msg}"),
        }
    }
}

impl std::error::Error for TransportError {}

/// Einheitliche Schnittstelle für `BluetoothTransport`, `UsbDongleTransport`, `RadioTransport` u. a.
pub trait Transport: Send {
    fn connect(&mut self) -> impl Future<Output = Result<(), TransportError>> + Send;

    fn disconnect(&mut self) -> impl Future<Output = Result<(), TransportError>> + Send;

    /// Sendet genau einen Frame.
    fn send(&mut self, frame: &[u8]) -> impl Future<Output = Result<(), TransportError>> + Send;

    /// Wartet auf den nächsten Frame, schreibt ihn in `buf` und gibt die Länge zurück.
    fn receive(&mut self, buf: &mut [u8]) -> impl Future<Output = Result<usize, TransportError>> + Send;

    fn get_status(&self) -> TransportStatus;

    /// Stellt eine abgebrochene Verbindung zu einem bereits bekannten Partner wieder her.
    fn reconnect(&mut self) -> impl Future<Output = Result<(), TransportError>> + Send;
}
