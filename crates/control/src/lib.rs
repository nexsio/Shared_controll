//! Gemeinsame Maus, Tastatur und Zwischenablage über USB-Funk-Dongles.
//! Der Master hat Maus und Tastatur und steuert den Node; der Node spielt die Eingaben ein.

mod capture;
mod clipboard;
mod config;
mod dialog;
mod injector;
mod link;
mod session;
mod settings;
mod tray;

use std::{
    future::pending,
    sync::{atomic::AtomicBool, Arc},
    time::Duration,
};

use hidapi::HidApi;
use security::TrustStore;
use tokio::{sync::mpsc, task::LocalSet, time::sleep};

use capture::CaptureEvent;
use session::{Ended, Local, Session};
use tray::{LinkStatus, Ui, UiCmd};

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Master,
    Node,
}

pub struct Options {
    pub role: Role,
    pub dongle: usize,
}

// So oft hintereinander darf der Dongle fehlen, bevor sich das Programm beendet (Dongle abgezogen).
const NO_DONGLE_RETRIES: u8 = 5;

fn usage(role: Role) -> &'static str {
    match role {
        Role::Master => "Usage: shared-control-master [--dongle N]\n  --dongle N   use the N-th dongle if several are plugged in (default 0)\nEmergency exit: Scroll Lock ends remote control immediately.",
        Role::Node => "Usage: shared-control-node [--dongle N]\n  --dongle N   use the N-th dongle if several are plugged in (default 0)",
    }
}

fn parse_args(role: Role) -> Result<Options, String> {
    let mut opts = Options { role, dongle: 0 };
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--dongle" => {
                opts.dongle = args.next().and_then(|v| v.parse().ok()).ok_or("--dongle needs a number")?;
            }
            other => return Err(format!("unknown option {other}")),
        }
    }
    Ok(opts)
}

async fn app(opts: Options) -> Result<(), String> {
    let identity = config::load_identity().map_err(|e| format!("identity: {e}"))?;
    let mut trust = config::FileTrustStore::load().map_err(|e| format!("trust list: {e}"))?;
    let mut ui = tray::start(opts.role, trust.latest().as_ref().map(config::partner_label)).await;

    println!("Requesting permissions, please confirm the KDE dialogs ...");
    let injector = match opts.role {
        Role::Node => Some(injector::Injector::connect().await.map_err(|e| format!("input injection: {e}"))?),
        Role::Master => None,
    };
    let connected = Arc::new(AtomicBool::new(false));
    let (event_tx, mut events) = mpsc::channel(512);
    let (cmd, cmd_rx) = mpsc::channel(8);
    tokio::task::spawn_local(capture::run(Arc::clone(&connected), event_tx, cmd_rx, ui.settings.clone()));
    let zone = match events.recv().await {
        Some(CaptureEvent::Ready { width, height }) => (width, height),
        Some(CaptureEvent::Failed(e)) => return Err(format!("input capture: {e}")),
        _ => return Err("input capture could not be started".into()),
    };
    let mut local = Local { injector, events, cmd, connected, zone };
    println!("Ready. Waiting for the dongle.");

    let mut force_pair = false;
    let mut missing = 0u8;
    loop {
        ui.state.send_modify(|s| s.status = LinkStatus::NoLink);
        let ended = connect_once(&opts, &identity, &mut trust, &mut local, &mut ui, force_pair).await;
        force_pair = false;
        missing = if matches!(ended, Ended::NoDongle) { missing + 1 } else { 0 };
        match ended {
            Ended::Fatal(e) => return Err(e),
            Ended::Quit => return Ok(()),
            Ended::NoDongle if missing >= NO_DONGLE_RETRIES => {
                println!("No dongle found, exiting.");
                return Ok(());
            }
            Ended::NoDongle => {}
            Ended::Repair => force_pair = true,
            Ended::Forget => {
                if let Some(key) = trust.latest() {
                    trust.revoke(&key);
                }
                ui.state.send_modify(|s| s.partner = None);
            }
            Ended::Link(e) => eprintln!("Connection ended: {e}"),
        }
        sleep(Duration::from_secs(1)).await;
    }
}

/// Tray-Befehle, die den Verbindungsaufbau abbrechen.
async fn interrupted(cmds: &mut mpsc::UnboundedReceiver<UiCmd>) -> Ended {
    loop {
        match cmds.recv().await {
            Some(UiCmd::Quit) => return Ended::Quit,
            Some(UiCmd::Forget) => return Ended::Forget,
            Some(UiCmd::Repair) => return Ended::Repair,
            Some(UiCmd::Reconnect) => return Ended::Link("reconnect requested".into()),
            Some(UiCmd::Back) => {}
            None => pending().await,
        }
    }
}

async fn connect_once(
    opts: &Options,
    identity: &security::Identity,
    trust: &mut config::FileTrustStore,
    local: &mut Local,
    ui: &mut Ui,
    force_pair: bool,
) -> Ended {
    let api = match HidApi::new() {
        Ok(api) => api,
        Err(e) => return Ended::Fatal(format!("HID: {e}")),
    };
    let transport = match link::open_dongle(&api, opts.dongle) {
        Ok(t) => t,
        Err(ended) => return ended,
    };
    let established = tokio::select! {
        result = link::establish(transport, opts, identity, trust, force_pair, &ui.state) => result,
        ended = interrupted(&mut ui.cmds) => Err(ended),
    };
    let channel = match established {
        Ok(channel) => channel,
        Err(ended) => return ended,
    };
    println!("Connected and authenticated.");
    Session::new(channel, opts, local, ui).run().await
}

/// Einstiegspunkt der beiden Programme: liest die Argumente und läuft bis zu einem Fehler.
pub fn main(role: Role) {
    let opts = match parse_args(role) {
        Ok(opts) => opts,
        Err(e) => {
            eprintln!("{e}\n{}", usage(role));
            std::process::exit(2);
        }
    };
    let runtime = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
        Ok(runtime) => runtime,
        Err(e) => {
            eprintln!("Error: {e}");
            std::process::exit(1);
        }
    };
    // Beenden ohne Drop von LocalSet und Runtime: das Aufräumen der zbus-Signalströme im Capture-Task bricht sonst mit Abort ab.
    let local = LocalSet::new();
    let result = runtime.block_on(local.run_until(app(opts)));
    if let Err(e) = result {
        eprintln!("Error: {e}");
        std::process::exit(1);
    }
    std::process::exit(0);
}
