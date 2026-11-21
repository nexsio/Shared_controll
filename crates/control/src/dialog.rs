//! Kleine KDE-Dialoge über `kdialog`.

use tokio::process::Command;

const TITLE: &str = "Shared Control";

/// `None`, wenn `kdialog` nicht gestartet werden kann.
pub async fn yes_no(text: &str) -> Option<bool> {
    let status = Command::new("kdialog").args(["--title", TITLE, "--yesno", text]).kill_on_drop(true).status().await.ok()?;
    Some(status.success())
}

pub async fn warn_continue(text: &str) -> bool {
    let status = Command::new("kdialog").args(["--title", TITLE, "--warningcontinuecancel", text]).kill_on_drop(true).status().await;
    status.is_ok_and(|s| s.success())
}

pub async fn input(text: &str, default: &str) -> Option<String> {
    let output = Command::new("kdialog").args(["--title", TITLE, "--inputbox", text, default]).kill_on_drop(true).output().await.ok()?;
    output.status.success().then(|| String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

pub async fn error(text: &str) {
    let _ = Command::new("kdialog").args(["--title", TITLE, "--error", text]).kill_on_drop(true).status().await;
}
