use anyhow::{Context, Result};
use clap::Parser;
use num::complex::Complex32 as c32;
use soapysdr::{Device, Direction};
use std::io::{self, Write};

const SAMPLE_RATE: f64 = 6_000_000.0;
const FREQUENCY: f64 = 433_815_598.0;
const BANDWIDTH: f64 = 6_000_000.0;

// Block size for amplitude averaging: 300 samples = 50 µs
// Well below pulse width (260 µs) but enough to smooth noise
const BLOCK_SIZE: usize = 300;

// Timing thresholds in blocks (1 block = 50 µs)
const PULSE_MIN_BLOCKS: usize = 3; // 150 µs — reject noise
const GAP_THRESHOLD: usize = 14; // 700 µs — below = bit 0, above = bit 1
const GAP_FRAME_BLOCKS: usize = 40; // 2000 µs — frame separator

#[derive(Parser)]
#[command(about = "Listen for fan remote OOK codes via SoapySDR")]
struct Args {
    /// RX gain in dB
    #[arg(short, long, default_value_t = 40.0)]
    gain: f64,

    /// SoapySDR driver name (e.g. hackrf, bladerf)
    #[arg(short, long)]
    driver: String,

    /// Print amplitude stats for threshold calibration
    #[arg(long)]
    calibrate: bool,
}

struct OokDecoder {
    // Accumulator for partial blocks
    block_acc: f32,
    block_count: usize,

    // Adaptive threshold
    noise_floor: f32,
    signal_peak: f32,
    threshold: f32,

    // State machine
    in_pulse: bool,
    pulse_blocks: usize,
    gap_blocks: usize,
    bits: Vec<u8>,
    last_code: Option<u32>,

    calibrate: bool,
    sample_count: u64,
}

impl OokDecoder {
    fn new(calibrate: bool) -> Self {
        Self {
            block_acc: 0.0,
            block_count: 0,
            noise_floor: 0.0,
            signal_peak: 0.0,
            threshold: 0.0,
            in_pulse: false,
            pulse_blocks: 0,
            gap_blocks: 0,
            bits: Vec::new(),
            last_code: None,
            calibrate,
            sample_count: 0,
        }
    }

    fn process(&mut self, samples: &[c32]) {
        for sample in samples {
            self.block_acc += sample.norm();
            self.block_count += 1;
            self.sample_count += 1;

            if self.block_count == BLOCK_SIZE {
                let block_amp = self.block_acc / BLOCK_SIZE as f32;
                self.block_acc = 0.0;
                self.block_count = 0;
                self.process_block(block_amp);
            }
        }
    }

    fn process_block(&mut self, amp: f32) {
        // Seed the detector from the first block of ambient samples. Starting
        // at zero makes ordinary SDR noise look like one continuous pulse, so
        // the decoder never gets a chance to learn the actual noise floor.
        if self.noise_floor == 0.0 {
            self.noise_floor = amp;
            self.threshold = amp * 3.0;
            return;
        }

        // Adaptive threshold: track noise floor with slow decay
        if !self.in_pulse {
            self.noise_floor = self.noise_floor * 0.999 + amp * 0.001;
        } else {
            self.signal_peak = self.signal_peak * 0.99 + amp * 0.01;
        }
        // Threshold midpoint between noise and signal (on log scale effectively)
        if self.signal_peak > self.noise_floor * 2.0 {
            self.threshold = (self.noise_floor + self.signal_peak) * 0.5;
        } else {
            self.threshold = self.noise_floor * 3.0;
        }

        let signal = amp > self.threshold;

        if self.calibrate && self.sample_count.is_multiple_of(BLOCK_SIZE as u64 * 1000) {
            eprintln!(
                "amp={amp:.4} noise={:.4} peak={:.4} thresh={:.4}",
                self.noise_floor, self.signal_peak, self.threshold
            );
        }

        if signal {
            if !self.in_pulse {
                // Rising edge — classify the gap
                if self.pulse_blocks >= PULSE_MIN_BLOCKS && self.gap_blocks > 0 {
                    if self.gap_blocks >= GAP_FRAME_BLOCKS {
                        self.emit_frame();
                    } else if self.gap_blocks >= GAP_THRESHOLD {
                        self.bits.push(1);
                    } else {
                        self.bits.push(0);
                    }
                }
                self.gap_blocks = 0;
            }
            self.in_pulse = true;
            self.pulse_blocks += 1;
        } else {
            if self.in_pulse && self.pulse_blocks < PULSE_MIN_BLOCKS {
                // Pulse too short — noise glitch, reset
                self.pulse_blocks = 0;
            }
            self.in_pulse = false;
            self.gap_blocks += 1;

            // Long silence with pending bits → emit
            if self.gap_blocks == GAP_FRAME_BLOCKS && !self.bits.is_empty() {
                self.emit_frame();
            }
        }
    }

    fn emit_frame(&mut self) {
        self.pulse_blocks = 0;
        let n_bits = self.bits.len();
        if n_bits != 32 {
            self.bits.clear();
            return;
        }

        let mut code: u64 = 0;
        for &bit in &self.bits {
            code = (code << 1) | bit as u64;
        }
        self.bits.clear();

        // Deduplicate repeated frames within same press
        if self.last_code == Some(code as u32) {
            return;
        }
        self.last_code = Some(code as u32);

        let device_id = code >> 12;
        println!("0x{code:08X}  device_id=0x{device_id:05X}");
        let _ = io::stdout().flush();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feed_blocks(decoder: &mut OokDecoder, amplitude: f32, count: usize) {
        for _ in 0..count {
            decoder.process_block(amplitude);
        }
    }

    #[test]
    fn seeds_noise_floor_before_detecting_pulses() {
        let mut decoder = OokDecoder::new(false);

        decoder.process_block(0.01);

        assert_eq!(decoder.noise_floor, 0.01);
        assert_eq!(decoder.threshold, 0.03);
        assert!(!decoder.in_pulse);
    }

    #[test]
    fn decodes_vendor_a_frame_after_noise_bootstrap() {
        let mut decoder = OokDecoder::new(false);
        let code = 0x87AD_7105_u32;

        feed_blocks(&mut decoder, 0.01, 100);
        for bit_idx in (0..32).rev() {
            feed_blocks(&mut decoder, 1.0, 6);
            let gap_blocks = if (code >> bit_idx) & 1 == 1 { 30 } else { 10 };
            feed_blocks(&mut decoder, 0.01, gap_blocks);
        }

        // Vendor A ends each frame with one extra pulse followed by its frame
        // gap. The rising edge records the 32nd bit; the gap emits the frame.
        feed_blocks(&mut decoder, 1.0, 6);
        feed_blocks(&mut decoder, 0.01, GAP_FRAME_BLOCKS);

        assert_eq!(decoder.last_code, Some(code));
    }
}

fn main() -> Result<()> {
    let args = Args::parse();

    let dev = Device::new(format!("driver={}", args.driver).as_str())
        .context("Failed to open SoapySDR device")?;

    let ch = 0;
    dev.set_frequency(Direction::Rx, ch, FREQUENCY, ())
        .context("Failed to set RX frequency")?;
    dev.set_sample_rate(Direction::Rx, ch, SAMPLE_RATE)
        .context("Failed to set RX sample rate")?;
    dev.set_bandwidth(Direction::Rx, ch, BANDWIDTH)
        .context("Failed to set RX bandwidth")?;
    dev.set_gain(Direction::Rx, ch, args.gain)
        .context("Failed to set RX gain")?;

    let mut stream = dev
        .rx_stream::<c32>(&[ch])
        .context("Failed to create RX stream")?;

    stream
        .activate(None)
        .context("Failed to activate RX stream")?;

    println!("Listening on {:.6} MHz...", FREQUENCY / 1e6);
    if args.calibrate {
        eprintln!("Calibration mode: printing amplitude stats");
    }

    let mut decoder = OokDecoder::new(args.calibrate);
    let mut buf = vec![c32::new(0.0, 0.0); 65536];

    loop {
        match stream.read(&mut [&mut buf], 1_000_000) {
            Ok(n) => decoder.process(&buf[..n]),
            Err(e) if e.code == soapysdr::ErrorCode::Overflow => continue,
            Err(e) => {
                eprintln!("Read error: {e}");
                continue;
            }
        }
    }
}
