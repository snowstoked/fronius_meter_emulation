//! Source independent description of what a Shelly 3EM reported, and its translation into the registers of the
//! emulated Fronius meter.

use crate::smart_meter_emulator::Readings;

#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct PhaseMeasurements {
    pub voltage: f32,
    pub current: f32,
    pub active_power: f32,
    pub apparent_power: f32,
    pub power_factor: f32,
}

impl PhaseMeasurements {
    /// The Shelly does not report reactive power, derive it from the power triangle
    pub fn reactive_power(&self) -> f32 {
        reactive_from(self.active_power, self.apparent_power)
    }
}

/// Average over the phases that are actually connected, so a single phase device doesn't report a third of the voltage
fn average_voltage(phases: &[PhaseMeasurements; 3]) -> f32 {
    let live: Vec<f32> = phases
        .iter()
        .map(|p| p.voltage)
        .filter(|v| *v > 1.0)
        .collect();
    if live.is_empty() {
        0.0
    } else {
        live.iter().sum::<f32>() / live.len() as f32
    }
}

/// `sqrt(S² - P²)`, never negative and never NaN
pub fn reactive_from(active: f32, apparent: f32) -> f32 {
    let q2 = apparent * apparent - active * active;
    if q2 > 0.0 { q2.sqrt() } else { 0.0 }
}

/// Everything we know about the Shelly. Every field is optional because the different read paths each provide a
/// part of it; `merge` combines them.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Measurements {
    pub total_active_power: Option<f32>,
    pub phases: [Option<PhaseMeasurements>; 3],
    pub frequency: Option<f32>,
    /// (import Wh, export Wh)
    pub energy: Option<(f32, f32)>,
}

impl Measurements {
    pub fn merge(&mut self, other: Measurements) {
        if other.total_active_power.is_some() {
            self.total_active_power = other.total_active_power;
        }
        for (mine, theirs) in self.phases.iter_mut().zip(other.phases) {
            if theirs.is_some() {
                *mine = theirs;
            }
        }
        if other.frequency.is_some() {
            self.frequency = other.frequency;
        }
        if other.energy.is_some() {
            self.energy = other.energy;
        }
    }

    /// Flip the direction of power flow, for a meter that is wired (or a device that counts) the other way round
    pub fn invert(&mut self) {
        fn neg(value: f32) -> f32 {
            if value == 0.0 { 0.0 } else { -value }
        }
        self.total_active_power = self.total_active_power.map(neg);
        for phase in self.phases.iter_mut().flatten() {
            phase.active_power = neg(phase.active_power);
            phase.power_factor = neg(phase.power_factor);
        }
        self.energy = self.energy.map(|(import, export)| (export, import));
    }

    pub fn all_phases(&self) -> Option<[PhaseMeasurements; 3]> {
        Some([self.phases[0]?, self.phases[1]?, self.phases[2]?])
    }

    /// Translate into register updates.
    /// `net_power` is what is reported as total power (Shelly plus any external offset); everything else is
    /// straight from the Shelly. Sent as one batch so the inverter never sees a half updated set.
    pub fn to_readings(&self, net_power: f32) -> Vec<Readings> {
        let mut out = vec![Readings::TotalRealPower(net_power)];
        if let Some(phases) = self.all_phases() {
            let sum_current: f32 = phases.iter().map(|p| p.current).sum();
            let sum_active: f32 = phases.iter().map(|p| p.active_power).sum();
            let sum_apparent: f32 = phases.iter().map(|p| p.apparent_power).sum();
            let [a, b, c] = phases;
            out.extend([
                Readings::PhaseAVoltage(a.voltage),
                Readings::PhaseBVoltage(b.voltage),
                Readings::PhaseCVoltage(c.voltage),
                Readings::AveragePhaseVoltage(average_voltage(&phases)),
                Readings::PhaseACurrent(a.current),
                Readings::PhaseBCurrent(b.current),
                Readings::PhaseCCurrent(c.current),
                Readings::NetACCurrent(sum_current),
                Readings::PhaseAWatts(a.active_power),
                Readings::PhaseBWatts(b.active_power),
                Readings::PhaseCWatts(c.active_power),
                Readings::PhaseAVA(a.apparent_power),
                Readings::PhaseBVA(b.apparent_power),
                Readings::PhaseCVA(c.apparent_power),
                Readings::ApparentPower(sum_apparent),
                Readings::PhaseAVAR(a.reactive_power()),
                Readings::PhaseBVAR(b.reactive_power()),
                Readings::PhaseCVAR(c.reactive_power()),
                Readings::ReactivePower(reactive_from(sum_active, sum_apparent)),
                Readings::PhaseAPF(a.power_factor),
                Readings::PhaseBPF(b.power_factor),
                Readings::PhaseCPF(c.power_factor),
                Readings::PowerFactorTotal(if sum_apparent > 1.0 {
                    (sum_active / sum_apparent).clamp(-1.0, 1.0)
                } else {
                    0.0
                }),
            ]);
        }
        if let Some(freq) = self.frequency {
            out.push(Readings::Frequency(freq));
        }
        if let Some((import, export)) = self.energy {
            out.push(Readings::TotalWhImport(import));
            out.push(Readings::TotalWhExport(export));
        }
        out
    }

    /// One line description for debug logs
    pub fn summary(&self) -> String {
        let phase = |p: &Option<PhaseMeasurements>| match p {
            Some(p) => format!(
                "{:.0}W/{:.1}V/{:.2}A/{:.0}VA/pf{:.2}",
                p.active_power, p.voltage, p.current, p.apparent_power, p.power_factor
            ),
            None => "-".to_string(),
        };
        format!(
            "total {:?}W | A {} | B {} | C {} | {:?}Hz | Wh {:?}",
            self.total_active_power,
            phase(&self.phases[0]),
            phase(&self.phases[1]),
            phase(&self.phases[2]),
            self.frequency,
            self.energy
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn phase(w: f32, va: f32) -> PhaseMeasurements {
        PhaseMeasurements {
            voltage: 230.0,
            current: va / 230.0,
            active_power: w,
            apparent_power: va,
            power_factor: w / va,
        }
    }

    #[test]
    fn reactive_power_triangle() {
        assert!((reactive_from(300.0, 500.0) - 400.0).abs() < 0.01);
        assert_eq!(reactive_from(500.0, 500.0), 0.0);
        // Rounding noise must not give NaN
        assert_eq!(reactive_from(500.1, 500.0), 0.0);
        assert_eq!(reactive_from(-300.0, 500.0), 400.0);
    }

    #[test]
    fn only_total_without_phases() {
        let m = Measurements {
            total_active_power: Some(100.0),
            ..Default::default()
        };
        let readings = m.to_readings(100.0);
        assert_eq!(readings.len(), 1);
        assert!(matches!(readings[0], Readings::TotalRealPower(p) if p == 100.0));
    }

    #[test]
    fn partial_phases_are_not_published() {
        let mut m = Measurements::default();
        m.phases[0] = Some(phase(100.0, 120.0));
        assert!(m.all_phases().is_none());
        assert_eq!(m.to_readings(1.0).len(), 1);
    }

    #[test]
    fn net_power_is_not_overwritten_by_current() {
        let mut m = Measurements::default();
        m.phases = [
            Some(phase(300.0, 500.0)),
            Some(phase(-100.0, 200.0)),
            Some(phase(50.0, 100.0)),
        ];
        let readings = m.to_readings(250.0);
        let currents: Vec<f32> = readings
            .iter()
            .filter_map(|r| match r {
                Readings::NetACCurrent(a) => Some(*a),
                _ => None,
            })
            .collect();
        // Exactly one total current and it is amps, not watts
        assert_eq!(currents.len(), 1);
        assert!((currents[0] - (500.0 + 200.0 + 100.0) / 230.0).abs() < 0.01);
    }

    #[test]
    fn single_phase_device_reports_its_real_voltage() {
        let mut m = Measurements::default();
        m.phases = [
            Some(phase(1000.0, 1100.0)),
            Some(PhaseMeasurements::default()),
            Some(PhaseMeasurements::default()),
        ];
        let average = m
            .to_readings(1000.0)
            .iter()
            .find_map(|r| match r {
                Readings::AveragePhaseVoltage(v) => Some(*v),
                _ => None,
            })
            .unwrap();
        assert_eq!(average, 230.0);
    }

    #[test]
    fn invert_flips_power_and_swaps_energy() {
        let mut m = Measurements {
            total_active_power: Some(500.0),
            energy: Some((100.0, 20.0)),
            ..Default::default()
        };
        m.phases = [Some(phase(300.0, 500.0)), Some(phase(0.0, 0.0)), None];
        m.invert();
        assert_eq!(m.total_active_power, Some(-500.0));
        assert_eq!(m.energy, Some((20.0, 100.0)));
        assert_eq!(m.phases[0].unwrap().active_power, -300.0);
        assert!(m.phases[0].unwrap().power_factor < 0.0);
        // Current and voltage are magnitudes, and zero stays a plain zero (no negative zero in the registers)
        assert_eq!(m.phases[0].unwrap().voltage, 230.0);
        assert!(m.phases[1].unwrap().active_power.is_sign_positive());
        m.invert();
        assert_eq!(m.total_active_power, Some(500.0));
        assert_eq!(m.energy, Some((100.0, 20.0)));
    }

    #[test]
    fn merge_keeps_old_values_that_are_missing() {
        let mut state = Measurements {
            frequency: Some(50.0),
            ..Default::default()
        };
        state.merge(Measurements {
            total_active_power: Some(5.0),
            ..Default::default()
        });
        assert_eq!(state.frequency, Some(50.0));
        assert_eq!(state.total_active_power, Some(5.0));
    }
}
