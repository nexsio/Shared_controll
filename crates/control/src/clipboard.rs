//! Zugriff auf die Wayland-Zwischenablage über `wl-paste` und `wl-copy` (nur Text).

use std::{io, process::Stdio};

use base64::{engine::general_purpose::STANDARD, Engine};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines},
    process::{Child, ChildStdout, Command},
};

pub const MAX_TEXT_LEN: usize = 4096;

pub struct Watcher {
    _child: Child,
    lines: Lines<BufReader<ChildStdout>>,
}

impl Watcher {
    pub fn start() -> io::Result<Self> {
        let mut child = Command::new("wl-paste")
            .args(["--type", "text/plain", "--watch", "sh", "-c", "base64 -w0; echo"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()?;
        let stdout = child.stdout.take().ok_or_else(|| io::Error::other("wl-paste ohne Ausgabe"))?;
        Ok(Self { _child: child, lines: BufReader::new(stdout).lines() })
    }

    /// Wartet auf die nächste Textänderung; `None`, wenn `wl-paste` beendet wurde.
    pub async fn next(&mut self) -> Option<String> {
        loop {
            let line = self.lines.next_line().await.ok()??;
            let Ok(bytes) = STANDARD.decode(line.trim()) else { continue };
            if let Ok(text) = String::from_utf8(bytes) {
                if !text.is_empty() {
                    return Some(text);
                }
            }
        }
    }
}

pub async fn set(text: &str) -> io::Result<()> {
    let mut child = Command::new("wl-copy")
        .args(["--type", "text/plain"])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(text.as_bytes()).await?;
    }
    child.wait().await?;
    Ok(())
}
