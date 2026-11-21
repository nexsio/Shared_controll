//! Betrieb einer aufgebauten Verbindung: Umschalten, Eingaben weiterleiten, Zwischenablage abgleichen.

use std::{
    future::pending,
    sync::{atomic::Ordering, Arc},
    time::{Duration, Instant},
};

use protocol::{
    Button, ClipboardChunk, Edge, Key, Message, MouseButton, MouseMove, MouseWheel, SwitchConfirm, SwitchRequest,
    CLIPBOARD_CHUNK_LEN, KEY_MAX,
};
use security::{SecureChannel, SecurityError};
use tokio::{
    sync::mpsc,
    time::{interval, MissedTickBehavior},
};
use transport_dongle::UsbDongleTransport;

use crate::{
    capture::{CaptureCmd, CaptureEvent},
    clipboard,
    injector::Injector,
    tray::{Ui, UiCmd},
    Options, Role,
};

const KEY_SCROLLLOCK: u32 = 70;
const BTN_LEFT: u32 = 0x110;
const BTN_RIGHT: u32 = 0x111;
const BTN_MIDDLE: u32 = 0x112;
const BTN_SIDE: u32 = 0x113;
const BTN_EXTRA: u32 = 0x114;

const PEER_TIMEOUT: Duration = Duration::from_secs(4);
// Der Partner kann beim Pairing lange auf die Bestätigung des Benutzers warten.
const FIRST_CONTACT_TIMEOUT: Duration = Duration::from_secs(150);
// Direkt nach dem Betreten erzeugt die Platzierung des Zeigers selbst Randkontakt; der Rückwechsel wird so lange ignoriert.
const ENTRY_GRACE: Duration = Duration::from_millis(400);
const ENTRY_INSET: f32 = 20.0;
const CLIPBOARD_RETRY: Duration = Duration::from_millis(1200);
const CLIPBOARD_TRIES: u8 = 3;

#[derive(Debug)]
pub enum Ended {
    /// Verbindung zum Partner endete; neu aufbauen.
    Link(String),
    /// Programm kann nicht sinnvoll weiterlaufen.
    Fatal(String),
    /// Kein Dongle (mehr) angesteckt.
    NoDongle,
    /// Vom Tray angefordert: Pairing erzwingen, Partner vergessen, beenden.
    Repair,
    Forget,
    Quit,
}

/// Lokale Ein- und Ausgabe, die über Verbindungen hinweg bestehen bleibt.
pub struct Local {
    /// Nur beim Node vorhanden.
    pub injector: Option<Injector>,
    pub events: mpsc::Receiver<CaptureEvent>,
    pub cmd: mpsc::Sender<CaptureCmd>,
    pub connected: Arc<std::sync::atomic::AtomicBool>,
    /// Größe der Zone an der konfigurierten Kante.
    pub zone: (i32, i32),
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    /// Dieser Rechner nutzt seine Eingaben selbst.
    Local,
    /// Lokale Eingaben werden abgefangen und zum Partner geschickt.
    Capturing,
    /// Der Partner steuert diesen Rechner.
    Controlled { since: Instant },
}

struct Outgoing {
    id: u8,
    chunks: Vec<ClipboardChunk>,
    tries: u8,
    deadline: Instant,
}

struct Incoming {
    id: u8,
    parts: Vec<Option<Vec<u8>>>,
}

struct ClipState {
    enabled: bool,
    watcher: Option<clipboard::Watcher>,
    started: Instant,
    next_id: u8,
    last_synced: Option<String>,
    outgoing: Option<Outgoing>,
    incoming: Option<Incoming>,
    completed_id: Option<u8>,
}

pub struct Session<'a> {
    channel: SecureChannel<UsbDongleTransport>,
    opts: &'a Options,
    local: &'a mut Local,
    ui: &'a mut Ui,
    mode: Mode,
    motion: (f32, f32),
    my_seq: u8,
    peer_seq: Option<u8>,
    clip: ClipState,
}

fn button_from_code(code: u32) -> Option<Button> {
    Some(match code {
        BTN_LEFT => Button::Left,
        BTN_RIGHT => Button::Right,
        BTN_MIDDLE => Button::Middle,
        BTN_SIDE => Button::Back,
        BTN_EXTRA => Button::Forward,
        _ => return None,
    })
}

fn code_from_button(button: Button) -> u32 {
    match button {
        Button::Left => BTN_LEFT,
        Button::Right => BTN_RIGHT,
        Button::Middle => BTN_MIDDLE,
        Button::Back => BTN_SIDE,
        Button::Forward => BTN_EXTRA,
    }
}

fn clamp_i16(v: i32) -> i16 {
    v.clamp(i16::MIN.into(), i16::MAX.into()) as i16
}

fn start_watcher() -> Option<clipboard::Watcher> {
    clipboard::Watcher::start().map_err(|e| eprintln!("Clipboard unavailable: {e}")).ok()
}

impl<'a> Session<'a> {
    pub fn new(channel: SecureChannel<UsbDongleTransport>, opts: &'a Options, local: &'a mut Local, ui: &'a mut Ui) -> Self {
        let enabled = ui.settings.borrow().clipboard;
        let watcher = if enabled { start_watcher() } else { None };
        Self {
            channel,
            opts,
            local,
            ui,
            mode: Mode::Local,
            motion: (0.0, 0.0),
            my_seq: 0,
            peer_seq: None,
            clip: ClipState {
                enabled,
                watcher,
                started: Instant::now(),
                next_id: 0,
                last_synced: None,
                outgoing: None,
                incoming: None,
                completed_id: None,
            },
        }
    }

    pub async fn run(mut self) -> Ended {
        // Der Master fängt ab Verbindungsaufbau an der Kante ab, der Node erst, wenn er gesteuert wird.
        self.local.connected.store(self.opts.role == Role::Master, Ordering::Relaxed);
        let ended = self.event_loop().await;
        self.cleanup().await;
        ended
    }

    fn inject(&self, f: impl FnOnce(&Injector)) {
        if let Some(injector) = &self.local.injector {
            f(injector);
        }
    }

    fn set_mode(&mut self, mode: Mode) {
        self.mode = mode;
        if self.opts.role == Role::Node {
            self.local.connected.store(matches!(mode, Mode::Controlled { .. }), Ordering::Relaxed);
        }
    }

    async fn cleanup(&mut self) {
        self.local.connected.store(false, Ordering::Relaxed);
        self.inject(Injector::end);
        if self.mode == Mode::Capturing {
            let _ = self.local.cmd.send(CaptureCmd::Release { position: None }).await;
        }
        let _ = self.channel.transport_mut().set_authenticated(false).await;
    }

    async fn event_loop(&mut self) -> Ended {
        let mut motion_tick = interval(Duration::from_millis(8));
        let mut keepalive_tick = interval(Duration::from_secs(1));
        let mut clipboard_tick = interval(Duration::from_millis(250));
        for tick in [&mut motion_tick, &mut keepalive_tick, &mut clipboard_tick] {
            tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
        }
        let mut last_heard = Instant::now();
        let mut heard_any = false;
        let mut invalid_frames = 0u32;
        let mut last_invalid = String::new();
        let mut reports_at_heard = self.channel.transport_mut().reports_received();
        let mut last_tick = Instant::now();

        loop {
            let step = tokio::select! {
                received = self.channel.receive() => match received {
                    Ok(message) => {
                        last_heard = Instant::now();
                        reports_at_heard = self.channel.transport_mut().reports_received();
                        heard_any = true;
                        self.on_peer(message).await
                    }
                    Err(SecurityError::Transport(e)) => Err(Ended::Link(e.to_string())),
                    // Einzelne ungültige oder wiederholte Pakete werden verworfen.
                    Err(e) => {
                        invalid_frames += 1;
                        last_invalid = e.to_string();
                        Ok(())
                    }
                },
                event = self.local.events.recv() => match event {
                    Some(event) => self.on_local(event).await,
                    None => Err(Ended::Fatal("Eingabeabfang beendet".into())),
                },
                text = async {
                    match self.clip.watcher.as_mut() {
                        Some(watcher) => watcher.next().await,
                        None => pending().await,
                    }
                } => match text {
                    Some(text) => self.on_local_clipboard(text).await,
                    None => {
                        self.clip.watcher = None;
                        eprintln!("Zwischenablage-Überwachung beendet");
                        Ok(())
                    }
                },
                _ = motion_tick.tick() => self.flush_motion().await,
                _ = clipboard_tick.tick() => self.clipboard_retry().await,
                cmd = self.ui.cmds.recv() => match cmd {
                    Some(cmd) => self.on_ui(cmd).await,
                    None => pending().await,
                },
                changed = self.ui.settings.changed() => {
                    if changed.is_ok() {
                        self.apply_settings();
                    }
                    Ok(())
                }
                _ = keepalive_tick.tick() => {
                    let gap = last_tick.elapsed();
                    last_tick = Instant::now();
                    if gap > Duration::from_millis(1500) {
                        eprintln!("Event loop stalled for {} ms", gap.as_millis());
                    }
                    let limit = if heard_any { PEER_TIMEOUT } else { FIRST_CONTACT_TIMEOUT };
                    if last_heard.elapsed() > limit {
                        let silent = last_heard.elapsed().as_secs();
                        let reports = self.channel.transport_mut().reports_received().wrapping_sub(reports_at_heard);
                        Err(Ended::Link(format!(
                            "peer silent for {silent}s ({reports} dongle reports, {invalid_frames} invalid frames, last: {last_invalid:?})"
                        )))
                    } else if self.local.injector.as_ref().is_some_and(|i| !i.alive()) {
                        Err(Ended::Fatal("Compositor hat die Eingabeverbindung beendet".into()))
                    } else {
                        self.send(Message::Keepalive).await
                    }
                }
            };
            if let Err(ended) = step {
                return ended;
            }
        }
    }

    async fn send(&mut self, message: Message) -> Result<(), Ended> {
        match self.channel.send(&message).await {
            Err(SecurityError::Transport(e)) => Err(Ended::Link(e.to_string())),
            _ => Ok(()),
        }
    }

    // Der Funk verliert gelegentlich Pakete; Wiederholungen sind unschädlich, weil der Empfänger Zustände setzt.
    async fn send_repeated(&mut self, message: Message, times: usize) -> Result<(), Ended> {
        for _ in 0..times {
            self.send(message).await?;
        }
        Ok(())
    }

    fn next_seq(&mut self) -> u8 {
        self.my_seq = self.my_seq.wrapping_add(1);
        self.my_seq
    }

    async fn flush_motion(&mut self) -> Result<(), Ended> {
        if self.mode != Mode::Capturing {
            return Ok(());
        }
        let (dx, dy) = (self.motion.0.trunc(), self.motion.1.trunc());
        if dx == 0.0 && dy == 0.0 {
            return Ok(());
        }
        self.motion.0 -= dx;
        self.motion.1 -= dy;
        self.send(Message::MouseMove(MouseMove { dx: clamp_i16(dx as i32), dy: clamp_i16(dy as i32) })).await
    }

    async fn release_capture(&mut self, position: Option<u16>) {
        let _ = self.local.cmd.send(CaptureCmd::Release { position }).await;
    }

    async fn on_ui(&mut self, cmd: UiCmd) -> Result<(), Ended> {
        match cmd {
            UiCmd::Back if self.mode == Mode::Capturing => self.end_remote_control().await,
            UiCmd::Back => Ok(()),
            UiCmd::Reconnect => Err(Ended::Link("reconnect requested".into())),
            UiCmd::Repair => Err(Ended::Repair),
            UiCmd::Forget => Err(Ended::Forget),
            UiCmd::Quit => Err(Ended::Quit),
        }
    }

    fn apply_settings(&mut self) {
        let enabled = self.ui.settings.borrow_and_update().clipboard;
        if enabled == self.clip.enabled {
            return;
        }
        self.clip.enabled = enabled;
        if enabled {
            self.clip.started = Instant::now();
            self.clip.watcher = start_watcher();
        } else {
            self.clip.watcher = None;
            self.clip.outgoing = None;
            self.clip.incoming = None;
        }
    }

    fn edge(&self) -> Option<Edge> {
        self.ui.settings.borrow().edge
    }

    async fn on_local(&mut self, event: CaptureEvent) -> Result<(), Ended> {
        match event {
            CaptureEvent::Failed(e) => Err(Ended::Fatal(format!("Eingabeabfang: {e}"))),
            CaptureEvent::Ready { width, height } => {
                self.local.zone = (width, height);
                Ok(())
            }
            CaptureEvent::Activated { position } => match self.mode {
                Mode::Local if self.opts.role == Role::Master => {
                    self.motion = (0.0, 0.0);
                    self.mode = Mode::Capturing;
                    let seq = self.next_seq();
                    let request = SwitchRequest { target: seq, edge: self.edge().unwrap_or(Edge::Right), position };
                    self.send_repeated(Message::SwitchRequest(request), 3).await
                }
                Mode::Controlled { since } => {
                    self.release_capture(None).await;
                    if since.elapsed() < ENTRY_GRACE {
                        return Ok(());
                    }
                    self.inject(Injector::end);
                    self.set_mode(Mode::Local);
                    let seq = self.next_seq();
                    let request = SwitchRequest { target: seq, edge: self.edge().unwrap_or(Edge::Right), position };
                    self.send_repeated(Message::SwitchRequest(request), 3).await
                }
                Mode::Local | Mode::Capturing => Ok(()),
            },
            CaptureEvent::Motion(dx, dy) if self.mode == Mode::Capturing => {
                self.motion.0 += dx;
                self.motion.1 += dy;
                Ok(())
            }
            CaptureEvent::Button { code, pressed } if self.mode == Mode::Capturing => {
                let Some(button) = button_from_code(code) else { return Ok(()) };
                self.flush_motion().await?;
                self.send_repeated(Message::MouseButton(MouseButton { button, pressed }), 2).await
            }
            CaptureEvent::Wheel { dx, dy } if self.mode == Mode::Capturing => {
                self.flush_motion().await?;
                self.send(Message::MouseWheel(MouseWheel { dx: clamp_i16(dx), dy: clamp_i16(dy) })).await
            }
            CaptureEvent::Key { code, pressed } if self.mode == Mode::Capturing => {
                if pressed && code == KEY_SCROLLLOCK {
                    return self.end_remote_control().await;
                }
                let Ok(code) = u16::try_from(code) else { return Ok(()) };
                if code > KEY_MAX {
                    return Ok(());
                }
                self.flush_motion().await?;
                let key = Key { code };
                self.send_repeated(if pressed { Message::KeyDown(key) } else { Message::KeyUp(key) }, 2).await
            }
            _ => Ok(()),
        }
    }

    /// Notausgang per Scroll-Lock: lokale Steuerung zurück und dem Partner das Ende melden.
    async fn end_remote_control(&mut self) -> Result<(), Ended> {
        self.release_capture(None).await;
        self.mode = Mode::Local;
        let seq = self.next_seq();
        self.send_repeated(Message::SwitchConfirm(SwitchConfirm { target: seq, accepted: false }), 3).await
    }

    async fn on_peer(&mut self, message: Message) -> Result<(), Ended> {
        let controlled = matches!(self.mode, Mode::Controlled { .. });
        match message {
            Message::Keepalive => {}
            Message::MouseMove(m) if controlled => self.inject(|i| i.motion(f32::from(m.dx), f32::from(m.dy))),
            Message::MouseWheel(w) if controlled => self.inject(|i| i.wheel(i32::from(w.dx), i32::from(w.dy))),
            Message::MouseButton(b) if controlled => {
                self.inject(|i| i.button(code_from_button(b.button), b.pressed));
            }
            Message::KeyDown(k) if controlled => self.inject(|i| i.key(u32::from(k.code), true)),
            Message::KeyUp(k) if controlled => self.inject(|i| i.key(u32::from(k.code), false)),
            Message::SwitchRequest(r) => {
                if self.peer_seq == Some(r.target) {
                    return Ok(());
                }
                self.peer_seq = Some(r.target);
                return self.on_switch_request(r).await;
            }
            Message::SwitchConfirm(c) => {
                if self.peer_seq == Some(c.target) {
                    return Ok(());
                }
                self.peer_seq = Some(c.target);
                if !c.accepted {
                    match self.mode {
                        Mode::Capturing => {
                            self.release_capture(None).await;
                            self.mode = Mode::Local;
                        }
                        Mode::Controlled { .. } => {
                            self.inject(Injector::end);
                            self.set_mode(Mode::Local);
                        }
                        Mode::Local => {}
                    }
                }
            }
            Message::ClipboardData(chunk) => return self.on_clipboard_chunk(chunk).await,
            Message::ClipboardAck(id) => {
                if self.clip.outgoing.as_ref().is_some_and(|o| o.id == id) {
                    self.clip.outgoing = None;
                }
            }
            _ => {}
        }
        Ok(())
    }

    async fn on_switch_request(&mut self, request: SwitchRequest) -> Result<(), Ended> {
        if self.mode == Mode::Capturing {
            // Der Partner gibt uns die Steuerung zurück.
            self.release_capture(Some(request.position)).await;
            self.mode = Mode::Local;
            return Ok(());
        }
        // Der Master wird nie gesteuert.
        if self.opts.role == Role::Master {
            return Ok(());
        }
        let accepted = self.local.injector.as_ref().is_some_and(Injector::alive);
        if accepted {
            self.inject(Injector::begin);
            self.place_cursor(request.position);
            self.set_mode(Mode::Controlled { since: Instant::now() });
        }
        self.send_repeated(Message::SwitchConfirm(SwitchConfirm { target: request.target, accepted }), 2).await
    }

    // Erst an die Kante schieben (der Compositor begrenzt), dann ein Stück nach innen und entlang der Kante setzen.
    fn place_cursor(&self, position: u16) {
        let Some(inj) = &self.local.injector else { return };
        let Some(screen_edge) = self.edge() else { return };
        let t = f32::from(position) / 65535.0;
        let (w, h) = (self.local.zone.0 as f32, self.local.zone.1 as f32);
        const FAR: f32 = 100_000.0;
        match screen_edge {
            Edge::Left | Edge::Right => {
                let (edge_dx, inset) = if screen_edge == Edge::Left { (-FAR, ENTRY_INSET) } else { (FAR, -ENTRY_INSET) };
                inj.motion(edge_dx, -FAR);
                inj.motion(inset, t * h);
            }
            Edge::Top | Edge::Bottom => {
                let (edge_dy, inset) = if screen_edge == Edge::Top { (-FAR, ENTRY_INSET) } else { (FAR, -ENTRY_INSET) };
                inj.motion(-FAR, edge_dy);
                inj.motion(t * w, inset);
            }
        }
    }

    async fn on_local_clipboard(&mut self, text: String) -> Result<(), Ended> {
        // Der Beobachter meldet beim Start den vorhandenen Inhalt; der soll nicht automatisch übertragen werden.
        if self.clip.started.elapsed() < Duration::from_millis(500) {
            self.clip.last_synced = Some(text);
            return Ok(());
        }
        if text.len() > clipboard::MAX_TEXT_LEN || self.clip.last_synced.as_deref() == Some(text.as_str()) {
            return Ok(());
        }
        self.clip.next_id = self.clip.next_id.wrapping_add(1);
        let id = self.clip.next_id;
        let pieces: Vec<&[u8]> = text.as_bytes().chunks(CLIPBOARD_CHUNK_LEN).collect();
        let total = pieces.len() as u8;
        let chunks = pieces
            .iter()
            .enumerate()
            .map(|(index, piece)| {
                let mut data = [0u8; CLIPBOARD_CHUNK_LEN];
                data[..piece.len()].copy_from_slice(piece);
                ClipboardChunk { id, index: index as u8, total, len: piece.len() as u8, data }
            })
            .collect();
        self.clip.last_synced = Some(text);
        self.clip.outgoing = Some(Outgoing { id, chunks, tries: 1, deadline: Instant::now() + CLIPBOARD_RETRY });
        self.send_outgoing().await
    }

    async fn send_outgoing(&mut self) -> Result<(), Ended> {
        let chunks = self.clip.outgoing.as_ref().map(|o| o.chunks.clone()).unwrap_or_default();
        for chunk in chunks {
            self.send(Message::ClipboardData(chunk)).await?;
        }
        Ok(())
    }

    async fn clipboard_retry(&mut self) -> Result<(), Ended> {
        let Some(outgoing) = self.clip.outgoing.as_mut() else { return Ok(()) };
        if Instant::now() < outgoing.deadline {
            return Ok(());
        }
        if outgoing.tries >= CLIPBOARD_TRIES {
            self.clip.outgoing = None;
            eprintln!("Zwischenablage wurde nicht bestätigt");
            return Ok(());
        }
        outgoing.tries += 1;
        outgoing.deadline = Instant::now() + CLIPBOARD_RETRY;
        self.send_outgoing().await
    }

    async fn on_clipboard_chunk(&mut self, chunk: ClipboardChunk) -> Result<(), Ended> {
        if !self.clip.enabled {
            return Ok(());
        }
        if self.clip.completed_id == Some(chunk.id) {
            // Bestätigung ging wohl verloren.
            return self.send_repeated(Message::ClipboardAck(chunk.id), 2).await;
        }
        let total = usize::from(chunk.total);
        let incoming = match self.clip.incoming.as_mut() {
            Some(i) if i.id == chunk.id && i.parts.len() == total => i,
            _ => self.clip.incoming.insert(Incoming { id: chunk.id, parts: vec![None; total] }),
        };
        incoming.parts[usize::from(chunk.index)] = Some(chunk.data[..usize::from(chunk.len)].to_vec());
        if incoming.parts.iter().any(Option::is_none) {
            return Ok(());
        }
        let bytes: Vec<u8> = incoming.parts.iter().flatten().flatten().copied().collect();
        self.clip.incoming = None;
        self.clip.completed_id = Some(chunk.id);
        if let Ok(text) = String::from_utf8(bytes) {
            if text.len() <= clipboard::MAX_TEXT_LEN {
                self.clip.last_synced = Some(text.clone());
                if let Err(e) = clipboard::set(&text).await {
                    eprintln!("Zwischenablage konnte nicht gesetzt werden: {e}");
                }
            }
        }
        self.send_repeated(Message::ClipboardAck(chunk.id), 2).await
    }
}
