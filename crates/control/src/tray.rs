//! Tray-Symbol ("SCM" bzw. "SCN") mit Rechtsklick-Menü.

use std::sync::Arc;

use ksni::{
    menu::{CheckmarkItem, RadioGroup, RadioItem, StandardItem, SubMenu},
    Icon, MenuItem, ToolTip, Tray, TrayMethods,
};
use protocol::Edge;
use tokio::sync::{mpsc, watch};

use crate::{
    dialog,
    settings::{normalize_shortcut, Settings},
    Role,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UiCmd {
    Back,
    Reconnect,
    Repair,
    Forget,
    Quit,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LinkStatus {
    NoLink,
    Radio,
    Authenticated,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UiState {
    pub status: LinkStatus,
    /// Kurzkennung des gekoppelten Partners.
    pub partner: Option<String>,
}

/// Verbindung zwischen Tray und Programm.
pub struct Ui {
    pub settings: watch::Receiver<Settings>,
    pub state: watch::Sender<UiState>,
    pub cmds: mpsc::UnboundedReceiver<UiCmd>,
    // Halten die Kanäle offen, auch wenn das Tray nicht starten konnte.
    _cmd_tx: mpsc::UnboundedSender<UiCmd>,
    _settings_tx: Arc<watch::Sender<Settings>>,
}

struct ScTray {
    role: Role,
    state: UiState,
    settings: Settings,
    settings_tx: Arc<watch::Sender<Settings>>,
    cmds: mpsc::UnboundedSender<UiCmd>,
}

const EDGES: [(&str, Edge); 4] = [("Top", Edge::Top), ("Bottom", Edge::Bottom), ("Left", Edge::Left), ("Right", Edge::Right)];

pub async fn start(role: Role, partner: Option<String>) -> Ui {
    let settings = Settings::load(role);
    let (settings_tx, settings_rx) = watch::channel(settings.clone());
    let settings_tx = Arc::new(settings_tx);
    let state = UiState { status: LinkStatus::NoLink, partner };
    let (state_tx, mut state_rx) = watch::channel(state.clone());
    let (cmd_tx, cmds) = mpsc::unbounded_channel();

    let tray = ScTray { role, state, settings, settings_tx: Arc::clone(&settings_tx), cmds: cmd_tx.clone() };
    match tray.spawn().await {
        Ok(handle) => {
            let state_handle = handle.clone();
            tokio::spawn(async move {
                while state_rx.changed().await.is_ok() {
                    let state = state_rx.borrow_and_update().clone();
                    state_handle.update(|tray| tray.state = state).await;
                }
            });
            let mut settings_changes = settings_tx.subscribe();
            tokio::spawn(async move {
                while settings_changes.changed().await.is_ok() {
                    let settings = settings_changes.borrow_and_update().clone();
                    handle.update(|tray| tray.settings = settings).await;
                }
            });
        }
        Err(e) => eprintln!("Tray icon unavailable: {e}"),
    }
    Ui { settings: settings_rx, state: state_tx, cmds, _cmd_tx: cmd_tx, _settings_tx: settings_tx }
}

const GLYPH_ROWS: usize = 7;
// Je Zeile 5 Bit, das linke Pixel ist Bit 4.
fn glyph(letter: char) -> [u8; GLYPH_ROWS] {
    match letter {
        'S' => [0b01111, 0b10000, 0b10000, 0b01110, 0b00001, 0b00001, 0b11110],
        'C' => [0b01110, 0b10001, 0b10000, 0b10000, 0b10000, 0b10001, 0b01110],
        'M' => [0b10001, 0b11011, 0b10101, 0b10001, 0b10001, 0b10001, 0b10001],
        _ => [0b10001, 0b11001, 0b10101, 0b10011, 0b10001, 0b10001, 0b10001],
    }
}

fn status_color(status: LinkStatus) -> [u8; 3] {
    match status {
        LinkStatus::NoLink => [0xE5, 0x48, 0x4D],
        LinkStatus::Radio => [0x3E, 0x8E, 0xF7],
        LinkStatus::Authenticated => [0x30, 0xA4, 0x6C],
    }
}

/// Zeichnet "SCM" bzw. "SCN" in der Statusfarbe auf transparentem Grund (ARGB32).
pub fn icon(role: Role, status: LinkStatus, size: u32) -> Icon {
    const COLUMNS: usize = 17;
    let last = if role == Role::Master { 'M' } else { 'N' };
    let letters = [glyph('S'), glyph('C'), glyph(last)];
    let [r, g, b] = status_color(status);
    let size_f = size as f32;
    let scale_x = size_f / COLUMNS as f32;
    let scale_y = (scale_x * 1.4).min(size_f / GLYPH_ROWS as f32);
    let top = (size_f - GLYPH_ROWS as f32 * scale_y) / 2.0;
    let mut data = vec![0u8; (size * size * 4) as usize];
    for y in 0..size {
        let row_pos = y as f32 - top;
        if row_pos < 0.0 {
            continue;
        }
        let row = (row_pos / scale_y) as usize;
        if row >= GLYPH_ROWS {
            continue;
        }
        for x in 0..size {
            let column = (x as f32 / scale_x) as usize;
            if column >= COLUMNS || column % 6 == 5 {
                continue;
            }
            if (letters[column / 6][row] >> (4 - column % 6)) & 1 == 1 {
                let at = ((y * size + x) * 4) as usize;
                data[at..at + 4].copy_from_slice(&[255, r, g, b]);
            }
        }
    }
    Icon { width: size as i32, height: size as i32, data }
}

fn radio(label: &str) -> RadioItem {
    RadioItem { label: label.into(), ..Default::default() }
}

fn action(label: &str, cmd: UiCmd) -> MenuItem<ScTray> {
    StandardItem { label: label.into(), activate: Box::new(move |tray: &mut ScTray| tray.send(cmd)), ..Default::default() }.into()
}

impl ScTray {
    fn send(&self, cmd: UiCmd) {
        let _ = self.cmds.send(cmd);
    }

    fn change(&mut self, apply: impl FnOnce(&mut Settings)) {
        apply(&mut self.settings);
        self.settings.save(self.role);
        self.settings_tx.send_replace(self.settings.clone());
    }

    fn peer_word(&self) -> &'static str {
        if self.role == Role::Master {
            "Node"
        } else {
            "Master"
        }
    }

    fn peer_menu(&self) -> MenuItem<Self> {
        let word = self.peer_word();
        let label = match &self.state.partner {
            Some(partner) => format!("{word}: {partner}"),
            None => format!("{word}: not paired"),
        };
        if self.role == Role::Node {
            return StandardItem { label, enabled: false, ..Default::default() }.into();
        }
        let mut submenu = Vec::new();
        if let Some(partner) = &self.state.partner {
            submenu.push(RadioGroup { selected: 0, options: vec![radio(partner)], ..Default::default() }.into());
            submenu.push(MenuItem::Separator);
        }
        submenu.push(action("Pair new node…", UiCmd::Repair));
        SubMenu { label, submenu, ..Default::default() }.into()
    }

    fn edge_menu(&self) -> MenuItem<Self> {
        let master = self.role == Role::Master;
        let offset = usize::from(master);
        let mut options = Vec::new();
        if master {
            options.push(radio("Off"));
        }
        options.extend(EDGES.iter().map(|(label, _)| radio(label)));
        let selected = match self.settings.edge {
            None => 0,
            Some(edge) => EDGES.iter().position(|(_, e)| *e == edge).map_or(0, |i| i + offset),
        };
        RadioGroup {
            selected,
            select: Box::new(move |tray: &mut Self, index| {
                let edge = if master && index == 0 { Some(None) } else { EDGES.get(index - offset).map(|(_, e)| Some(*e)) };
                if let Some(edge) = edge {
                    tray.change(|s| s.edge = edge);
                }
            }),
            options,
        }
        .into()
    }

    fn shortcut_menu(&self) -> MenuItem<Self> {
        let current = self.settings.shortcut.clone();
        let create = |label: String| -> MenuItem<Self> {
            StandardItem { label, activate: Box::new(|tray: &mut Self| tray.create_shortcut()), ..Default::default() }.into()
        };
        let submenu = match current {
            None => vec![create("Create shortcut…".into())],
            Some(shortcut) => vec![
                create(shortcut),
                StandardItem { label: "Delete".into(), activate: Box::new(|tray: &mut Self| tray.change(|s| s.shortcut = None)), ..Default::default() }
                    .into(),
            ],
        };
        SubMenu { label: "Shortcut".into(), submenu, ..Default::default() }.into()
    }

    fn create_shortcut(&self) {
        let settings_tx = Arc::clone(&self.settings_tx);
        let role = self.role;
        let current = self.settings.shortcut.clone();
        tokio::spawn(async move {
            let default = current.as_deref().unwrap_or("Ctrl+P");
            let Some(text) = dialog::input("Enter a key combination, for example Ctrl+P", default).await else { return };
            match normalize_shortcut(&text) {
                Some(shortcut) => {
                    settings_tx.send_modify(|s| s.shortcut = Some(shortcut));
                    settings_tx.borrow().clone().save(role);
                }
                None => dialog::error("Invalid shortcut. Use at least one modifier and one key, for example Ctrl+Alt+P.").await,
            }
        });
    }

    fn forget_item(&self) -> MenuItem<Self> {
        let word = self.peer_word().to_lowercase();
        StandardItem {
            label: format!("Forget {word}…"),
            activate: Box::new(move |tray: &mut Self| {
                let cmds = tray.cmds.clone();
                let text = format!("Forget the paired {word}? Pairing with code confirmation will be required again.");
                tokio::spawn(async move {
                    if dialog::warn_continue(&text).await {
                        let _ = cmds.send(UiCmd::Forget);
                    }
                });
            }),
            ..Default::default()
        }
        .into()
    }
}

impl Tray for ScTray {
    const MENU_ON_ACTIVATE: bool = true;

    fn id(&self) -> String {
        match self.role {
            Role::Master => "shared-control-master".into(),
            Role::Node => "shared-control-node".into(),
        }
    }

    fn title(&self) -> String {
        match self.role {
            Role::Master => "Shared Control Master".into(),
            Role::Node => "Shared Control Node".into(),
        }
    }

    fn icon_pixmap(&self) -> Vec<Icon> {
        [16, 22, 32, 48, 64].into_iter().map(|size| icon(self.role, self.state.status, size)).collect()
    }

    fn tool_tip(&self) -> ToolTip {
        let description = match self.state.status {
            LinkStatus::NoLink => "No radio link",
            LinkStatus::Radio => "Radio link up, not paired yet",
            LinkStatus::Authenticated => "Connected and authenticated",
        };
        ToolTip { title: self.title(), description: description.into(), ..Default::default() }
    }

    fn menu(&self) -> Vec<MenuItem<Self>> {
        let master = self.role == Role::Master;
        let clipboard = CheckmarkItem {
            label: "Sync clipboard".into(),
            checked: self.settings.clipboard,
            activate: Box::new(|tray: &mut Self| {
                let enabled = !tray.settings.clipboard;
                tray.change(|s| s.clipboard = enabled);
            }),
            ..Default::default()
        };
        let mut items = vec![self.peer_menu(), MenuItem::Separator, clipboard.into()];
        if master {
            let edge = SubMenu { label: "Switch at screen edge".into(), submenu: vec![self.edge_menu()], ..Default::default() };
            let switch = SubMenu { label: "Control switch".into(), submenu: vec![edge.into(), self.shortcut_menu()], ..Default::default() };
            items.push(switch.into());
        } else {
            items.push(SubMenu { label: "Screen edge to master".into(), submenu: vec![self.edge_menu()], ..Default::default() }.into());
        }
        items.push(MenuItem::Separator);
        if master {
            items.push(action("Back to this computer", UiCmd::Back));
        }
        items.push(action("Reconnect", UiCmd::Reconnect));
        items.push(MenuItem::Separator);
        items.push(self.forget_item());
        items.push(action("Quit", UiCmd::Quit));
        items
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opaque_pixels(icon: &Icon) -> usize {
        icon.data.chunks_exact(4).filter(|p| p[0] == 255).count()
    }

    #[test]
    fn icon_is_argb_of_the_requested_size() {
        let icon = icon(Role::Master, LinkStatus::Radio, 22);
        assert_eq!((icon.width, icon.height), (22, 22));
        assert_eq!(icon.data.len(), 22 * 22 * 4);
        assert!(opaque_pixels(&icon) > 20);
        let first = icon.data.chunks_exact(4).find(|p| p[0] == 255).unwrap();
        assert_eq!(&first[1..], &status_color(LinkStatus::Radio));
    }

    #[test]
    fn master_and_node_differ_in_the_last_letter() {
        let master = icon(Role::Master, LinkStatus::NoLink, 48);
        let node = icon(Role::Node, LinkStatus::NoLink, 48);
        assert_ne!(master.data, node.data);
    }

    #[test]
    fn every_size_draws_something() {
        for size in [16, 22, 32, 48, 64] {
            assert!(opaque_pixels(&icon(Role::Node, LinkStatus::Authenticated, size)) > 0);
        }
    }
}
