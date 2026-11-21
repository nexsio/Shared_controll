//! Einspeisen von Eingaben über das RemoteDesktop-Portal und libei.

use std::{
    any::Any,
    cell::RefCell,
    collections::{HashMap, HashSet},
    fs,
    os::unix::net::UnixStream,
    rc::Rc,
    time::{Duration, Instant},
};

use ashpd::desktop::{
    remote_desktop::{ConnectToEISOptions, DeviceType, RemoteDesktop, SelectDevicesOptions, StartOptions},
    CreateSessionOptions, PersistMode,
};
use futures_util::StreamExt;
use reis::{ei, tokio::EiEventStream, PendingRequestResult};
use tokio::time::{sleep, timeout};

use crate::config;

// Manche KDE-Oberflächen schließen Menüs und erkennen Doppelklicks nicht, wenn Drücken und Loslassen fast gleichzeitig ankommen.
const MIN_BUTTON_HOLD: Duration = Duration::from_millis(30);

type AnyResult<T> = Result<T, Box<dyn std::error::Error>>;

struct Dev {
    device: ei::Device,
    name: String,
    pointer: Option<ei::Pointer>,
    button: Option<ei::Button>,
    scroll: Option<ei::Scroll>,
    keyboard: Option<ei::Keyboard>,
    resumed: bool,
}

#[derive(Default)]
struct Seat {
    capabilities: HashMap<String, u64>,
}

struct State {
    context: ei::Context,
    seats: HashMap<ei::Seat, Seat>,
    devices: HashMap<ei::Device, Dev>,
    serial: u32,
    sequence: u32,
    started: Instant,
    emulating: bool,
    keys: HashSet<u32>,
    buttons: HashSet<u32>,
    pressed_at: HashMap<u32, Instant>,
    release_pending: HashSet<u32>,
    alive: bool,
    // Hält die Portal-Sitzung offen.
    _portal: Option<Box<dyn Any>>,
}

impl State {
    fn handle(&mut self, event: ei::Event) {
        match event {
            ei::Event::Connection(_, ei::connection::Event::Seat { seat }) => {
                self.seats.insert(seat, Seat::default());
            }
            ei::Event::Connection(_, ei::connection::Event::Ping { ping }) => ping.done(0),
            ei::Event::Seat(seat, request) => match request {
                ei::seat::Event::Capability { mask, interface } => {
                    if let Some(s) = self.seats.get_mut(&seat) {
                        s.capabilities.insert(interface, mask);
                    }
                }
                ei::seat::Event::Done => {
                    if let Some(s) = self.seats.get(&seat) {
                        seat.bind(s.capabilities.values().fold(0, |a, b| a | b));
                    }
                }
                ei::seat::Event::Device { device } => {
                    self.devices.insert(
                        device.clone(),
                        Dev { device, name: String::new(), pointer: None, button: None, scroll: None, keyboard: None, resumed: false },
                    );
                }
                _ => {}
            },
            ei::Event::Device(device, request) => {
                let Some(dev) = self.devices.get_mut(&device) else { return };
                match request {
                    ei::device::Event::Interface { object } => {
                        if let Some(p) = object.clone().downcast::<ei::Pointer>() {
                            dev.pointer = Some(p);
                        } else if let Some(b) = object.clone().downcast::<ei::Button>() {
                            dev.button = Some(b);
                        } else if let Some(s) = object.clone().downcast::<ei::Scroll>() {
                            dev.scroll = Some(s);
                        } else if let Some(k) = object.downcast::<ei::Keyboard>() {
                            dev.keyboard = Some(k);
                        }
                    }
                    ei::device::Event::Resumed { serial } => {
                        self.serial = serial;
                        dev.resumed = true;
                    }
                    ei::device::Event::Paused { .. } => dev.resumed = false,
                    ei::device::Event::Name { name } => dev.name = name,
                    _ => {}
                }
            }
            _ => {}
        }
        let _ = self.context.flush();
    }

    fn ready(&self) -> bool {
        let has = |f: fn(&Dev) -> bool| self.devices.values().any(|d| d.resumed && f(d));
        has(|d| d.pointer.is_some()) && has(|d| d.keyboard.is_some())
    }

    fn ts(&self) -> u64 {
        self.started.elapsed().as_micros() as u64
    }

    // Ein Gerät pro Ereignis: Bietet der Compositor ein relatives und ein absolutes Zeigergerät, bekäme er Tasten und Rad sonst doppelt.
    fn device_with(&self, has: fn(&Dev) -> bool) -> Option<&Dev> {
        let mut found: Option<&Dev> = None;
        for dev in self.devices.values().filter(|d| d.resumed && has(d)) {
            if found.map_or(true, |f| f.pointer.is_none() && dev.pointer.is_some()) {
                found = Some(dev);
            }
        }
        found
    }

    fn frame(&self, dev: &Dev) {
        dev.device.frame(self.serial, self.ts());
        let _ = self.context.flush();
    }

    fn release_all(&mut self) {
        for code in std::mem::take(&mut self.keys) {
            self.send_key(code, false);
        }
        for code in std::mem::take(&mut self.buttons) {
            self.send_button(code, false);
        }
    }

    fn send_key(&self, code: u32, pressed: bool) {
        let state = if pressed { ei::keyboard::KeyState::Press } else { ei::keyboard::KeyState::Released };
        let Some(dev) = self.device_with(|d| d.keyboard.is_some()) else { return };
        if let Some(k) = &dev.keyboard {
            k.key(code, state);
        }
        self.frame(dev);
    }

    fn send_button(&self, code: u32, pressed: bool) {
        let state = if pressed { ei::button::ButtonState::Press } else { ei::button::ButtonState::Released };
        let Some(dev) = self.device_with(|d| d.button.is_some()) else { return };
        if let Some(b) = &dev.button {
            b.button(code, state);
        }
        self.frame(dev);
    }
}

/// Handle auf die Eingabe-Einspeisung; Ereignisse des Compositors werden in einer eigenen Task verarbeitet.
#[derive(Clone)]
pub struct Injector {
    state: Rc<RefCell<State>>,
}

impl Injector {
    /// Zeigt beim ersten Start den Portal-Dialog und speichert danach den Freigabe-Token.
    pub async fn connect() -> AnyResult<Self> {
        let remote_desktop = RemoteDesktop::new().await?;
        let session = remote_desktop.create_session(CreateSessionOptions::default()).await?;
        let token_file = config::config_dir()?.join("input-token");
        let stored = fs::read_to_string(&token_file).ok();
        let options = SelectDevicesOptions::default()
            .set_devices(DeviceType::Keyboard | DeviceType::Pointer)
            .set_restore_token(stored.as_deref())
            .set_persist_mode(PersistMode::ExplicitlyRevoked);
        remote_desktop.select_devices(&session, options).await?;
        let selected = remote_desktop.start(&session, None, StartOptions::default()).await?.response()?;
        if let Some(token) = selected.restore_token() {
            config::write_private(&token_file, token.as_bytes())?;
        }

        let fd = remote_desktop.connect_to_eis(&session, ConnectToEISOptions::default()).await?;
        let context = ei::Context::new(UnixStream::from(fd))?;
        let mut events = EiEventStream::new(context.clone())?;
        let handshake =
            reis::tokio::ei_handshake(&mut events, "shared-control", ei::handshake::ContextType::Sender).await?;

        let state = Rc::new(RefCell::new(State {
            context,
            seats: HashMap::new(),
            devices: HashMap::new(),
            serial: handshake.serial,
            sequence: 0,
            started: Instant::now(),
            emulating: false,
            keys: HashSet::new(),
            buttons: HashSet::new(),
            pressed_at: HashMap::new(),
            release_pending: HashSet::new(),
            alive: true,
            _portal: Some(Box::new((remote_desktop, session))),
        }));

        let wait = async {
            while let Some(result) = events.next().await {
                if let PendingRequestResult::Request(event) = result? {
                    state.borrow_mut().handle(event);
                    if state.borrow().ready() {
                        return Ok::<_, reis::Error>(true);
                    }
                }
            }
            Ok(false)
        };
        match timeout(Duration::from_secs(10), wait).await {
            Ok(Ok(true)) => {}
            Ok(Ok(false)) => return Err("Compositor hat die Eingabeverbindung beendet".into()),
            Ok(Err(e)) => return Err(e.into()),
            Err(_) => return Err("Keine Zeiger- und Tastaturgeräte vom Compositor erhalten".into()),
        }

        let pump_state = Rc::clone(&state);
        for dev in state.borrow().devices.values() {
            eprintln!(
                "Input device {:?}: pointer={} button={} scroll={} keyboard={} resumed={}",
                dev.name,
                dev.pointer.is_some(),
                dev.button.is_some(),
                dev.scroll.is_some(),
                dev.keyboard.is_some(),
                dev.resumed
            );
        }
        tokio::task::spawn_local(async move {
            while let Some(Ok(result)) = events.next().await {
                if let PendingRequestResult::Request(event) = result {
                    pump_state.borrow_mut().handle(event);
                }
            }
            pump_state.borrow_mut().alive = false;
        });
        Ok(Self { state })
    }

    pub fn alive(&self) -> bool {
        self.state.borrow().alive
    }

    /// Beginnt die Einspeisung; muss vor `motion` & Co. aufgerufen werden.
    pub fn begin(&self) {
        let mut s = self.state.borrow_mut();
        if s.emulating {
            return;
        }
        let serial = s.serial;
        let mut sequence = s.sequence;
        for dev in s.devices.values().filter(|d| d.resumed) {
            dev.device.start_emulating(sequence, serial);
            sequence += 1;
        }
        s.sequence = sequence;
        s.emulating = true;
        let _ = s.context.flush();
    }

    /// Beendet die Einspeisung und lässt alle gedrückten Tasten und Maustasten los.
    pub fn end(&self) {
        let mut s = self.state.borrow_mut();
        if !s.emulating {
            return;
        }
        s.release_all();
        for dev in s.devices.values().filter(|d| d.resumed) {
            dev.device.stop_emulating(s.serial);
        }
        s.emulating = false;
        let _ = s.context.flush();
    }

    pub fn motion(&self, dx: f32, dy: f32) {
        let s = self.state.borrow();
        if !s.emulating {
            return;
        }
        let Some(dev) = s.device_with(|d| d.pointer.is_some()) else { return };
        if let Some(p) = &dev.pointer {
            p.motion_relative(dx, dy);
        }
        s.frame(dev);
    }

    /// Einheiten wie bei libei: 120 entsprechen einer Rasterstufe des Mausrads.
    pub fn wheel(&self, dx: i32, dy: i32) {
        let s = self.state.borrow();
        if !s.emulating {
            return;
        }
        let Some(dev) = s.device_with(|d| d.scroll.is_some()) else { return };
        if let Some(sc) = &dev.scroll {
            sc.scroll_discrete(dx, dy);
        }
        s.frame(dev);
    }

    pub fn key(&self, code: u32, pressed: bool) {
        let mut s = self.state.borrow_mut();
        let changed = if pressed { s.keys.insert(code) } else { s.keys.remove(&code) };
        if s.emulating && changed {
            s.send_key(code, pressed);
        }
    }

    pub fn button(&self, code: u32, pressed: bool) {
        if pressed {
            let pending = self.state.borrow().release_pending.contains(&code);
            if pending {
                self.release_button(code);
            }
            let mut s = self.state.borrow_mut();
            if s.buttons.insert(code) {
                s.pressed_at.insert(code, Instant::now());
                if s.emulating {
                    s.send_button(code, true);
                }
            }
            return;
        }
        let wait = {
            let mut s = self.state.borrow_mut();
            if !s.buttons.contains(&code) || s.release_pending.contains(&code) {
                return;
            }
            let held = s.pressed_at.get(&code).map_or(MIN_BUTTON_HOLD, Instant::elapsed);
            let wait = MIN_BUTTON_HOLD.saturating_sub(held);
            if !wait.is_zero() {
                s.release_pending.insert(code);
            }
            wait
        };
        if wait.is_zero() {
            self.release_button(code);
        } else {
            let this = self.clone();
            tokio::task::spawn_local(async move {
                sleep(wait).await;
                this.release_button(code);
            });
        }
    }

    fn release_button(&self, code: u32) {
        let mut s = self.state.borrow_mut();
        s.release_pending.remove(&code);
        s.pressed_at.remove(&code);
        if s.buttons.remove(&code) && s.emulating {
            s.send_button(code, false);
        }
    }
}
