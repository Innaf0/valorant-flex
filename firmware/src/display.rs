//! ILI9341 TFT display driver via SPI.
//!
//! Initializes the display and provides helpers to draw match info.
//! All hardware types are generic — the caller decides which SPI bus
//! and which GPIO pins to use.

use core::fmt::Write;

use defmt::*;
use embassy_rp::gpio::{Level, Output, Pin};
use embassy_rp::spi::{Blocking, ClkPin, Instance as SpiInstance, MisoPin, MosiPin, Spi};
use embassy_rp::{spi, Peri};
use embedded_graphics::mono_font::iso_8859_1::FONT_6X10;
use embedded_graphics::mono_font::MonoTextStyle;
use embedded_graphics::pixelcolor::Rgb565;
use embedded_graphics::prelude::*;
use embedded_graphics::primitives::{PrimitiveStyle, Rectangle};
use embedded_graphics::text::Text;
use embedded_hal_bus::spi::ExclusiveDevice;
use heapless::String;
use mipidsi::interface::SpiInterface;
use mipidsi::models::ILI9341Rgb565;
use mipidsi::options::ColorOrder;
use mipidsi::Builder;
use static_cell::StaticCell;

use crate::request::MatchInfo;

// --- Public API ---

/// Initialise the ILI9341 display over SPI.
///
/// Returns the display object ready for drawing.  Pass `&mut display` to
/// [`draw_match`] to render match data.
///
/// `SPI` is the SPI peripheral (e.g. `SPI0` or `SPI1`).
pub fn init_display<SPI: SpiInstance + 'static>(
    spi: Peri<'static, SPI>,
    sclk: Peri<'static, impl ClkPin<SPI>>,
    mosi: Peri<'static, impl MosiPin<SPI>>,
    miso: Peri<'static, impl MisoPin<SPI>>,
    dc: Peri<'static, impl Pin>,
    cs: Peri<'static, impl Pin>,
    rst: Peri<'static, impl Pin>,
) -> impl DrawTarget<Color = Rgb565> {
    static SPI_BUF: StaticCell<[u8; 256]> = StaticCell::new();

    let mut spi_config = spi::Config::default();
    spi_config.frequency = 64_000_000;

    let spi = Spi::new_blocking(spi, sclk, mosi, miso, spi_config);
    let cs = Output::new(cs, Level::High);
    let spi_dev = ExclusiveDevice::new_no_delay(spi, cs);
    let dc = Output::new(dc, Level::Low);
    let di = SpiInterface::new(spi_dev, dc, SPI_BUF.init([0u8; 256]));

    let rst = Output::new(rst, Level::High);

    let mut display = Builder::new(ILI9341Rgb565, di)
        .reset_pin(rst)
        .display_size(240, 320)
        .color_order(ColorOrder::Bgr)
        .init(&mut embassy_time::Delay)
        .unwrap();

    display.clear(Rgb565::BLACK).unwrap();
    info!("Display initialised");

    display
}

/// Draw a full match summary on screen.
pub fn draw_match(display: &mut impl DrawTarget<Color = Rgb565>, m: &MatchInfo) {
    let _ = display.clear(Rgb565::BLACK);

    let style = MonoTextStyle::new(&FONT_6X10, Rgb565::WHITE);
    let small_style = MonoTextStyle::new(&FONT_6X10, Rgb565::CSS_DARK_GRAY);

    let mut y = 10i32;

    // --- Header line: "Map | Mode" ---
    let mut header: String<32> = String::new();
    let _ = core::write!(header, "{} | {}", m.map.as_str(), m.mode.as_str());
    let _ = Text::new(header.as_str(), Point::new(10, y), style).draw(display);
    y += 14;

    // --- Score ---
    let mut score: String<32> = String::new();
    let _ = core::write!(
        score,
        "Red {} - {} Blue",
        m.red_rounds_won,
        m.blue_rounds_won
    );
    let _ = Text::new(score.as_str(), Point::new(10, y), style).draw(display);
    y += 14;

    // --- Winner ---
    let winner = if m.red_has_won {
        "Winner: Red"
    } else {
        "Winner: Blue"
    };
    let _ = Text::new(winner, Point::new(10, y), style).draw(display);
    y += 16;

    // --- Separator ---
    let _ = Rectangle::new(Point::new(10, y), Size::new(220, 1))
        .into_styled(PrimitiveStyle::with_fill(Rgb565::WHITE))
        .draw(display);
    y += 8;

    // --- Players ---
    for player in &m.players {
        if y > 300 {
            break; // screen full
        }
        let mut line: String<48> = String::new();
        let _ = core::write!(
            line,
            "{} ({}) {}/{}",
            player.name.as_str(),
            player.character.as_str(),
            player.kills,
            player.deaths,
        );
        let _ = Text::new(line.as_str(), Point::new(10, y), small_style).draw(display);
        y += 12;
    }
}
