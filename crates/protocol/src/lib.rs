//! Versioniertes binäres Frame-Protokoll (Phase 1 – Grundlage).
//!
//! Frame-Aufbau (Big Endian):
//! ```text
//! [0]     version   (u8)  – muss PROTOCOL_VERSION sein
//! [1]     kind      (u8)  – MessageKind
//! [2..4]  length    (u16) – Länge des Payloads
//! [4..]   payload   (length Bytes)
//! ```
//! Kein Heap, keine Abhängigkeiten – damit später auch auf Mikrocontrollern nutzbar.

#![cfg_attr(not(test), no_std)]

mod payload;

pub use payload::{
    Button, ClipboardChunk, Edge, Key, Message, MouseButton, MouseMove, MouseWheel, PayloadError,
    SwitchConfirm, SwitchRequest, CLIPBOARD_CHUNK_LEN, KEY_MAX,
};

/// Aktuelle Protokollversion.
pub const PROTOCOL_VERSION: u8 = 1;
/// Größe des Frame-Headers in Bytes.
pub const HEADER_LEN: usize = 4;
/// Maximale Payload-Größe; schützt vor übergroßen Nachrichten.
pub const MAX_PAYLOAD_LEN: usize = 1024;

/// Nachrichtentypen gemäß Plan.md (Abschnitt 4.5).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum MessageKind {
    Hello = 1,
    Auth = 2,
    Keepalive = 3,
    MouseMove = 4,
    MouseButton = 5,
    MouseWheel = 6,
    KeyDown = 7,
    KeyUp = 8,
    SwitchRequest = 9,
    SwitchConfirm = 10,
    ClipboardOffer = 11,
    ClipboardData = 12,
    ClipboardAck = 13,
    Disconnect = 14,
    Error = 15,
}

impl MessageKind {
    /// Wandelt ein Byte in einen Nachrichtentyp um; unbekannte Werte werden abgelehnt.
    pub const fn from_u8(value: u8) -> Option<Self> {
        Some(match value {
            1 => Self::Hello,
            2 => Self::Auth,
            3 => Self::Keepalive,
            4 => Self::MouseMove,
            5 => Self::MouseButton,
            6 => Self::MouseWheel,
            7 => Self::KeyDown,
            8 => Self::KeyUp,
            9 => Self::SwitchRequest,
            10 => Self::SwitchConfirm,
            11 => Self::ClipboardOffer,
            12 => Self::ClipboardData,
            13 => Self::ClipboardAck,
            14 => Self::Disconnect,
            15 => Self::Error,
            _ => return None,
        })
    }
}

/// Fehler bei Kodierung oder Dekodierung.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameError {
    /// Eingabe kürzer als Header bzw. angekündigte Länge.
    Truncated,
    /// Versionsfeld passt nicht zur unterstützten Version.
    UnsupportedVersion(u8),
    /// Unbekannter Nachrichtentyp.
    UnknownKind(u8),
    /// Payload überschreitet MAX_PAYLOAD_LEN.
    PayloadTooLarge(usize),
    /// Ausgabepuffer zu klein.
    BufferTooSmall,
}

/// Ein dekodierter Frame, der die Payload borgt (keine Kopie).
#[derive(Debug, PartialEq, Eq)]
pub struct Frame<'a> {
    pub kind: MessageKind,
    pub payload: &'a [u8],
}

/// Kodiert einen Frame in `out` und gibt die Anzahl geschriebener Bytes zurück.
pub fn encode_into(kind: MessageKind, payload: &[u8], out: &mut [u8]) -> Result<usize, FrameError> {
    if payload.len() > MAX_PAYLOAD_LEN {
        return Err(FrameError::PayloadTooLarge(payload.len()));
    }
    let total = HEADER_LEN + payload.len();
    if out.len() < total {
        return Err(FrameError::BufferTooSmall);
    }
    out[0] = PROTOCOL_VERSION;
    out[1] = kind as u8;
    let len = payload.len() as u16;
    out[2..4].copy_from_slice(&len.to_be_bytes());
    out[HEADER_LEN..total].copy_from_slice(payload);
    Ok(total)
}

/// Dekodiert genau einen Frame. Zusätzliche Bytes am Ende werden abgelehnt.
pub fn decode(input: &[u8]) -> Result<Frame<'_>, FrameError> {
    if input.len() < HEADER_LEN {
        return Err(FrameError::Truncated);
    }
    let version = input[0];
    if version != PROTOCOL_VERSION {
        return Err(FrameError::UnsupportedVersion(version));
    }
    let kind = MessageKind::from_u8(input[1]).ok_or(FrameError::UnknownKind(input[1]))?;
    let len = u16::from_be_bytes([input[2], input[3]]) as usize;
    if len > MAX_PAYLOAD_LEN {
        return Err(FrameError::PayloadTooLarge(len));
    }
    let total = HEADER_LEN + len;
    if input.len() != total {
        return Err(FrameError::Truncated);
    }
    Ok(Frame {
        kind,
        payload: &input[HEADER_LEN..total],
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let mut buf = [0u8; 64];
        let n = encode_into(MessageKind::MouseMove, &[1, 2, 3], &mut buf).unwrap();
        let frame = decode(&buf[..n]).unwrap();
        assert_eq!(frame.kind, MessageKind::MouseMove);
        assert_eq!(frame.payload, &[1, 2, 3]);
    }

    #[test]
    fn empty_payload_roundtrip() {
        let mut buf = [0u8; HEADER_LEN];
        let n = encode_into(MessageKind::Keepalive, &[], &mut buf).unwrap();
        assert_eq!(decode(&buf[..n]).unwrap().payload, &[] as &[u8]);
    }

    #[test]
    fn rejects_short_input() {
        assert_eq!(decode(&[PROTOCOL_VERSION, 1]), Err(FrameError::Truncated));
    }

    #[test]
    fn rejects_wrong_version() {
        assert_eq!(
            decode(&[99, 1, 0, 0]),
            Err(FrameError::UnsupportedVersion(99))
        );
    }

    #[test]
    fn rejects_unknown_kind() {
        assert_eq!(
            decode(&[PROTOCOL_VERSION, 0, 0, 0]),
            Err(FrameError::UnknownKind(0))
        );
    }

    #[test]
    fn rejects_length_mismatch() {
        // Angekündigt 2 Bytes, geliefert 1 Byte.
        assert_eq!(
            decode(&[PROTOCOL_VERSION, 3, 0, 2, 0xAA]),
            Err(FrameError::Truncated)
        );
        // Angekündigt 0 Bytes, geliefert 1 zusätzliches Byte.
        assert_eq!(
            decode(&[PROTOCOL_VERSION, 3, 0, 0, 0xAA]),
            Err(FrameError::Truncated)
        );
    }

    #[test]
    fn rejects_oversized_length_field() {
        let len = (MAX_PAYLOAD_LEN + 1) as u16;
        let [hi, lo] = len.to_be_bytes();
        assert_eq!(
            decode(&[PROTOCOL_VERSION, 3, hi, lo]),
            Err(FrameError::PayloadTooLarge(MAX_PAYLOAD_LEN + 1))
        );
    }

    #[test]
    fn encode_rejects_oversized_payload_and_small_buffer() {
        let big = [0u8; MAX_PAYLOAD_LEN + 1];
        let mut buf = [0u8; 2048];
        assert_eq!(
            encode_into(MessageKind::ClipboardData, &big, &mut buf),
            Err(FrameError::PayloadTooLarge(MAX_PAYLOAD_LEN + 1))
        );
        let mut tiny = [0u8; 2];
        assert_eq!(
            encode_into(MessageKind::Hello, &[], &mut tiny),
            Err(FrameError::BufferTooSmall)
        );
    }

    #[test]
    fn all_kinds_roundtrip() {
        for v in 1u8..=15 {
            let kind = MessageKind::from_u8(v).unwrap();
            assert_eq!(kind as u8, v);
        }
        assert!(MessageKind::from_u8(0).is_none());
        assert!(MessageKind::from_u8(16).is_none());
    }
}
