//! Centralised interrupt bindings for the whole application.
//!
//! All of the IRQ handlers live here so they are defined exactly once.

use embassy_rp::peripherals::{DMA_CH0, DMA_CH1, DMA_CH4, PIO0, PIO1};
use embassy_rp::pio::InterruptHandler;
use embassy_rp::{bind_interrupts, dma};

bind_interrupts!(pub struct Irqs {
    PIO0_IRQ_0 => InterruptHandler<PIO0>;
    PIO1_IRQ_0 => InterruptHandler<PIO1>;
    DMA_IRQ_0 =>
        dma::InterruptHandler<DMA_CH0>,
        dma::InterruptHandler<DMA_CH1>,
        dma::InterruptHandler<DMA_CH4>;
});
