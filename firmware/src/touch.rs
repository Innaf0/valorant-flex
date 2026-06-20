//! Touch screen driver (XPT2046/ILI9341 integrated touch).
//!
//! Shares the SPI bus with the display. Uses a separate CS pin and
//! optional IRQ pin to detect touches.

use embassy_rp::gpio::{Input, Pin, Pull};
use embassy_rp::Peri;
use embedded_hal_1::spi::{Operation, SpiDevice};

struct Calibration {
    x1: i32,
    x2: i32,
    y1: i32,
    y2: i32,
    sx: i32,
    sy: i32,
}

const CALIBRATION: Calibration = Calibration {
    x1: 3880,
    x2: 340,
    y1: 262,
    y2: 3850,
    sx: 320,
    sy: 240,
};

pub struct Touch<SPI: SpiDevice> {
    spi: SPI,
    _irq: Input<'static>,
}

impl<SPI> Touch<SPI>
where
    SPI: SpiDevice,
{
    pub fn new(spi: SPI, irq_pin: Peri<'static, impl Pin>) -> Self {
        let irq = Input::new(irq_pin, Pull::Up);
        Self { spi, _irq: irq }
    }

    /// Check if the screen is being touched (IRQ pin is low).
    pub fn is_touched(&self) -> bool {
        self._irq.is_low()
    }

    /// Read raw touch coordinates and return calibrated (x, y) in pixels.
    /// Returns `None` if no touch is detected.
    pub fn read(&mut self) -> Option<(i32, i32)> {
        let mut x = [0; 2];
        let mut y = [0; 2];
        self.spi
            .transaction(&mut [
                Operation::Write(&[0x90]),
                Operation::Read(&mut x),
                Operation::Write(&[0xd0]),
                Operation::Read(&mut y),
            ])
            .unwrap();

        let x = (u16::from_be_bytes(x) >> 3) as i32;
        let y = (u16::from_be_bytes(y) >> 3) as i32;

        let cal = &CALIBRATION;

        let x = ((x - cal.x1) * cal.sx / (cal.x2 - cal.x1)).clamp(0, cal.sx);
        let y = ((y - cal.y1) * cal.sy / (cal.y2 - cal.y1)).clamp(0, cal.sy);

        if x == 0 && y == 0 {
            None
        } else {
            Some((x, y))
        }
    }
}
