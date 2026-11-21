//! Abfangen lokaler Eingaben am Bildschirmrand über das InputCapture-Portal.

use std::{
    num::NonZeroU32,
    os::unix::net::UnixStream,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
};

use ashpd::desktop::input_capture::{
    Activated, Barrier, BarrierID, BarrierPosition, Capabilities, CreateSessionOptions, InputCapture, ReleaseOptions, Zones,
};
use futures_util::StreamExt;
use protocol::Edge;
use reis::{
    ei::{self, keyboard::KeyState},
    event::{DeviceCapability, EiEvent},
};
use tokio::sync::{mpsc, watch};

use crate::settings::Settings;

type AnyResult<T> = Result<T, Box<dyn std::error::Error>>;

pub enum CaptureEvent {
    /// Größe der Zone an der konfigurierten Kante.
    Ready { width: i32, height: i32 },
    Failed(String),
    /// Zeiger hat die Kante erreicht; `position` liegt normiert (0..=65535) entlang der Kante.
    Activated { position: u16 },
    Motion(f32, f32),
    Button { code: u32, pressed: bool },
    /// Einheiten wie bei libei: 120 pro Rasterstufe.
    Wheel { dx: i32, dy: i32 },
    Key { code: u32, pressed: bool },
}

pub enum CaptureCmd {
    /// Gibt die Eingaben frei; `position` setzt den Zeiger entlang der Kante, sonst bleibt er an der Aktivierungsstelle.
    Release { position: Option<u16> },
}

struct Zone {
    x: i32,
    y: i32,
    w: i32,
    h: i32,
}

impl Zone {
    fn fraction(&self, edge: Edge, cursor: (f32, f32)) -> u16 {
        let (value, origin, len) = match edge {
            Edge::Left | Edge::Right => (cursor.1, self.y, self.h),
            Edge::Top | Edge::Bottom => (cursor.0, self.x, self.w),
        };
        let t = (value - origin as f32) / (len - 1).max(1) as f32;
        (t.clamp(0.0, 1.0) * 65535.0) as u16
    }

    // Zeiger liegt nach der Freigabe zwei Pixel innerhalb der Kante, damit die Barriere nicht sofort wieder auslöst.
    fn point(&self, edge: Edge, position: u16) -> (f64, f64) {
        let t = f64::from(position) / 65535.0;
        let along_y = f64::from(self.y) + t * f64::from((self.h - 1).max(0));
        let along_x = f64::from(self.x) + t * f64::from((self.w - 1).max(0));
        match edge {
            Edge::Right => (f64::from(self.x + self.w - 2), along_y),
            Edge::Left => (f64::from(self.x + 1), along_y),
            Edge::Top => (along_x, f64::from(self.y + 1)),
            Edge::Bottom => (along_x, f64::from(self.y + self.h - 2)),
        }
    }

    fn barrier(&self, edge: Edge) -> BarrierPosition {
        let (x, y, w, h) = (self.x, self.y, self.w, self.h);
        match edge {
            Edge::Right => BarrierPosition::new(x + w, y, x + w, y + h - 1),
            Edge::Left => BarrierPosition::new(x, y, x, y + h - 1),
            Edge::Top => BarrierPosition::new(x, y, x + w - 1, y),
            Edge::Bottom => BarrierPosition::new(x, y + h, x + w - 1, y + h),
        }
    }
}

/// Läuft als eigene Task. Solange `connected` nicht gesetzt ist, wird jede Aktivierung sofort freigegeben.
pub async fn run(
    connected: Arc<AtomicBool>,
    tx: mpsc::Sender<CaptureEvent>,
    mut cmds: mpsc::Receiver<CaptureCmd>,
    settings: watch::Receiver<Settings>,
) {
    if let Err(e) = capture(&connected, &tx, &mut cmds, settings).await {
        let _ = tx.send(CaptureEvent::Failed(e.to_string())).await;
    }
}

async fn capture(
    connected: &AtomicBool,
    tx: &mpsc::Sender<CaptureEvent>,
    cmds: &mut mpsc::Receiver<CaptureCmd>,
    mut settings: watch::Receiver<Settings>,
) -> AnyResult<()> {
    let input_capture = InputCapture::new().await?;
    let (session, _granted) = input_capture
        .create_session(
            None,
            CreateSessionOptions::default().set_capabilities(Capabilities::Keyboard | Capabilities::Pointer),
        )
        .await?;

    let fd = input_capture.connect_to_eis(&session, Default::default()).await?;
    let stream = UnixStream::from(fd);
    stream.set_nonblocking(true)?;
    let context = ei::Context::new(stream)?;
    context.flush()?;
    let (_connection, mut events) =
        context.handshake_tokio("shared-control", ei::handshake::ContextType::Receiver).await?;

    let zones = input_capture.zones(&session, Default::default()).await?.response()?;
    let edge = settings.borrow().edge;
    let mut current = apply_barrier(&input_capture, &session, &zones, edge).await?;

    let mut activated_stream = input_capture.receive_activated().await?;
    input_capture.enable(&session, Default::default()).await?;
    tx.send(ready_event(&current)).await?;

    let mut active: Option<(Activated, u16)> = None;
    let mut saw_discrete = false;

    loop {
        tokio::select! {
            Some(a) = activated_stream.next() => {
                let Some((edge, zone)) = &current else { continue };
                let cursor = a.cursor_position().unwrap_or((0.0, 0.0));
                let position = zone.fraction(*edge, cursor);
                if connected.load(Ordering::Relaxed) {
                    active = Some((a, position));
                    tx.send(CaptureEvent::Activated { position }).await?;
                } else {
                    release(&input_capture, &session, &a, zone, *edge, position).await?;
                }
            }
            ev = events.next() => {
                let Some(ev) = ev else { return Err("EI-Verbindung beendet".into()) };
                match ev? {
                    EiEvent::SeatAdded(seat) => {
                        seat.seat.bind_capabilities(
                            DeviceCapability::Pointer
                                | DeviceCapability::PointerAbsolute
                                | DeviceCapability::Keyboard
                                | DeviceCapability::Scroll
                                | DeviceCapability::Button,
                        );
                        context.flush()?;
                    }
                    EiEvent::PointerMotion(m) if active.is_some() => {
                        tx.send(CaptureEvent::Motion(m.dx, m.dy)).await?;
                    }
                    EiEvent::Button(b) if active.is_some() => {
                        tx.send(CaptureEvent::Button { code: b.button, pressed: b.state == ei::button::ButtonState::Press }).await?;
                    }
                    EiEvent::ScrollDiscrete(s) if active.is_some() => {
                        saw_discrete = true;
                        tx.send(CaptureEvent::Wheel { dx: s.discrete_dx, dy: s.discrete_dy }).await?;
                    }
                    EiEvent::ScrollDelta(s) if active.is_some() && !saw_discrete => {
                        tx.send(CaptureEvent::Wheel { dx: (s.dx * 8.0) as i32, dy: (s.dy * 8.0) as i32 }).await?;
                    }
                    EiEvent::KeyboardKey(k) if active.is_some() => {
                        tx.send(CaptureEvent::Key { code: k.key, pressed: k.state == KeyState::Press }).await?;
                    }
                    _ => {}
                }
            }
            cmd = cmds.recv() => {
                let Some(CaptureCmd::Release { position }) = cmd else { return Ok(()) };
                if let (Some((activation, own)), Some((edge, zone))) = (active.take(), &current) {
                    release(&input_capture, &session, &activation, zone, *edge, position.unwrap_or(own)).await?;
                }
            }
            changed = settings.changed() => {
                if changed.is_err() {
                    return Ok(());
                }
                let edge = settings.borrow_and_update().edge;
                if edge == current.as_ref().map(|(e, _)| *e) {
                    continue;
                }
                if let (Some((activation, own)), Some((old_edge, zone))) = (active.take(), &current) {
                    release(&input_capture, &session, &activation, zone, *old_edge, own).await?;
                }
                input_capture.disable(&session, Default::default()).await?;
                current = apply_barrier(&input_capture, &session, &zones, edge).await?;
                input_capture.enable(&session, Default::default()).await?;
                tx.send(ready_event(&current)).await?;
            }
        }
    }
}

fn ready_event(current: &Option<(Edge, Zone)>) -> CaptureEvent {
    let (width, height) = current.as_ref().map_or((0, 0), |(_, zone)| (zone.w, zone.h));
    CaptureEvent::Ready { width, height }
}

/// Setzt die Barriere an der gewählten Kante; ohne Kante gibt es keine Barriere.
async fn apply_barrier<S>(
    input_capture: &InputCapture,
    session: &S,
    zones: &Zones,
    edge: Option<Edge>,
) -> AnyResult<Option<(Edge, Zone)>>
where
    S: std::borrow::Borrow<ashpd::desktop::Session<InputCapture>>,
{
    let Some(edge) = edge else {
        input_capture.set_pointer_barriers(session.borrow(), &[], zones.zone_set(), Default::default()).await?.response()?;
        return Ok(None);
    };
    let regions = zones.regions();
    let region = match edge {
        Edge::Right => regions.iter().max_by_key(|r| r.x_offset() + r.width() as i32),
        Edge::Left => regions.iter().min_by_key(|r| r.x_offset()),
        Edge::Top => regions.iter().min_by_key(|r| r.y_offset()),
        Edge::Bottom => regions.iter().max_by_key(|r| r.y_offset() + r.height() as i32),
    }
    .ok_or("No screen zones reported")?;
    let zone = Zone { x: region.x_offset(), y: region.y_offset(), w: region.width() as i32, h: region.height() as i32 };

    let barrier = Barrier::new(BarrierID::new(1).unwrap_or(NonZeroU32::MIN), zone.barrier(edge));
    let response = input_capture
        .set_pointer_barriers(session.borrow(), &[barrier], zones.zone_set(), Default::default())
        .await?
        .response()?;
    if !response.failed_barriers().is_empty() {
        return Err("The compositor rejected the screen edge".into());
    }
    Ok(Some((edge, zone)))
}

async fn release<S>(
    input_capture: &InputCapture,
    session: &S,
    activation: &Activated,
    zone: &Zone,
    edge: Edge,
    position: u16,
) -> AnyResult<()>
where
    S: std::borrow::Borrow<ashpd::desktop::Session<InputCapture>>,
{
    input_capture
        .release(
            session.borrow(),
            ReleaseOptions::default()
                .set_activation_id(activation.activation_id())
                .set_cursor_position(zone.point(edge, position)),
        )
        .await?;
    Ok(())
}
