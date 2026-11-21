#![no_std]
#![no_main]

use core::cell::Cell;

use embassy_executor::Spawner;
use embassy_futures::join::join5;
use embassy_futures::select::{select3, Either3};
use embassy_nrf::config::{Config as NrfConfig, HfclkSource};
use embassy_nrf::gpio::{Level, Output, OutputDrive};
use embassy_nrf::radio::ieee802154::{Packet, Radio};
use embassy_nrf::{bind_interrupts, peripherals, radio, usb};
use embassy_sync::blocking_mutex::raw::NoopRawMutex;
use embassy_sync::channel::Channel;
use embassy_sync::signal::Signal;
use embassy_time::{Duration, Instant, Timer};
use embassy_usb::class::hid::{
    Config as HidConfig, HidBootProtocol, HidReader, HidReaderWriter, HidSubclass, HidWriter, State as HidState,
};
use embassy_usb::driver::Driver;
use embassy_usb::{Builder, Config};
use panic_halt as _;

bind_interrupts!(struct Irqs {
    USBD => usb::InterruptHandler<peripherals::USBD>;
    RADIO => radio::InterruptHandler<peripherals::RADIO>;
});

const REPORT_LEN: usize = 64;
const PAYLOAD_LEN: usize = REPORT_LEN - 1;

// HID-Bericht: Byte 0 ist der Typ. DATA gilt in beide Richtungen, STATUS nur Software -> Dongle, LINK nur Dongle -> Software.
const TYPE_DATA: u8 = 1;
const TYPE_STATUS: u8 = 2;
const TYPE_LINK: u8 = 3;

// Funkrahmen: zwei Magic-Bytes, ein Typ, dann Nutzdaten.
const MAGIC: [u8; 2] = *b"SC";
const FRAME_HELLO: u8 = 1;
const FRAME_DATA: u8 = 2;
const RADIO_CHANNEL: u8 = 15;
const LINK_TIMEOUT: Duration = Duration::from_secs(2);

type Report = [u8; REPORT_LEN];
type ReportChannel = Channel<NoopRawMutex, Report, 16>;

struct State {
    link: Cell<bool>,
    authenticated: Cell<bool>,
    changed: Signal<NoopRawMutex, ()>,
}

const REPORT_DESCRIPTOR: &[u8] = &[
    0x06, 0x00, 0xff,
    0x09, 0x01,
    0xa1, 0x01,
    0x09, 0x02,
    0x15, 0x00,
    0x26, 0xff, 0x00,
    0x75, 0x08,
    0x95, 0x40,
    0x81, 0x02,
    0x09, 0x03,
    0x91, 0x02,
    0xc0,
];

fn send_link(state: &State, to_usb: &ReportChannel) {
    let mut report = [0; REPORT_LEN];
    report[0] = TYPE_LINK;
    report[1] = state.link.get() as u8;
    let _ = to_usb.try_send(report);
}

fn set_link(state: &State, to_usb: &ReportChannel, up: bool) {
    state.link.set(up);
    if !up {
        state.authenticated.set(false);
    }
    state.changed.signal(());
    send_link(state, to_usb);
}

async fn send_frame(radio: &mut Radio<'_>, packet: &mut Packet, kind: u8, payload: &[u8]) {
    let mut frame = [0; 3 + PAYLOAD_LEN];
    frame[..2].copy_from_slice(&MAGIC);
    frame[2] = kind;
    frame[3..3 + payload.len()].copy_from_slice(payload);
    packet.copy_from_slice(&frame[..3 + payload.len()]);
    // Bei belegtem Kanal (CCA) kurz warten und erneut versuchen; HELLO darf verloren gehen.
    let attempts = if kind == FRAME_DATA { 6 } else { 1 };
    for attempt in 0..attempts {
        if radio.try_send(packet).await.is_ok() {
            break;
        }
        Timer::after_micros(300 + 200 * attempt).await;
    }
}

async fn radio_loop(
    mut radio: Radio<'_>,
    state: &State,
    to_radio: &ReportChannel,
    to_usb: &ReportChannel,
    hello_interval: Duration,
) {
    let mut rx = Packet::new();
    let mut tx = Packet::new();
    let mut next_hello = Instant::now();
    let mut last_heard = Instant::now();

    loop {
        if Instant::now() >= next_hello {
            send_frame(&mut radio, &mut tx, FRAME_HELLO, &[]).await;
            next_hello = Instant::now() + hello_interval;
            // Wiederholt den Status, damit eine später gestartete Software ihn erfährt.
            send_link(state, to_usb);
        }
        if state.link.get() && Instant::now().duration_since(last_heard) > LINK_TIMEOUT {
            set_link(state, to_usb, false);
        }

        let event = select3(radio.receive(&mut rx), to_radio.receive(), Timer::at(next_hello)).await;
        match event {
            Either3::First(Ok(())) if rx.len() >= 3 && rx[..2] == MAGIC => {
                last_heard = Instant::now();
                if !state.link.get() {
                    set_link(state, to_usb, true);
                }
                if rx[2] == FRAME_DATA {
                    let mut report = [0; REPORT_LEN];
                    report[0] = TYPE_DATA;
                    let data = &rx[3..];
                    let length = data.len().min(PAYLOAD_LEN);
                    report[1..1 + length].copy_from_slice(&data[..length]);
                    let _ = to_usb.try_send(report);
                }
            }
            Either3::Second(report) => send_frame(&mut radio, &mut tx, FRAME_DATA, &report[1..]).await,
            _ => {}
        }
    }
}

async fn usb_reader<'d, D: Driver<'d>>(
    mut reader: HidReader<'d, D, REPORT_LEN>,
    state: &State,
    to_radio: &ReportChannel,
) {
    loop {
        reader.ready().await;
        let mut report = [0; REPORT_LEN];
        while reader.read(&mut report).await.is_ok() {
            match report[0] {
                TYPE_DATA => {
                    // Wartet auf freien Platz statt zu verwerfen; der USB-Host wird so gebremst.
                    to_radio.send(report).await;
                }
                TYPE_STATUS => {
                    state.authenticated.set(report[1] == 1 && state.link.get());
                    state.changed.signal(());
                }
                _ => {}
            }
            report = [0; REPORT_LEN];
        }
    }
}

async fn usb_writer<'d, D: Driver<'d>>(mut writer: HidWriter<'d, D, REPORT_LEN>, to_usb: &ReportChannel) {
    loop {
        writer.ready().await;
        loop {
            let report = to_usb.receive().await;
            if writer.write(&report).await.is_err() {
                break;
            }
        }
    }
}

// Rot = kein Funk, Blau = Funk steht, Grün = Software hat den Partner authentifiziert. Aktiv-low.
async fn led_loop(state: &State, mut red: Output<'_>, mut green: Output<'_>, mut blue: Output<'_>) {
    loop {
        let link = state.link.get();
        let authenticated = state.authenticated.get();
        red.set_level(if link { Level::High } else { Level::Low });
        blue.set_level(if link && !authenticated { Level::Low } else { Level::High });
        green.set_level(if link && authenticated { Level::Low } else { Level::High });
        state.changed.wait().await;
    }
}

#[embassy_executor::main]
async fn main(_spawner: Spawner) {
    let mut nrf_config = NrfConfig::default();
    nrf_config.hfclk_source = HfclkSource::ExternalXtal;
    let p = embassy_nrf::init(nrf_config);

    let red = Output::new(p.P0_08, Level::Low, OutputDrive::Standard);
    let green = Output::new(p.P1_09, Level::High, OutputDrive::Standard);
    let blue = Output::new(p.P0_12, Level::High, OutputDrive::Standard);

    let vbus = usb::vbus_detect::SoftwareVbusDetect::new(true, true);
    let driver = usb::Driver::new(p.USBD, Irqs, &vbus);

    let mut config = Config::new(0x1915, 0x5211);
    config.manufacturer = Some("Shared Control");
    config.product = Some("Shared Control Dongle");
    config.max_power = 100;

    let mut config_descriptor = [0; 64];
    let mut bos_descriptor = [0; 32];
    let mut msos_descriptor = [0; 0];
    let mut control_buf = [0; 64];
    let mut hid_state = HidState::new();
    let mut builder = Builder::new(
        driver,
        config,
        &mut config_descriptor,
        &mut bos_descriptor,
        &mut msos_descriptor,
        &mut control_buf,
    );
    let hid_config = HidConfig {
        report_descriptor: REPORT_DESCRIPTOR,
        request_handler: None,
        poll_ms: 1,
        max_packet_size: 64,
        hid_subclass: HidSubclass::No,
        hid_boot_protocol: HidBootProtocol::None,
    };
    let (reader, writer) = HidReaderWriter::<_, REPORT_LEN, REPORT_LEN>::new(&mut builder, &mut hid_state, hid_config).split();
    let mut device = builder.build();

    let mut radio = Radio::new(p.RADIO, Irqs);
    radio.set_channel(RADIO_CHANNEL);
    // Pro Dongle ein anderes Intervall, damit zwei Dongles nicht dauerhaft gleichzeitig senden.
    let hello_interval = Duration::from_millis(400 + u64::from(embassy_nrf::pac::FICR.deviceid(0).read() % 200));

    let state = State {
        link: Cell::new(false),
        authenticated: Cell::new(false),
        changed: Signal::new(),
    };
    let to_radio = ReportChannel::new();
    let to_usb = ReportChannel::new();

    join5(
        device.run(),
        usb_reader(reader, &state, &to_radio),
        usb_writer(writer, &to_usb),
        radio_loop(radio, &state, &to_radio, &to_usb, hello_interval),
        led_loop(&state, red, green, blue),
    )
    .await;
}
