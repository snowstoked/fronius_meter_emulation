use std::{
    fmt,
    net::SocketAddr,
    time::{Duration, Instant},
};

use client::Context;
use tokio::time::timeout;
use tokio_modbus::{ExceptionCode, prelude::*};

use crate::{
    debug::{debug_log, warn_log},
    meter_data::{Measurements, PhaseMeasurements},
};

/// A stalled Shelly must never stall us. Every network operation is bounded by this.
const IO_TIMEOUT: Duration = Duration::from_secs(3);

// Registers are documented here
// https://shelly-api-docs.shelly.cloud/gen2/ComponentsAndServices/EM/#modbus-registers
// https://shelly-api-docs.shelly.cloud/gen2/ComponentsAndServices/EMData/#modbus-registers
//
// The component registers are listed at 31000+ in the docs, but a Pro 3EM has been seen to refuse those with
// IllegalDataAddress while serving the same layout 30000 lower. We try both and remember which one works.
const BASE_DOCUMENTED: u16 = 31000;
const BASE_ALTERNATIVE: u16 = 1000;

// Offsets within the component block
const REL_TOTAL_ACTIVE_POWER: u16 = 13;
const REL_PHASE_A: u16 = 20;
const REL_PHASE_B: u16 = 40;
const REL_PHASE_C: u16 = 60;
// Per phase: voltage, current, active power, apparent power, power factor (2 registers each).
// The registers after that are error flags, and must not be decoded as floats.
const PHASE_FLOAT_COUNT: u16 = 5;
const REL_PHASE_FREQUENCY: u16 = 13; // relative to the phase, we only read phase A
const REL_ENERGY: u16 = 162; // total import, then total export (Wh)

/// The slow read is split into steps that are run one per fast cycle, so a slow read never delays the power reading
pub const SLOW_STEPS: usize = 5;

pub struct Shelly3EMClient {
    target: SocketAddr,
    connection: Option<Context>,
    base: u16,
    base_confirmed: bool,
}

#[derive(Debug)]
pub enum ReadError {
    Timeout,
    Io(String),
    Exception(ExceptionCode),
}

impl fmt::Display for ReadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ReadError::Timeout => write!(f, "timed out after {IO_TIMEOUT:?}"),
            ReadError::Io(e) => write!(f, "{e}"),
            ReadError::Exception(e) => write!(f, "refused ({e:?})"),
        }
    }
}

impl Shelly3EMClient {
    pub fn new(target: SocketAddr) -> Self {
        Self {
            target,
            connection: None,
            base: BASE_DOCUMENTED,
            base_confirmed: false,
        }
    }

    /// Net power, the only thing that needs reading quickly
    pub async fn read_fast(&mut self) -> Result<Measurements, ReadError> {
        let regs = self.read_component(REL_TOTAL_ACTIVE_POWER, 2).await?;
        Ok(Measurements {
            total_active_power: Some(merge_u16_f32(regs[0], regs[1])),
            ..Default::default()
        })
    }

    /// One piece of the slow data, see `SLOW_STEPS`
    pub async fn read_slow_step(&mut self, step: usize) -> Result<Measurements, ReadError> {
        let mut result = Measurements::default();
        match step {
            0..=2 => {
                let rel = [REL_PHASE_A, REL_PHASE_B, REL_PHASE_C][step];
                let regs = self.read_component(rel, 2 * PHASE_FLOAT_COUNT).await?;
                let f = decode_floats(&regs);
                result.phases[step] = Some(PhaseMeasurements {
                    voltage: f[0],
                    current: f[1],
                    active_power: f[2],
                    apparent_power: f[3],
                    power_factor: f[4],
                });
            }
            3 => {
                let regs = self
                    .read_component(REL_PHASE_A + REL_PHASE_FREQUENCY, 2)
                    .await?;
                result.frequency = Some(merge_u16_f32(regs[0], regs[1]));
            }
            _ => {
                let regs = self.read_component(REL_ENERGY, 4).await?;
                let f = decode_floats(&regs);
                result.energy = Some((f[0], f[1]));
            }
        }
        Ok(result)
    }

    /// Reads from the EM component block, working out where the Shelly serves it on the first success
    async fn read_component(&mut self, rel: u16, count: u16) -> Result<Vec<u16>, ReadError> {
        let other = if self.base == BASE_DOCUMENTED {
            BASE_ALTERNATIVE
        } else {
            BASE_DOCUMENTED
        };
        let candidates: &[u16] = if self.base_confirmed {
            &[self.base]
        } else {
            &[self.base, other]
        };
        let mut last_error = None;
        for &base in candidates {
            match self.read_registers(base + rel, count).await {
                Ok(regs) => {
                    if !self.base_confirmed || self.base != base {
                        println!("Shelly serves component registers at {base}+");
                    }
                    self.base = base;
                    self.base_confirmed = true;
                    return Ok(regs);
                }
                Err(ReadError::Exception(e)) if !self.base_confirmed => {
                    println!(
                        "Shelly refused read of {}/{count} ({e:?}), trying other register range",
                        base + rel
                    );
                    last_error = Some(ReadError::Exception(e));
                }
                Err(e) => return Err(e),
            }
        }
        Err(last_error.unwrap_or(ReadError::Timeout))
    }

    /// One bounded Modbus read. Any failure that leaves the connection in doubt (timeout, IO error) drops it so
    /// that the next read starts from a clean connection instead of picking up a late reply to this one.
    async fn read_registers(&mut self, addr: u16, count: u16) -> Result<Vec<u16>, ReadError> {
        let started = Instant::now();
        let result = self.read_registers_inner(addr, count).await;
        match &result {
            Ok(regs) => debug_log!(
                "Shelly modbus read {addr}/{count} ok in {}ms: {regs:?}",
                started.elapsed().as_millis()
            ),
            Err(e) => warn_log!(
                "Shelly modbus read {addr}/{count} failed after {}ms: {e}",
                started.elapsed().as_millis()
            ),
        }
        if matches!(result, Err(ReadError::Timeout | ReadError::Io(_))) {
            self.connection = None;
        }
        result
    }

    async fn read_registers_inner(&mut self, addr: u16, count: u16) -> Result<Vec<u16>, ReadError> {
        if self.connection.is_none() {
            debug_log!("Connecting to Shelly modbus {}", self.target);
            let connection = timeout(IO_TIMEOUT, tcp::connect(self.target))
                .await
                .map_err(|_| ReadError::Timeout)?
                .map_err(|e| ReadError::Io(format!("connect failed: {e}")))?;
            println!("Connected to Shelly modbus {}", self.target);
            self.connection = Some(connection);
        }
        let connection = self.connection.as_mut().expect("connected above");
        let reply = timeout(IO_TIMEOUT, connection.read_input_registers(addr, count))
            .await
            .map_err(|_| ReadError::Timeout)?
            .map_err(|e| ReadError::Io(e.to_string()))?;
        let regs = reply.map_err(ReadError::Exception)?;
        if regs.len() != count as usize {
            return Err(ReadError::Io(format!(
                "asked for {count} registers, got {}",
                regs.len()
            )));
        }
        Ok(regs)
    }
}

/// The Shelly sends floats low word first
fn merge_u16_f32(low: u16, high: u16) -> f32 {
    f32::from_bits(low as u32 | (high as u32) << 16)
}

fn decode_floats(regs: &[u16]) -> Vec<f32> {
    regs.chunks_exact(2)
        .map(|chunk| merge_u16_f32(chunk[0], chunk[1]))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn float_word_order() {
        let bits = 230.5f32.to_bits();
        assert_eq!(
            merge_u16_f32((bits & 0xFFFF) as u16, (bits >> 16) as u16),
            230.5
        );
    }

    #[test]
    fn decodes_pairs() {
        let a = 1.5f32.to_bits();
        let b = (-2.0f32).to_bits();
        let regs = [
            (a & 0xFFFF) as u16,
            (a >> 16) as u16,
            (b & 0xFFFF) as u16,
            (b >> 16) as u16,
        ];
        assert_eq!(decode_floats(&regs), vec![1.5, -2.0]);
    }
}
