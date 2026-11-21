# Shared Control

Shared Control lets you use one computer's mouse and keyboard on another computer.
The first computer is the **master**. The second computer is the **node**.
You can also share text from the clipboard in both directions.

## Current status

Shared Control is in development. The current version supports one master and one
node. It runs on Linux with Wayland. It was developed and tested with Fedora KDE
Plasma.

The software must run on both computers. It captures and injects input through
Wayland desktop portals. The USB dongles carry data between the computers over
2.4 GHz radio. They do not act as a USB keyboard or mouse.

This means the current version does not work before login or in the BIOS. The
planned USB keyboard and mouse mode is not implemented yet. See
[`Plans/Plan_Dongle_Key.md`](Plans/Plan_Dongle_Key.md).

## What you need

- Two Linux computers running Wayland.
- Two Nordic nRF52840 USB Dongles (PCA10059), with Shared Control firmware.
- Rust and Cargo to build from source.
- `wl-clipboard` and `kdialog` on both computers.

The current version uses the dongle transport. A Bluetooth transport crate is
also present in the repository, but it is not the transport used by the
master and node applications.

## Build

Run these commands from the repository root:

```sh
cargo build --release
```

The build creates these programs:

- `target/release/shared-control-master`
- `target/release/shared-control-node`

## Setup

Do these steps on both computers:

1. Make sure you are logged in to a Wayland desktop session.
2. Build Shared Control, or obtain the matching program binaries.
3. Install the udev rule so your user can access the dongle:

   ```sh
   sudo install -Dm644 dongle-setup/60-shared-control.rules \
     /etc/udev/rules.d/60-shared-control.rules
   sudo udevadm control --reload-rules
   ```

4. Unplug and reconnect the dongle after installing the rule.

The rule grants access to the logged-in user. Do not run Shared Control as root.
The `dongle-setup/` directory contains setup files; it does not provide a
step-by-step firmware flashing guide. Make sure each dongle already has the
firmware used by this version.

## Start and pair

1. Plug one dongle into each computer.
2. On the master, run:

   ```sh
   ./target/release/shared-control-master
   ```

3. On the node, run:

   ```sh
   ./target/release/shared-control-node
   ```

4. Approve the input capture or input injection requests from the desktop when
   they appear.
5. On first connection, compare the pairing codes shown on both computers.
   Confirm only if the codes match.

The programs save the pairing information and try to reconnect to the trusted
partner on later starts.

## Use

- Move the pointer to the configured edge of the master screen to start
  controlling the node. The default master edge is **Right**.
- Move the pointer to the configured edge of the node screen to return. The
  default node edge is **Left**.
- Use the tray menu to change the edge or enable and disable clipboard sharing.
  Text clipboard sharing is enabled by default.
- Press **Scroll Lock** on the master to stop remote control immediately.

The tray icon shows the connection state. The dongle LED shows:

- **Red**: no radio link.
- **Blue**: radio link is up, but the software has not authenticated the partner.
- **Green**: the software has authenticated the partner.

## How it works

1. The master software asks the Wayland desktop to capture mouse and keyboard
   input when the pointer reaches the configured edge.
2. The master software encrypts the data and sends it to the master dongle over
   USB.
3. The dongles transfer the encrypted data over a 2.4 GHz IEEE 802.15.4 radio
   link.
4. The node software receives and checks the data. It injects the input through
   the Wayland desktop portal.
5. Clipboard text uses the same encrypted connection in either direction.

The dongle firmware handles USB and radio transport. Pairing, encryption,
authentication, input capture, and input injection are handled by the software
on the computers.

## Limitations

- Both computers need to be logged in, and both programs must be running.
- Wayland desktop portals must support input capture and remote desktop input.
- The current application supports one node per master.
- Windows, macOS, and input through the dongle before login are not supported.
- A compromised computer can send unwanted input. Software cannot protect the
  other computer from a compromised trusted partner.

## Repository

- `software_master/` and `software_node/`: application entry points.
- `crates/control/`: input capture, injection, clipboard, pairing, and tray.
- `crates/security/`: authentication and encrypted communication.
- `crates/transport-dongle/`: USB transport for the dongles.
- `dongle-firmware/`: nRF52840 dongle firmware.


## License

MIT OR Apache-2.0
