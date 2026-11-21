//! Phase-0-Prototyp: Eingaben am rechten Bildschirmrand über das InputCapture-Portal abfangen.
//!
//! Zählt nur Ereignisse (keine Tastencodes, keine Inhalte). Gibt die Eingaben bei Esc oder
//! spätestens nach MAX_CAPTURE wieder frei.

use std::{
    num::NonZeroU32,
    os::unix::net::UnixStream,
    time::{Duration, Instant},
};

use ashpd::desktop::input_capture::{
    Activated, Barrier, BarrierID, BarrierPosition, Capabilities, CreateSessionOptions, InputCapture,
    ReleaseOptions,
};
use futures_util::StreamExt;
use reis::{
    ei::{self, keyboard::KeyState},
    event::{DeviceCapability, EiEvent},
};

type AnyResult<T> = Result<T, Box<dyn std::error::Error>>;

const KEY_ESC: u32 = 1;
const MAX_CAPTURE: Duration = Duration::from_secs(15);
const MAX_WAIT: Duration = Duration::from_secs(120);

#[derive(Default)]
struct Stats {
    motion_events: u32,
    dx: f64,
    dy: f64,
    button_events: u32,
    scroll_events: u32,
    key_events: u32,
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> AnyResult<()> {
    let input_capture = InputCapture::new().await?;
    println!("Fordere Zugriff über das InputCapture-Portal an. Bitte den KDE-Dialog bestätigen ...");
    let (session, granted) = input_capture
        .create_session(
            None,
            CreateSessionOptions::default().set_capabilities(Capabilities::Keyboard | Capabilities::Pointer),
        )
        .await?;
    println!("Freigegebene Fähigkeiten: {granted:?}");

    let fd = input_capture.connect_to_eis(&session, Default::default()).await?;
    let stream = UnixStream::from(fd);
    stream.set_nonblocking(true)?;
    let context = ei::Context::new(stream)?;
    context.flush()?;
    let (_connection, mut events) = context
        .handshake_tokio("shared-control-capture-poc", ei::handshake::ContextType::Receiver)
        .await?;

    let zones = input_capture.zones(&session, Default::default()).await?.response()?;
    println!("Zonen: {:?}", zones.regions());
    let Some(region) = zones.regions().iter().max_by_key(|r| r.x_offset() + r.width() as i32) else {
        return Err("Keine Zonen gemeldet".into());
    };
    let (x, y) = (region.x_offset(), region.y_offset());
    let (w, h) = (region.width() as i32, region.height() as i32);
    let barrier = Barrier::new(
        BarrierID::new(1).unwrap_or(NonZeroU32::MIN),
        BarrierPosition::new(x + w, y, x + w, y + h - 1),
    );
    let response = input_capture
        .set_pointer_barriers(&session, &[barrier], zones.zone_set(), Default::default())
        .await?
        .response()?;
    println!("Abgelehnte Barrieren: {:?}", response.failed_barriers());

    let mut activated_stream = input_capture.receive_activated().await?;
    input_capture.enable(&session, Default::default()).await?;
    println!(
        "Aktiv. Bewege den Mauszeiger an den RECHTEN Rand von Zone {:?}. Esc gibt frei, \
         automatische Freigabe nach {} s.",
        (x, y, w, h),
        MAX_CAPTURE.as_secs()
    );

    let started = Instant::now();
    let mut tick = tokio::time::interval(Duration::from_millis(200));
    let mut active: Option<(Activated, Instant)> = None;
    let mut stats = Stats::default();
    let mut esc = false;

    loop {
        tokio::select! {
            Some(a) = activated_stream.next() => {
                println!("Eingaben werden abgefangen (Aktivierung {:?}).", a.activation_id());
                stats = Stats::default();
                esc = false;
                active = Some((a, Instant::now()));
            }
            ev = events.next() => {
                let Some(ev) = ev else { println!("EI-Verbindung beendet."); break };
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
                        stats.motion_events += 1;
                        stats.dx += f64::from(m.dx);
                        stats.dy += f64::from(m.dy);
                    }
                    EiEvent::Button(_) if active.is_some() => stats.button_events += 1,
                    EiEvent::ScrollDelta(_) | EiEvent::ScrollDiscrete(_) if active.is_some() => {
                        stats.scroll_events += 1;
                    }
                    EiEvent::KeyboardKey(k) if active.is_some() => {
                        stats.key_events += 1;
                        if k.key == KEY_ESC && k.state == KeyState::Press {
                            esc = true;
                        }
                    }
                    _ => {}
                }
            }
            _ = tick.tick() => {}
        }

        if let Some((activation, since)) = &active {
            if esc || since.elapsed() >= MAX_CAPTURE {
                let (cx, cy) = activation.cursor_position().unwrap_or((x as f32 + w as f32, y as f32));
                input_capture
                    .release(
                        &session,
                        ReleaseOptions::default()
                            .set_activation_id(activation.activation_id())
                            .set_cursor_position((f64::from(cx) - 1.0, f64::from(cy))),
                    )
                    .await?;
                println!(
                    "Freigegeben ({}). Zeigerereignisse: {}, Summe dx/dy: {:.0}/{:.0}, Tasten: {}, Klicks: {}, Scrollen: {}",
                    if esc { "Esc" } else { "Zeitlimit" },
                    stats.motion_events, stats.dx, stats.dy, stats.key_events, stats.button_events, stats.scroll_events
                );
                break;
            }
        } else if started.elapsed() >= MAX_WAIT {
            println!("Keine Aktivierung innerhalb von {} s.", MAX_WAIT.as_secs());
            break;
        }
    }

    let _ = input_capture.disable(&session, Default::default()).await;
    println!("Fertig.");
    Ok(())
}
