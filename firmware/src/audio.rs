//! I2S audio playback from WAV files stored on the SD card.
//!
//! Connects to an I2S DAC on:
//!   wsel : GPIO 0  (DAC_WSEL)
//!   bclk : GPIO 1  (DAC_BCK)
//!   din  : GPIO 9  (DAC_DIN)
//!
//! DAC I2C control on GPIO 24 (SDA) / GPIO 25 (SCL).
//! DAC reset on GPIO 18 (DAC_RST).
//!
//! Uses a PIO state machine and DMA channel to stream PCM data
//! with double-buffering. All types are generic — the caller
//! specifies which PIO, state machine, DMA channel, and GPIO
//! pins to use.

use core::mem;

use defmt::*;
use embassy_rp::dma;
use embassy_rp::interrupt::typelevel::Binding;
use embassy_rp::pio::{Common, Instance, PioPin, StateMachine};
use embassy_rp::pio_programs::i2s::{PioI2sOut, PioI2sOutProgram};
use embassy_rp::spi::Instance as SpiInstance;
use embassy_rp::Peri;
use embedded_sdmmc::{Mode, VolumeIdx};
use static_cell::StaticCell;

use crate::sd_card::SdCardHandle;

// --- Constants ---

const BUFFER_SIZE: usize = 960;

// --- Public types ---

#[derive(Debug, defmt::Format)]
pub enum AudioError {
    FileNotFound,
    InvalidWav,
    UnsupportedFormat,
    ReadError,
}

pub struct WavInfo {
    pub sample_rate: u32,
    pub bits_per_sample: u16,
    pub num_channels: u16,
}

/// Generic I2S audio player.
///
/// `P` is the PIO peripheral (e.g. `PIO0` or `PIO1`),
/// `S` is the state machine index (0–3).
pub struct I2sPlayer<'d, P: Instance, const S: usize> {
    i2s: PioI2sOut<'d, P, S>,
    front_buffer: &'static mut [u32],
    back_buffer: &'static mut [u32],
    raw_buf: &'static mut [u8],
}

impl<'d, P: Instance, const S: usize> I2sPlayer<'d, P, S> {
    /// Initialise the I2S PIO state machine and DMA, and allocate
    /// double-buffers.
    ///
    /// The caller is responsible for creating the `Common` and
    /// `StateMachine` from a `Pio::new(…)` call, and for providing
    /// an IRQ struct that binds the DMA channel's interrupt.
    pub fn new<D: dma::ChannelInstance>(
        common: &mut Common<'d, P>,
        sm: StateMachine<'d, P, S>,
        dma_ch: Peri<'d, D>,
        irq: impl Binding<D::Interrupt, dma::InterruptHandler<D>> + 'd,
        data_pin: Peri<'d, impl PioPin>,
        bit_clock_pin: Peri<'d, impl PioPin>,
        lr_clock_pin: Peri<'d, impl PioPin>,
        sample_rate: u32,
        bit_depth: u32,
    ) -> Self {
        let program = PioI2sOutProgram::new(common);
        let mut i2s = PioI2sOut::new(
            common,
            sm,
            dma_ch,
            irq,
            data_pin,
            bit_clock_pin,
            lr_clock_pin,
            sample_rate,
            bit_depth,
            &program,
        );
        i2s.start();

        static DMA_BUFFER: StaticCell<[u32; BUFFER_SIZE * 2]> = StaticCell::new();
        let dma_buffer = DMA_BUFFER.init_with(|| [0u32; BUFFER_SIZE * 2]);
        let (back_buffer, front_buffer) = dma_buffer.split_at_mut(BUFFER_SIZE);

        static RAW_BUF: StaticCell<[u8; 4096]> = StaticCell::new();
        let raw_buf = RAW_BUF.init([0u8; 4096]);

        I2sPlayer {
            i2s,
            front_buffer,
            back_buffer,
            raw_buf,
        }
    }

    /// Play a WAV file from the SD card through this I2S output.
    ///
    /// The file's sample rate and bit depth must match the values
    /// this `I2sPlayer` was initialised with.
    ///
    /// `SPI` is the SPI bus used for the SD card (inferred from the handle).
    pub async fn play_wav<SPI: SpiInstance + 'static>(
        &mut self,
        sd_handle: &mut SdCardHandle<SPI>,
        filename: &str,
    ) -> Result<(), AudioError> {
        // ── Open the WAV file ─────────────────────────────────

        let volume_mgr = sd_handle.volume_mgr();

        let mut volume0 = volume_mgr
            .open_volume(VolumeIdx(0))
            .map_err(|_| AudioError::ReadError)?;
        let mut root_dir = volume0.open_root_dir().map_err(|_| AudioError::ReadError)?;
        let mut file = root_dir
            .open_file_in_dir(filename, Mode::ReadOnly)
            .map_err(|_| AudioError::FileNotFound)?;

        // Wrap file.read() so helpers don't need the File type
        let mut reader = |buf: &mut [u8]| -> Result<usize, AudioError> {
            file.read(buf).map_err(|_| AudioError::ReadError)
        };

        // ── Parse WAV header ──────────────────────────────────

        let info = parse_wav_header(&mut reader)?;

        info!(
            "WAV: {} Hz, {}-bit, {} ch",
            info.sample_rate, info.bits_per_sample, info.num_channels,
        );

        // ── Stream audio ──────────────────────────────────────

        let bps = (info.bits_per_sample / 8) as usize;

        let samples_read = fill_i2s_buffer(
            &mut reader,
            self.raw_buf,
            self.front_buffer,
            info.num_channels,
            bps,
        );
        if samples_read == 0 {
            return Err(AudioError::InvalidWav);
        }

        loop {
            let dma_future = self.i2s.write(self.front_buffer);
            let back_samples = fill_i2s_buffer(
                &mut reader,
                self.raw_buf,
                self.back_buffer,
                info.num_channels,
                bps,
            );
            dma_future.await;

            if back_samples == 0 {
                break;
            }

            mem::swap(&mut self.back_buffer, &mut self.front_buffer);
        }

        info!("Audio playback complete");
        Ok(())
    }
}

// ─────────────────────────────────────────────────────────────────────
//  Helpers
// ─────────────────────────────────────────────────────────────────────

fn parse_wav_header(
    reader: &mut impl FnMut(&mut [u8]) -> Result<usize, AudioError>,
) -> Result<WavInfo, AudioError> {
    let mut buf = [0u8; 44];

    reader(&mut buf[..12])?;
    if &buf[0..4] != b"RIFF" || &buf[8..12] != b"WAVE" {
        return Err(AudioError::InvalidWav);
    }

    let mut sample_rate = 0u32;
    let mut bits_per_sample = 0u16;
    let mut num_channels = 0u16;
    let mut found_fmt = false;
    let mut found_data = false;

    loop {
        let n = reader(&mut buf[..8])?;
        if n < 8 {
            break;
        }

        let chunk_size = u32::from_le_bytes([buf[4], buf[5], buf[6], buf[7]]) as usize;

        match &buf[0..4] {
            b"fmt " => {
                let read_size = chunk_size.min(40);
                reader(&mut buf[..read_size])?;

                if u16::from_le_bytes([buf[0], buf[1]]) != 1 {
                    return Err(AudioError::UnsupportedFormat);
                }
                num_channels = u16::from_le_bytes([buf[2], buf[3]]);
                sample_rate = u32::from_le_bytes([buf[4], buf[5], buf[6], buf[7]]);
                bits_per_sample = u16::from_le_bytes([buf[14], buf[15]]);

                if chunk_size > 16 {
                    discard_bytes(reader, chunk_size - 16)?;
                }
                found_fmt = true;
            }
            b"data" => {
                found_data = true;
                break;
            }
            _ => {
                discard_bytes(reader, chunk_size)?;
            }
        }
    }

    if !found_fmt || !found_data {
        return Err(AudioError::InvalidWav);
    }

    Ok(WavInfo {
        sample_rate,
        bits_per_sample,
        num_channels,
    })
}

fn fill_i2s_buffer(
    reader: &mut impl FnMut(&mut [u8]) -> Result<usize, AudioError>,
    raw_buf: &mut [u8],
    buffer: &mut [u32],
    num_channels: u16,
    bytes_per_sample: usize,
) -> usize {
    let max_bytes = buffer.len() * num_channels as usize * bytes_per_sample;
    let to_read = max_bytes.min(raw_buf.len());
    let bytes_read = reader(&mut raw_buf[..to_read]).unwrap_or(0);

    let frame_bytes = num_channels as usize * bytes_per_sample;
    let total_samples = bytes_read / frame_bytes;

    for i in 0..total_samples {
        let offset = i * frame_bytes;
        if num_channels == 1 {
            let sample = read_pcm_sample(&raw_buf[offset..], bytes_per_sample);
            buffer[i] = (sample as u32).wrapping_mul(0x10001);
        } else {
            let left = read_pcm_sample(&raw_buf[offset..], bytes_per_sample);
            let right = read_pcm_sample(&raw_buf[offset + bytes_per_sample..], bytes_per_sample);
            buffer[i] = (left as u32) | ((right as u32) << 16);
        }
    }

    for i in total_samples..buffer.len() {
        buffer[i] = 0x8000_8000;
    }

    total_samples
}

fn discard_bytes(
    reader: &mut impl FnMut(&mut [u8]) -> Result<usize, AudioError>,
    mut n: usize,
) -> Result<(), AudioError> {
    let mut dummy = [0u8; 64];
    while n > 0 {
        let to_read = n.min(dummy.len());
        reader(&mut dummy[..to_read])?;
        n -= to_read;
    }
    Ok(())
}

fn read_pcm_sample(bytes: &[u8], bytes_per_sample: usize) -> u16 {
    match bytes_per_sample {
        2 => {
            let sample = i16::from_le_bytes([bytes[0], bytes[1]]);
            (sample as u16) ^ 0x8000
        }
        1 => (bytes[0] as u16) << 8,
        3 => {
            let sign_byte = if bytes[2] & 0x80 != 0 { 0xFFu8 } else { 0x00 };
            let sample = i32::from_le_bytes([bytes[0], bytes[1], bytes[2], sign_byte]);
            ((sample >> 8) as i16 as u16) ^ 0x8000
        }
        _ => 0x8000,
    }
}
