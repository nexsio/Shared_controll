//! Phase-0-Prototyp: Frames über eine BLE-GATT-Characteristic austauschen (Echo + Latenz).
//!
//! `ble-poc server` – bietet den Dienst an und echot gültige Frames.
//! `ble-poc client` – sucht den Dienst, sendet Keepalive-Frames und misst die Round-Trip-Zeit.

use std::time::{Duration, Instant};

use bluer::{
    adv::Advertisement,
    gatt::{
        local::{
            characteristic_control, Application, Characteristic, CharacteristicControlEvent,
            CharacteristicNotify, CharacteristicNotifyMethod, CharacteristicWrite,
            CharacteristicWriteMethod, Service,
        },
        remote, CharacteristicReader, CharacteristicWriter,
    },
    AdapterEvent, Uuid,
};
use futures::{future, pin_mut, StreamExt};
use protocol::{decode, encode_into, MessageKind, MAX_PAYLOAD_LEN};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    time::timeout,
};

type AnyResult<T> = Result<T, Box<dyn std::error::Error>>;

const SERVICE_UUID: Uuid = Uuid::from_u128(0x5f0c_a1b2_7d3e_4c11_9a52_0e6b_8d41_c7a0);
const CHARACTERISTIC_UUID: Uuid = Uuid::from_u128(0x5f0c_a1b3_7d3e_4c11_9a52_0e6b_8d41_c7a0);
const LOCAL_NAME: &str = "SharedCtl";
const TEST_ROUNDS: u64 = 100;

#[tokio::main]
async fn main() -> AnyResult<()> {
    match std::env::args().nth(1).as_deref() {
        Some("server") => server().await,
        Some("client") => client().await,
        _ => {
            eprintln!("Aufruf: ble-poc <server|client>");
            Ok(())
        }
    }
}

async fn server() -> AnyResult<()> {
    let session = bluer::Session::new().await?;
    let adapter = session.default_adapter().await?;
    adapter.set_powered(true).await?;

    let adv = Advertisement {
        service_uuids: [SERVICE_UUID].into_iter().collect(),
        discoverable: Some(true),
        local_name: Some(LOCAL_NAME.to_string()),
        ..Default::default()
    };
    let _adv_handle = adapter.advertise(adv).await?;

    let (char_control, char_handle) = characteristic_control();
    let app = Application {
        services: vec![Service {
            uuid: SERVICE_UUID,
            primary: true,
            characteristics: vec![Characteristic {
                uuid: CHARACTERISTIC_UUID,
                write: Some(CharacteristicWrite {
                    write_without_response: true,
                    encrypt_authenticated_write: true,
                    method: CharacteristicWriteMethod::Io,
                    ..Default::default()
                }),
                notify: Some(CharacteristicNotify {
                    notify: true,
                    method: CharacteristicNotifyMethod::Io,
                    ..Default::default()
                }),
                control_handle: char_handle,
                ..Default::default()
            }],
            ..Default::default()
        }],
        ..Default::default()
    };
    let _app_handle = adapter.serve_gatt_application(app).await?;
    println!(
        "Server bereit auf {} ({}). Beenden mit Strg+C.",
        adapter.name(),
        adapter.address().await?
    );

    let mut read_buf = Vec::new();
    let mut reader: Option<CharacteristicReader> = None;
    let mut writer: Option<CharacteristicWriter> = None;
    pin_mut!(char_control);

    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => break,
            evt = char_control.next() => match evt {
                Some(CharacteristicControlEvent::Write(req)) => {
                    read_buf = vec![0; req.mtu()];
                    reader = Some(req.accept()?);
                }
                Some(CharacteristicControlEvent::Notify(notifier)) => writer = Some(notifier),
                None => break,
            },
            res = async {
                match &mut reader {
                    Some(r) if writer.is_some() => r.read(&mut read_buf).await,
                    _ => future::pending().await,
                }
            } => match res {
                Ok(0) | Err(_) => reader = None,
                Ok(n) => {
                    // Nur gültige Frames werden zurückgesendet; Inhalte werden nicht geloggt.
                    match decode(&read_buf[..n]) {
                        Ok(frame) if frame.payload.len() <= MAX_PAYLOAD_LEN => {
                            if let Some(w) = writer.as_mut() {
                                if w.write_all(&read_buf[..n]).await.is_err() {
                                    writer = None;
                                }
                            }
                        }
                        Ok(_) | Err(_) => eprintln!("Ungültiger Frame verworfen ({n} Bytes)"),
                    }
                }
            },
        }
    }
    println!("Server beendet.");
    Ok(())
}

async fn client() -> AnyResult<()> {
    let session = bluer::Session::new().await?;
    let adapter = session.default_adapter().await?;
    adapter.set_powered(true).await?;

    println!("Suche nach Dienst {SERVICE_UUID} ...");
    let discover = adapter.discover_devices().await?;
    pin_mut!(discover);

    let found = timeout(Duration::from_secs(30), async {
        while let Some(evt) = discover.next().await {
            let AdapterEvent::DeviceAdded(addr) = evt else { continue };
            let device = adapter.device(addr)?;
            let uuids = device.uuids().await?.unwrap_or_default();
            if uuids.contains(&SERVICE_UUID) {
                return Ok::<_, bluer::Error>(Some(device));
            }
        }
        Ok(None)
    })
    .await??;

    let Some(device) = found else {
        return Err("Gerät nicht gefunden".into());
    };
    println!("Gefunden: {}. Verbinde ...", device.address());
    device.connect().await?;
    println!("Gekoppelt: {}", device.is_paired().await?);

    let characteristic = find_characteristic(&device).await?;
    let result = measure(&characteristic).await;
    let _ = device.disconnect().await;
    result
}

async fn find_characteristic(device: &bluer::Device) -> AnyResult<remote::Characteristic> {
    for service in device.services().await? {
        if service.uuid().await? != SERVICE_UUID {
            continue;
        }
        for ch in service.characteristics().await? {
            if ch.uuid().await? == CHARACTERISTIC_UUID {
                return Ok(ch);
            }
        }
    }
    Err("Characteristic nicht gefunden".into())
}

async fn measure(ch: &remote::Characteristic) -> AnyResult<()> {
    let mut write_io = ch.write_io().await?;
    let mut notify_io = ch.notify_io().await?;
    println!("MTU: schreiben {}, lesen {}", write_io.mtu(), notify_io.mtu());

    let mut out = [0u8; 64];
    let mut inbuf = [0u8; 64];
    let mut rtts: Vec<Duration> = Vec::new();
    let mut lost = 0u32;

    for seq in 0..TEST_ROUNDS {
        let payload = seq.to_be_bytes();
        let n = encode_into(MessageKind::Keepalive, &payload, &mut out)
            .map_err(|e| format!("{e:?}"))?;

        let start = Instant::now();
        write_io.write_all(&out[..n]).await?;
        match timeout(Duration::from_secs(2), notify_io.read(&mut inbuf)).await {
            Ok(Ok(len)) => match decode(&inbuf[..len]) {
                Ok(f) if f.kind == MessageKind::Keepalive && f.payload == payload => {
                    rtts.push(start.elapsed());
                }
                _ => lost += 1,
            },
            _ => lost += 1,
        }
    }

    if rtts.is_empty() {
        return Err("Keine Antwort erhalten".into());
    }
    rtts.sort();
    let sum: Duration = rtts.iter().sum();
    println!(
        "{} von {} Frames ok, {} verloren",
        rtts.len(),
        TEST_ROUNDS,
        lost
    );
    println!(
        "Round-Trip: min {:?}, Median {:?}, Mittel {:?}, max {:?}",
        rtts[0],
        rtts[rtts.len() / 2],
        sum / rtts.len() as u32,
        rtts[rtts.len() - 1]
    );
    Ok(())
}
