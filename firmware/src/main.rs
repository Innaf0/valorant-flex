#![no_std]
#![no_main]
mod audio;
mod display;
mod irqs;
mod request;
mod sd_card;
mod touch;

use core::cell::RefCell;

use defmt::*;
use embassy_executor::Spawner;
use embassy_rp::gpio::{Input, Level, Output, Pull};
use embassy_rp::peripherals::SPI1;
use embassy_rp::pio::Pio;
use embassy_rp::spi::{Blocking, Spi};
use embassy_time::{Duration, Timer};
use embedded_hal_bus::spi::{NoDelay, RefCellDevice};
use static_cell::StaticCell;
use {defmt_rtt as _, panic_probe as _};

use crate::irqs::Irqs;

#[embassy_executor::main]
async fn main(spawner: Spawner) {
    let p = embassy_rp::init(Default::default());

    // ── SD card on SPI0 ──────────────────────────────────────
    let mut sd_handle = sd_card::init_sd_card(
        p.SPI0,   // SPI peripheral
        p.PIN_6,  // SCK  (SD_CLK / SPI0 SCK)
        p.PIN_7,  // MOSI (SD_MOSI / SPI0 TX)
        p.PIN_4,  // MISO (SD_MISO / SPI0 RX)
        p.PIN_21, // CS   (SD_CS)
    );
    let config = sd_handle.read_config();

    // ─ WiFi (CYW43) ─────────────────────────────────────────
    let ctx = request::init_network(
        spawner,
        config.wifi_ssid.as_str(),
        config.wifi_password.as_str(),
        config.api_key.as_str(),
        config.player_region.as_str(),
        config.player_name.as_str(),
        config.player_tag.as_str(),
        p.PIN_23,
        p.PIN_25,
        p.PIN_24,
        p.PIN_29,
        p.PIO0,
        p.DMA_CH0,
    )
    .await;

    // ── Shared SPI1 bus for display + touch ─────────────────
    let mut spi_config = embassy_rp::spi::Config::default();
    spi_config.frequency = 64_000_000;

    let spi_bus = Spi::new_blocking(
        p.SPI1, p.PIN_14, // SCK  (DISP_SCK / DISP_T_CLK)
        p.PIN_11, // MOSI (DISP_MOSI / DISP_T_MOSI)
        p.PIN_12, // MISO (DISP_MISO / DISP_T_DO)
        spi_config,
    );

    static SPI_BUS: StaticCell<RefCell<Spi<'static, SPI1, Blocking>>> = StaticCell::new();
    let spi_bus = SPI_BUS.init(RefCell::new(spi_bus));

    // Display CS: GPIO13, Touch CS: GPIO9, Touch IRQ: GPIO8
    let display_spi = RefCellDevice::new(spi_bus, Output::new(p.PIN_13, Level::High), NoDelay);
    let touch_spi = RefCellDevice::new(spi_bus, Output::new(p.PIN_9, Level::High), NoDelay);

    // ── ILI9341 display ─────────────────────────────────────
    let mut display = display::init_display(
        display_spi,
        p.PIN_5,  // DC   (DISP_RS/DC)
        p.PIN_15, // RST  (DISP_RESET)
    );

    // ── Touch controller (shares SPI1 with display) ──────────
    let mut touch = touch::Touch::new(touch_spi, p.PIN_8); // IRQ  (DISP_T_IRQ)

    // ── I2S audio player on PIO1 ─────────────────────────────
    const SAMPLE_RATE: u32 = 48_000;
    const BIT_DEPTH: u32 = 16;

    let Pio {
        mut common, sm0, ..
    } = Pio::new(p.PIO1, Irqs);

    let mut player = audio::I2sPlayer::new(
        &mut common,
        sm0,
        p.DMA_CH4,
        Irqs,
        p.PIN_10, // DIN  (DAC_DIN)
        p.PIN_1,  // BCK  (DAC_BCK)
        p.PIN_0,  // WSEL (DAC_WSEL)
        SAMPLE_RATE,
        BIT_DEPTH,
    );

    // ── Buttons (active low) ─────────────────────────────────
    let play_button = Input::new(p.PIN_16, Pull::Up); // SW1
    let _btn2 = Input::new(p.PIN_20, Pull::Up); // SW2
    let _btn3 = Input::new(p.PIN_22, Pull::Up); // SW3

    info!(
        "Ready. Press SW1 (GPIO16) to play AUDIO.WAV ({} Hz / {}-bit).",
        SAMPLE_RATE, BIT_DEPTH
    );

    loop {
        // Check for button press
        if play_button.is_low() {
            info!("Button pressed – playing AUDIO.WAV ...");

            let result = player.play_wav(&mut sd_handle, "AUDIO.WAV").await;

            match result {
                Ok(()) => info!("Playback finished successfully."),
                Err(e) => warn!("Playback error: {:?}", e),
            }

            // Wait for button release to avoid retriggering
            while play_button.is_low() {
                Timer::after(Duration::from_millis(20)).await;
            }
            info!("Button released.");
        }

        // ── Check for touch input ────────────────────────────
        if touch.is_touched() {
            if let Some((x, y)) = touch.read() {
                info!("Touch at ({}, {})", x, y);
            }
        }

        // ── Fetch latest match and update display ────────────
        match request::fetch_match(&ctx).await {
            Some(m) => {
                info!("=== Match ===");
                info!("Map:         {}", m.map.as_str());
                info!("Mode:        {}", m.mode.as_str());
                info!("Region:      {}", m.region.as_str());
                info!("Started:     {}", m.game_start.as_str());
                info!("Rounds:      {}", m.rounds_played);
                info!(
                    "Score:       Red {} - {} Blue",
                    m.red_rounds_won, m.blue_rounds_won,
                );
                info!(
                    "Winner:      {}",
                    if m.red_has_won { "Red" } else { "Blue" }
                );

                info!("=== Players ===");
                for player in &m.players {
                    info!(
                        "  [{}] {} #{} | {} | {} | K/D/A: {}/{}/{} | HS: {}",
                        player.team.as_str(),
                        player.name.as_str(),
                        player.tag.as_str(),
                        player.character.as_str(),
                        player.rank.as_str(),
                        player.kills,
                        player.deaths,
                        player.assists,
                        player.headshots,
                    );
                }

                // Draw the match on the display
                display::draw_match(&mut display, &m);
            }
            None => {
                warn!("Failed to fetch match data, retrying...");
            }
        }

        Timer::after(Duration::from_secs(30)).await;
    }
}
