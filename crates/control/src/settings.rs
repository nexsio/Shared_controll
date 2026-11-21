//! Persistente Einstellungen, die über das Tray-Menü geändert werden.

use protocol::Edge;

use crate::{config, Role};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Settings {
    pub clipboard: bool,
    /// `None`: Umschalten an der Bildschirmkante ist aus (nur Master).
    pub edge: Option<Edge>,
    pub shortcut: Option<String>,
}

pub fn edge_name(edge: Edge) -> &'static str {
    match edge {
        Edge::Left => "left",
        Edge::Right => "right",
        Edge::Top => "top",
        Edge::Bottom => "bottom",
    }
}

fn parse_edge(text: &str) -> Option<Edge> {
    Some(match text {
        "left" => Edge::Left,
        "right" => Edge::Right,
        "top" => Edge::Top,
        "bottom" => Edge::Bottom,
        _ => return None,
    })
}

impl Settings {
    pub fn defaults(role: Role) -> Self {
        let edge = match role {
            Role::Master => Edge::Right,
            Role::Node => Edge::Left,
        };
        Self { clipboard: true, edge: Some(edge), shortcut: None }
    }

    pub fn parse(role: Role, text: &str) -> Self {
        let mut settings = Self::defaults(role);
        for line in text.lines() {
            let Some((key, value)) = line.split_once('=') else { continue };
            let value = value.trim();
            match key.trim() {
                "clipboard" => settings.clipboard = value != "false",
                "edge" if value == "off" && role == Role::Master => settings.edge = None,
                "edge" => settings.edge = parse_edge(value).or(settings.edge),
                "shortcut" if !value.is_empty() => settings.shortcut = Some(value.to_owned()),
                _ => {}
            }
        }
        settings
    }

    pub fn render(&self) -> String {
        let edge = self.edge.map_or("off", edge_name);
        let mut text = format!("clipboard={}\nedge={edge}\n", self.clipboard);
        if let Some(shortcut) = &self.shortcut {
            text.push_str(&format!("shortcut={shortcut}\n"));
        }
        text
    }

    pub fn load(role: Role) -> Self {
        let text = config::config_dir()
            .ok()
            .and_then(|dir| std::fs::read_to_string(dir.join(file_name(role))).ok())
            .unwrap_or_default();
        Self::parse(role, &text)
    }

    pub fn save(&self, role: Role) {
        let result = config::config_dir().and_then(|dir| config::write_private(&dir.join(file_name(role)), self.render().as_bytes()));
        if let Err(e) = result {
            eprintln!("Settings could not be saved: {e}");
        }
    }
}

fn file_name(role: Role) -> &'static str {
    match role {
        Role::Master => "master.conf",
        Role::Node => "node.conf",
    }
}

/// Normalisiert eine Eingabe wie "ctrl+p" zu "Ctrl+P"; `None`, wenn sie keine gültige Kombination ist.
pub fn normalize_shortcut(input: &str) -> Option<String> {
    let parts: Vec<&str> = input.split('+').map(str::trim).collect();
    let (key, modifiers) = parts.split_last()?;
    if modifiers.is_empty() {
        return None;
    }
    let mut out = Vec::new();
    for modifier in modifiers {
        out.push(match modifier.to_ascii_lowercase().as_str() {
            "ctrl" | "control" | "strg" => "Ctrl",
            "alt" => "Alt",
            "shift" => "Shift",
            "meta" | "super" => "Meta",
            _ => return None,
        });
    }
    let key = key.to_ascii_uppercase();
    let is_function_key = key.strip_prefix('F').and_then(|n| n.parse::<u8>().ok()).is_some_and(|n| (1..=12).contains(&n));
    let is_single = key.len() == 1 && key.chars().all(|c| c.is_ascii_alphanumeric());
    if !is_function_key && !is_single {
        return None;
    }
    out.push(&key);
    Some(out.join("+"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_roundtrip() {
        let settings = Settings { clipboard: false, edge: None, shortcut: Some("Ctrl+P".into()) };
        assert_eq!(Settings::parse(Role::Master, &settings.render()), settings);
    }

    #[test]
    fn node_always_keeps_an_edge() {
        assert_eq!(Settings::parse(Role::Node, "edge=off\n").edge, Some(Edge::Left));
        assert_eq!(Settings::parse(Role::Node, "edge=top\n").edge, Some(Edge::Top));
    }

    #[test]
    fn garbage_falls_back_to_defaults() {
        assert_eq!(Settings::parse(Role::Master, "edge=diagonal\nnonsense\n"), Settings::defaults(Role::Master));
    }

    #[test]
    fn shortcuts_are_validated() {
        assert_eq!(normalize_shortcut("ctrl + p").as_deref(), Some("Ctrl+P"));
        assert_eq!(normalize_shortcut("Strg+Alt+f5").as_deref(), Some("Ctrl+Alt+F5"));
        assert_eq!(normalize_shortcut("p"), None);
        assert_eq!(normalize_shortcut("ctrl+"), None);
        assert_eq!(normalize_shortcut("foo+p"), None);
        assert_eq!(normalize_shortcut("ctrl+f13"), None);
    }
}
