//! Typisierte Payloads der Eingabe- und Umschaltnachrichten (alle Felder Big Endian).
//!
//! Jede Nachricht hat eine feste Payload-Länge; abweichende Längen und ungültige Werte werden abgelehnt.

use crate::{encode_into, Frame, FrameError, MessageKind};

/// Höchster gültiger Linux-evdev-Tastencode (`KEY_MAX`).
pub const KEY_MAX: u16 = 0x2ff;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PayloadError {
    Length { expected: usize, actual: usize },
    InvalidValue,
    /// Nachrichtentyp ist noch nicht als typisierte Payload implementiert.
    Unsupported(MessageKind),
    Frame(FrameError),
}

impl From<FrameError> for PayloadError {
    fn from(e: FrameError) -> Self {
        Self::Frame(e)
    }
}

/// Relative Mausbewegung, summiert über ein Zeitfenster.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MouseMove {
    pub dx: i16,
    pub dy: i16,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Button {
    Left = 1,
    Right = 2,
    Middle = 3,
    Back = 4,
    Forward = 5,
}

impl Button {
    const fn from_u8(v: u8) -> Option<Self> {
        Some(match v {
            1 => Self::Left,
            2 => Self::Right,
            3 => Self::Middle,
            4 => Self::Back,
            5 => Self::Forward,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MouseButton {
    pub button: Button,
    pub pressed: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MouseWheel {
    pub dx: i16,
    pub dy: i16,
}

/// Tastencode nach Linux-evdev; Übersetzung auf andere Plattformen erfolgt in den Adaptern.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Key {
    pub code: u16,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Edge {
    Left = 0,
    Right = 1,
    Top = 2,
    Bottom = 3,
}

impl Edge {
    const fn from_u8(v: u8) -> Option<Self> {
        Some(match v {
            0 => Self::Left,
            1 => Self::Right,
            2 => Self::Top,
            3 => Self::Bottom,
            _ => return None,
        })
    }
}

/// Wechselwunsch; `position` ist die Lage entlang der Kante, 0..=65535 normiert.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SwitchRequest {
    pub target: u8,
    pub edge: Edge,
    pub position: u16,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SwitchConfirm {
    pub target: u8,
    pub accepted: bool,
}

/// Nutzdaten pro Zwischenablage-Stück; klein gehalten, damit ein Stück in wenigen Funkpaketen Platz findet.
pub const CLIPBOARD_CHUNK_LEN: usize = 128;

/// Ein Stück eines Zwischenablagetextes; `id` bindet die Stücke einer Übertragung zusammen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClipboardChunk {
    pub id: u8,
    pub index: u8,
    pub total: u8,
    pub len: u8,
    pub data: [u8; CLIPBOARD_CHUNK_LEN],
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Message {
    Keepalive,
    MouseMove(MouseMove),
    MouseButton(MouseButton),
    MouseWheel(MouseWheel),
    KeyDown(Key),
    KeyUp(Key),
    SwitchRequest(SwitchRequest),
    SwitchConfirm(SwitchConfirm),
    ClipboardData(ClipboardChunk),
    /// Bestätigt den vollständigen Empfang der Übertragung mit dieser `id`.
    ClipboardAck(u8),
}

/// Größte Payload der hier definierten Nachrichten.
const MAX_TYPED_PAYLOAD: usize = 4 + CLIPBOARD_CHUNK_LEN;

fn expect_len(payload: &[u8], expected: usize) -> Result<(), PayloadError> {
    if payload.len() == expected {
        Ok(())
    } else {
        Err(PayloadError::Length { expected, actual: payload.len() })
    }
}

fn i16_at(p: &[u8], offset: usize) -> i16 {
    i16::from_be_bytes([p[offset], p[offset + 1]])
}

fn bool_at(p: &[u8], offset: usize) -> Result<bool, PayloadError> {
    match p[offset] {
        0 => Ok(false),
        1 => Ok(true),
        _ => Err(PayloadError::InvalidValue),
    }
}

fn key_from(p: &[u8]) -> Result<Key, PayloadError> {
    expect_len(p, 2)?;
    let code = u16::from_be_bytes([p[0], p[1]]);
    if code > KEY_MAX {
        return Err(PayloadError::InvalidValue);
    }
    Ok(Key { code })
}

impl Message {
    pub const fn kind(&self) -> MessageKind {
        match self {
            Self::Keepalive => MessageKind::Keepalive,
            Self::MouseMove(_) => MessageKind::MouseMove,
            Self::MouseButton(_) => MessageKind::MouseButton,
            Self::MouseWheel(_) => MessageKind::MouseWheel,
            Self::KeyDown(_) => MessageKind::KeyDown,
            Self::KeyUp(_) => MessageKind::KeyUp,
            Self::SwitchRequest(_) => MessageKind::SwitchRequest,
            Self::SwitchConfirm(_) => MessageKind::SwitchConfirm,
            Self::ClipboardData(_) => MessageKind::ClipboardData,
            Self::ClipboardAck(_) => MessageKind::ClipboardAck,
        }
    }

    /// Wandelt einen bereits geprüften Frame in eine typisierte Nachricht um.
    pub fn parse(frame: &Frame<'_>) -> Result<Self, PayloadError> {
        let p = frame.payload;
        match frame.kind {
            MessageKind::Keepalive => {
                expect_len(p, 0)?;
                Ok(Self::Keepalive)
            }
            MessageKind::MouseMove => {
                expect_len(p, 4)?;
                Ok(Self::MouseMove(MouseMove { dx: i16_at(p, 0), dy: i16_at(p, 2) }))
            }
            MessageKind::MouseWheel => {
                expect_len(p, 4)?;
                Ok(Self::MouseWheel(MouseWheel { dx: i16_at(p, 0), dy: i16_at(p, 2) }))
            }
            MessageKind::MouseButton => {
                expect_len(p, 2)?;
                let button = Button::from_u8(p[0]).ok_or(PayloadError::InvalidValue)?;
                Ok(Self::MouseButton(MouseButton { button, pressed: bool_at(p, 1)? }))
            }
            MessageKind::KeyDown => Ok(Self::KeyDown(key_from(p)?)),
            MessageKind::KeyUp => Ok(Self::KeyUp(key_from(p)?)),
            MessageKind::SwitchRequest => {
                expect_len(p, 4)?;
                let edge = Edge::from_u8(p[1]).ok_or(PayloadError::InvalidValue)?;
                Ok(Self::SwitchRequest(SwitchRequest {
                    target: p[0],
                    edge,
                    position: u16::from_be_bytes([p[2], p[3]]),
                }))
            }
            MessageKind::SwitchConfirm => {
                expect_len(p, 2)?;
                Ok(Self::SwitchConfirm(SwitchConfirm { target: p[0], accepted: bool_at(p, 1)? }))
            }
            MessageKind::ClipboardData => {
                if p.len() < 4 {
                    return Err(PayloadError::Length { expected: 4, actual: p.len() });
                }
                let (id, index, total, len) = (p[0], p[1], p[2], p[3] as usize);
                if total == 0 || index >= total || len > CLIPBOARD_CHUNK_LEN || p.len() != 4 + len {
                    return Err(PayloadError::InvalidValue);
                }
                let mut data = [0u8; CLIPBOARD_CHUNK_LEN];
                data[..len].copy_from_slice(&p[4..]);
                Ok(Self::ClipboardData(ClipboardChunk { id, index, total, len: len as u8, data }))
            }
            MessageKind::ClipboardAck => {
                expect_len(p, 1)?;
                Ok(Self::ClipboardAck(p[0]))
            }
            other => Err(PayloadError::Unsupported(other)),
        }
    }

    /// Kodiert die Nachricht als kompletten Frame und gibt die geschriebene Länge zurück.
    pub fn encode_into(&self, out: &mut [u8]) -> Result<usize, PayloadError> {
        let mut buf = [0u8; MAX_TYPED_PAYLOAD];
        let len = match self {
            Self::Keepalive => 0,
            Self::MouseMove(m) => {
                buf[0..2].copy_from_slice(&m.dx.to_be_bytes());
                buf[2..4].copy_from_slice(&m.dy.to_be_bytes());
                4
            }
            Self::MouseWheel(w) => {
                buf[0..2].copy_from_slice(&w.dx.to_be_bytes());
                buf[2..4].copy_from_slice(&w.dy.to_be_bytes());
                4
            }
            Self::MouseButton(b) => {
                buf[0] = b.button as u8;
                buf[1] = u8::from(b.pressed);
                2
            }
            Self::KeyDown(k) | Self::KeyUp(k) => {
                if k.code > KEY_MAX {
                    return Err(PayloadError::InvalidValue);
                }
                buf[0..2].copy_from_slice(&k.code.to_be_bytes());
                2
            }
            Self::SwitchRequest(s) => {
                buf[0] = s.target;
                buf[1] = s.edge as u8;
                buf[2..4].copy_from_slice(&s.position.to_be_bytes());
                4
            }
            Self::SwitchConfirm(c) => {
                buf[0] = c.target;
                buf[1] = u8::from(c.accepted);
                2
            }
            Self::ClipboardData(c) => {
                let len = c.len as usize;
                if c.total == 0 || c.index >= c.total || len > CLIPBOARD_CHUNK_LEN {
                    return Err(PayloadError::InvalidValue);
                }
                buf[..4].copy_from_slice(&[c.id, c.index, c.total, c.len]);
                buf[4..4 + len].copy_from_slice(&c.data[..len]);
                4 + len
            }
            Self::ClipboardAck(id) => {
                buf[0] = *id;
                1
            }
        };
        Ok(encode_into(self.kind(), &buf[..len], out)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{decode, HEADER_LEN};

    fn roundtrip(msg: Message) {
        let mut buf = [0u8; 16];
        let n = msg.encode_into(&mut buf).unwrap();
        let frame = decode(&buf[..n]).unwrap();
        assert_eq!(Message::parse(&frame), Ok(msg));
    }

    fn parse_raw(kind: MessageKind, payload: &[u8]) -> Result<Message, PayloadError> {
        Message::parse(&Frame { kind, payload })
    }

    #[test]
    fn all_messages_roundtrip() {
        roundtrip(Message::Keepalive);
        roundtrip(Message::MouseMove(MouseMove { dx: -1, dy: 300 }));
        roundtrip(Message::MouseMove(MouseMove { dx: i16::MIN, dy: i16::MAX }));
        roundtrip(Message::MouseWheel(MouseWheel { dx: 0, dy: -120 }));
        roundtrip(Message::MouseButton(MouseButton { button: Button::Forward, pressed: true }));
        roundtrip(Message::KeyDown(Key { code: 30 }));
        roundtrip(Message::KeyUp(Key { code: KEY_MAX }));
        roundtrip(Message::SwitchRequest(SwitchRequest { target: 2, edge: Edge::Bottom, position: 65535 }));
        roundtrip(Message::SwitchConfirm(SwitchConfirm { target: 2, accepted: false }));
    }

    #[test]
    fn mouse_move_wire_layout() {
        let mut buf = [0u8; 8];
        let n = Message::MouseMove(MouseMove { dx: 0x0102, dy: -2 }).encode_into(&mut buf).unwrap();
        assert_eq!(&buf[..n], &[crate::PROTOCOL_VERSION, 4, 0, 4, 0x01, 0x02, 0xFF, 0xFE]);
    }

    #[test]
    fn rejects_wrong_payload_lengths() {
        for len in [0usize, 1, 3, 5] {
            assert!(matches!(
                parse_raw(MessageKind::MouseMove, &[0u8; 5][..len]),
                Err(PayloadError::Length { expected: 4, .. })
            ));
        }
        assert!(parse_raw(MessageKind::Keepalive, &[0]).is_err());
        assert!(parse_raw(MessageKind::KeyDown, &[0]).is_err());
        assert!(parse_raw(MessageKind::KeyDown, &[0, 1, 2]).is_err());
    }

    #[test]
    fn rejects_invalid_values() {
        assert_eq!(parse_raw(MessageKind::MouseButton, &[0, 1]), Err(PayloadError::InvalidValue));
        assert_eq!(parse_raw(MessageKind::MouseButton, &[6, 1]), Err(PayloadError::InvalidValue));
        assert_eq!(parse_raw(MessageKind::MouseButton, &[1, 2]), Err(PayloadError::InvalidValue));
        assert_eq!(parse_raw(MessageKind::SwitchRequest, &[1, 4, 0, 0]), Err(PayloadError::InvalidValue));
        assert_eq!(parse_raw(MessageKind::SwitchConfirm, &[1, 2]), Err(PayloadError::InvalidValue));
        let too_big = (KEY_MAX + 1).to_be_bytes();
        assert_eq!(parse_raw(MessageKind::KeyUp, &too_big), Err(PayloadError::InvalidValue));
        let mut buf = [0u8; 16];
        assert_eq!(
            Message::KeyDown(Key { code: KEY_MAX + 1 }).encode_into(&mut buf),
            Err(PayloadError::InvalidValue)
        );
    }

    #[test]
    fn untyped_kinds_are_unsupported() {
        assert_eq!(
            parse_raw(MessageKind::Hello, &[]),
            Err(PayloadError::Unsupported(MessageKind::Hello))
        );
    }

    #[test]
    fn encode_reports_small_buffer() {
        let mut tiny = [0u8; HEADER_LEN];
        assert_eq!(
            Message::MouseMove(MouseMove { dx: 1, dy: 1 }).encode_into(&mut tiny),
            Err(PayloadError::Frame(FrameError::BufferTooSmall))
        );
    }

    #[test]
    fn arbitrary_input_never_panics() {
        let mut state = 0x9E37_79B9_7F4A_7C15_u64;
        let mut next = || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        for _ in 0..50_000 {
            let len = (next() % 12) as usize;
            let mut bytes = [0u8; 12];
            for b in bytes.iter_mut() {
                *b = next() as u8;
            }
            if let Ok(frame) = decode(&bytes[..len]) {
                let _ = Message::parse(&frame);
            }
            // Häufig gültige Header erzeugen, damit auch Payload-Pfade getroffen werden.
            bytes[0] = crate::PROTOCOL_VERSION;
            bytes[1] = (next() % 17) as u8;
            let payload = (next() % 6) as usize;
            bytes[2] = 0;
            bytes[3] = payload as u8;
            if let Ok(frame) = decode(&bytes[..HEADER_LEN + payload]) {
                let _ = Message::parse(&frame);
            }
        }
    }
}
