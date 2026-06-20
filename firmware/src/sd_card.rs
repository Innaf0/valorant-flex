//! SD card initialization, config file reading, and file access.
//!
//! Reads a JSON config file `CONFIG.TXT` from the root of a FAT-formatted SD card
//! and provides access to other files (e.g. WAV audio).
//! Falls back to hardcoded defaults if the card or config file is missing.
//!
//! All hardware types are generic — the caller decides which SPI bus and which
//! GPIO pins to use.

use core::str;

use defmt::*;
use embassy_embedded_hal::SetConfig;
use embassy_rp::gpio::{Level, Output, Pin};
use embassy_rp::spi::{Blocking, ClkPin, Instance as SpiInstance, MisoPin, MosiPin, Spi};
use embassy_rp::{spi, Peri};
use embedded_hal_bus::spi::{ExclusiveDevice, NoDelay};
use embedded_sdmmc::sdcard::{DummyCsPin, SdCard};
use embedded_sdmmc::VolumeManager;
use heapless::String;
use serde::Deserialize;
use serde_json_core::from_slice;

// --- Time source required by embedded-sdmmc ---

pub(crate) struct DummyTimesource();

impl embedded_sdmmc::TimeSource for DummyTimesource {
    fn get_timestamp(&self) -> embedded_sdmmc::Timestamp {
        embedded_sdmmc::Timestamp {
            year_since_1970: 0,
            zero_indexed_month: 0,
            zero_indexed_day: 0,
            hours: 0,
            minutes: 0,
            seconds: 0,
        }
    }
}

// --- Config file JSON schema ---

#[derive(Deserialize)]
struct ConfigFile<'a> {
    wifi_ssid: &'a str,
    wifi_password: &'a str,
    api_key: &'a str,
    player_region: &'a str,
    player_name: &'a str,
    player_tag: &'a str,
}

// --- Public types ---

/// Application configuration loaded from SD card (or defaults).
pub struct Config {
    pub wifi_ssid: String<32>,
    pub wifi_password: String<64>,
    pub api_key: String<64>,
    pub player_region: String<8>,
    pub player_name: String<32>,
    pub player_tag: String<8>,
}

impl Config {
    fn default_config() -> Self {
        Config {
            wifi_ssid: copy_str("ssid"),
            wifi_password: copy_str("pwd"),
            api_key: copy_str("YOUR_API_KEY"),
            player_region: copy_str("eu"),
            player_name: copy_str("PlayerName"),
            player_tag: copy_str("TAG"),
        }
    }
}

// --- Concrete (but private) type alias for convenience ---

type SdVolumeManager<SPI> = VolumeManager<
    SdCard<
        ExclusiveDevice<Spi<'static, SPI, Blocking>, DummyCsPin, NoDelay>,
        Output<'static>,
        embassy_time::Delay,
    >,
    DummyTimesource,
    4,
    4,
    1,
>;

// --- Public API ---

/// Handle that keeps the SD card and filesystem alive so files can be
/// opened later without re-initialising the SPI bus.
///
/// `SPI` is the SPI peripheral used (e.g. `SPI0` or `SPI1`).
pub struct SdCardHandle<SPI: SpiInstance + 'static> {
    volume_mgr: SdVolumeManager<SPI>,
}

/// Initialise the SD card over SPI and return a handle.
///
/// SPI clock is bumped to 16 MHz after card initialisation (400 kHz during init).
/// The caller chooses which SPI bus and which GPIO pins to use.
pub fn init_sd_card<SPI: SpiInstance + 'static>(
    spi: Peri<'static, SPI>,
    sclk: Peri<'static, impl ClkPin<SPI>>,
    mosi: Peri<'static, impl MosiPin<SPI>>,
    miso: Peri<'static, impl MisoPin<SPI>>,
    cs: Peri<'static, impl Pin>,
) -> SdCardHandle<SPI> {
    // SPI clock must be <= 400 kHz during card initialisation
    let mut spi_config = spi::Config::default();
    spi_config.frequency = 400_000;
    let spi = Spi::new_blocking(spi, sclk, mosi, miso, spi_config);
    let spi_dev = ExclusiveDevice::new_no_delay(spi, DummyCsPin);
    let cs = Output::new(cs, Level::High);

    let sdcard = SdCard::new(spi_dev, cs, embassy_time::Delay);

    info!("Card size is {} bytes", sdcard.num_bytes().unwrap_or(0));

    // Now we can bump the SPI clock
    let mut fast_config = spi::Config::default();
    fast_config.frequency = 16_000_000;
    sdcard
        .spi(|dev| SetConfig::set_config(dev.bus_mut(), &fast_config))
        .ok();

    let volume_mgr = VolumeManager::new(sdcard, DummyTimesource());

    SdCardHandle { volume_mgr }
}

impl<SPI: SpiInstance + 'static> SdCardHandle<SPI> {
    /// Read `CONFIG.TXT` from the root of the SD card.
    ///
    /// Returns the parsed config, or built-in defaults if anything fails.
    pub fn read_config(&mut self) -> Config {
        let mut volume0 = match self.volume_mgr.open_volume(embedded_sdmmc::VolumeIdx(0)) {
            Ok(v) => v,
            Err(e) => {
                warn!("Failed to open volume: {:?}", Debug2Format(&e));
                return Config::default_config();
            }
        };

        let mut root_dir = match volume0.open_root_dir() {
            Ok(d) => d,
            Err(e) => {
                warn!("Failed to open root dir: {:?}", Debug2Format(&e));
                return Config::default_config();
            }
        };

        let mut file = match root_dir.open_file_in_dir("CONFIG.TXT", embedded_sdmmc::Mode::ReadOnly)
        {
            Ok(f) => f,
            Err(e) => {
                warn!("Config file not found: {:?}", Debug2Format(&e));
                return Config::default_config();
            }
        };

        let mut buf = [0u8; 512];
        let n = match file.read(&mut buf) {
            Ok(n) => n,
            Err(e) => {
                warn!("Failed to read config: {:?}", Debug2Format(&e));
                return Config::default_config();
            }
        };

        let json = match str::from_utf8(&buf[..n]) {
            Ok(s) => s,
            Err(_) => {
                warn!("Config file is not valid UTF-8");
                return Config::default_config();
            }
        };

        match from_slice::<ConfigFile>(json.as_bytes()) {
            Ok((cfg, _)) => {
                info!("Loaded config from SD card");
                Config {
                    wifi_ssid: copy_str(cfg.wifi_ssid),
                    wifi_password: copy_str(cfg.wifi_password),
                    api_key: copy_str(cfg.api_key),
                    player_region: copy_str(cfg.player_region),
                    player_name: copy_str(cfg.player_name),
                    player_tag: copy_str(cfg.player_tag),
                }
            }
            Err(e) => {
                warn!("Failed to parse config JSON: {:?}", Debug2Format(&e));
                Config::default_config()
            }
        }
    }

    /// Return a mutable reference to the underlying volume manager so
    /// external code can open files directly.
    pub fn volume_mgr(&mut self) -> &mut SdVolumeManager<SPI> {
        &mut self.volume_mgr
    }
}

// --- Helpers ---

fn copy_str<const N: usize>(s: &str) -> String<N> {
    let mut out = String::new();
    out.push_str(s).ok();
    out
}
