#![no_std]
#![no_main]

use embassy_executor::Spawner;
use embassy_nrf::gpio::{Level, Output, OutputDrive};
use embassy_time::Timer;
use panic_halt as _;

#[embassy_executor::main]
async fn main(_spawner: Spawner) {
    let p = embassy_nrf::init(Default::default());
    // Aktiv-low (verifiziert). Phase 1: Busy-Wait ohne Interrupt. Phase 2 (Blau an): Timer-Interrupt.
    let mut led = Output::new(p.P0_06, Level::Low, OutputDrive::Standard);
    let _green = Output::new(p.P1_09, Level::Low, OutputDrive::Standard);

    for _ in 0..6 {
        cortex_m::asm::delay(32_000_000);
        led.toggle();
    }

    let _blue = Output::new(p.P0_12, Level::Low, OutputDrive::Standard);
    loop {
        Timer::after_millis(500).await;
        led.toggle();
    }
}
