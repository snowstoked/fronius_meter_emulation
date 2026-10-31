use crate::debug::{debug_log, warn_log};
use std::{
    collections::HashMap,
    future,
    pin::Pin,
    process,
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::{
    sync::mpsc::{self, Receiver, Sender},
    time::timeout,
};
use tokio_modbus::prelude::*;

/// Register file of the emulated meter plus a little bookkeeping used for diagnostics
struct MeterState {
    registers: HashMap<u16, u16>,
    /// Announce the Modbus address (unit id) of each request as serial number, unless `FRONIUS_SERIAL` is set
    auto_serial: bool,
    last_power_update: Option<Instant>,
    last_inverter_read: Option<Instant>,
}

#[derive(Clone)]
pub struct SmartMeterEmulator {
    state: Arc<tokio::sync::Mutex<MeterState>>,
}
// Turns out you only need to implement total power here, but they are all supported for future hacks
#[allow(dead_code)]
#[derive(Debug)]
pub enum Readings {
    NetACCurrent(f32),
    AveragePhaseVoltage(f32),
    AverageLLVoltage(f32),
    PhaseACurrent(f32),
    PhaseBCurrent(f32),
    PhaseCCurrent(f32),
    PhaseAVoltage(f32),
    PhaseBVoltage(f32),
    PhaseCVoltage(f32),
    PhaseAWatts(f32),
    PhaseBWatts(f32),
    PhaseCWatts(f32),
    PhaseABVoltage(f32),
    PhaseBCVoltage(f32),
    PhaseCAVoltage(f32),
    Frequency(f32),
    TotalRealPower(f32),
    ApparentPower(f32),
    PhaseAVA(f32),
    PhaseBVA(f32),
    PhaseCVA(f32),
    ReactivePower(f32),
    PhaseAVAR(f32),
    PhaseBVAR(f32),
    PhaseCVAR(f32),
    PowerFactorTotal(f32),
    PhaseAPF(f32),
    PhaseBPF(f32),
    PhaseCPF(f32),
    TotalWhExport(f32),
    TotalWhImport(f32),
    /// Applied while holding the register lock, so the inverter can never read half of the update
    Batch(Vec<Readings>),
}

// SunSpec model 213 register map used by the Fronius smart meter (float variant)
const REG_AC_CURRENT_TOTAL: u16 = 40071;
const REG_AC_CURRENT_A: u16 = 40073;
const REG_AC_CURRENT_B: u16 = 40075;
const REG_AC_CURRENT_C: u16 = 40077;
const REG_VOLTAGE_LN_AVG: u16 = 40079;
const REG_VOLTAGE_A: u16 = 40081;
const REG_VOLTAGE_B: u16 = 40083;
const REG_VOLTAGE_C: u16 = 40085;
const REG_VOLTAGE_LL_AVG: u16 = 40087;
const REG_VOLTAGE_AB: u16 = 40089;
const REG_VOLTAGE_BC: u16 = 40091;
const REG_VOLTAGE_CA: u16 = 40093;
const REG_FREQUENCY: u16 = 40095;
const REG_POWER_TOTAL: u16 = 40097;
const REG_POWER_A: u16 = 40099;
const REG_POWER_B: u16 = 40101;
const REG_POWER_C: u16 = 40103;
const REG_VA_TOTAL: u16 = 40105;
const REG_VA_A: u16 = 40107;
const REG_VA_B: u16 = 40109;
const REG_VA_C: u16 = 40111;
const REG_VAR_TOTAL: u16 = 40113;
const REG_VAR_A: u16 = 40115;
const REG_VAR_B: u16 = 40117;
const REG_VAR_C: u16 = 40119;
const REG_PF_TOTAL: u16 = 40121;
const REG_PF_A: u16 = 40123;
const REG_PF_B: u16 = 40125;
const REG_PF_C: u16 = 40127;
const REG_WH_EXPORT: u16 = 40129;
const REG_WH_IMPORT: u16 = 40137;

// The identity block of the SunSpec common model, one character per register
const REG_SERIAL_FIRST: u16 = 40052;
const REG_SERIAL_LAST: u16 = 40067;
const REG_DEVICE_ADDRESS: u16 = 40068;
const DEFAULT_SERIAL: &str = "00000001";
const DEFAULT_DEVICE_ADDRESS: u16 = 240;

/// Sets the serial number and the Modbus device address that the emulated meter announces
fn apply_identity(registers: &mut HashMap<u16, u16>, serial: &str, device_address: u16) {
    for reg in REG_SERIAL_FIRST..=REG_SERIAL_LAST {
        registers.insert(reg, 0);
    }
    for (offset, byte) in serial.bytes().take(16).enumerate() {
        registers.insert(REG_SERIAL_FIRST + offset as u16, byte as u16);
    }
    registers.insert(REG_DEVICE_ADDRESS, device_address);
}

/// The serial number announced to a given Modbus address (unit id): the address with leading zeros.
/// Addresses outside the valid range (0 = broadcast, 248-255 = reserved, 255 is also the usual "any" id of TCP
/// clients) don't identify a meter, they get the register file's own serial.
fn serial_for_unit(unit: u8) -> Option<String> {
    (1..=247).contains(&unit).then(|| format!("{unit:08}"))
}

/// Replaces the serial number characters inside a block of registers that was read starting at `first_register`.
/// Returns whether the block contained any of them.
fn overlay_serial(values: &mut [u16], first_register: u16, serial: &str) -> bool {
    let bytes = serial.as_bytes();
    let mut touched = false;
    for (offset, value) in values.iter_mut().enumerate() {
        let register = first_register as usize + offset;
        if (REG_SERIAL_FIRST as usize..=REG_SERIAL_LAST as usize).contains(&register) {
            let index = register - REG_SERIAL_FIRST as usize;
            *value = bytes.get(index).map(|b| *b as u16).unwrap_or(0);
            touched = true;
        }
    }
    touched
}

impl tokio_modbus::server::Service for SmartMeterEmulator {
    // With the unit id, so that every Modbus address can be given its own identity
    type Request = SlaveRequest<'static>;
    type Response = Response;
    type Exception = tokio_modbus::ExceptionCode;
    type Future =
        Pin<Box<dyn future::Future<Output = Result<Self::Response, Self::Exception>> + Send>>;

    fn call(&self, req: Self::Request) -> Self::Future {
        let state = self.state.clone();
        Box::pin(async move {
            let SlaveRequest { slave, request } = req;
            let (holding, addr, cnt) = match request {
                Request::ReadInputRegisters(addr, cnt) => (false, addr, cnt),
                Request::ReadHoldingRegisters(addr, cnt) => (true, addr, cnt),
                other => {
                    println!(
                        "SERVER: Exception::IllegalFunction - Unimplemented function code in request: {other:?} (unit {slave})"
                    );
                    return Err(tokio_modbus::ExceptionCode::IllegalFunction);
                }
            };
            let mut state = state.lock().await;
            log_inverter_read(
                if holding { "holding" } else { "input" },
                slave,
                &mut state,
                addr,
                cnt,
            );
            let mut values = register_read(&state.registers, addr, cnt)?;
            if state.auto_serial
                && let Some(serial) = serial_for_unit(slave)
                && overlay_serial(&mut values, addr, &serial)
            {
                debug_log!("Unit {slave} asked for the identity block, announcing serial {serial}");
            }
            Ok(if holding {
                Response::ReadHoldingRegisters(values)
            } else {
                Response::ReadInputRegisters(values)
            })
        })
    }
}

/// What the inverter asked for, and what it was told (debug only)
fn log_inverter_read(kind: &str, unit: u8, state: &mut MeterState, addr: u16, cnt: u16) {
    let now = Instant::now();
    let since_previous = fmt_age(state.last_inverter_read.map(|t| now.duration_since(t)));
    state.last_inverter_read = Some(now);
    if !crate::debug::enabled() {
        return;
    }
    let regs = &state.registers;
    let covers = |reg: u16| addr <= reg && reg + 1 < addr + cnt;
    if covers(REG_POWER_TOTAL) {
        let power_age = fmt_age(state.last_power_update.map(|t| now.duration_since(t)));
        let f = |reg: u16| {
            if covers(reg) {
                get_f32(regs, reg)
            } else {
                f32::NAN
            }
        };
        debug_log!(
            "Inverter {kind} read {addr}/{cnt} (unit {unit}): net {:.1}W (A {:.1} B {:.1} C {:.1}), {:.2}A, {:.1}V, {:.2}Hz | power data {} old | {} since previous read",
            f(REG_POWER_TOTAL),
            f(REG_POWER_A),
            f(REG_POWER_B),
            f(REG_POWER_C),
            f(REG_AC_CURRENT_TOTAL),
            f(REG_VOLTAGE_LN_AVG),
            f(REG_FREQUENCY),
            power_age,
            since_previous
        );
    } else {
        debug_log!(
            "Inverter {kind} read {addr}/{cnt} (unit {unit}) | {since_previous} since previous read"
        );
    }
}

fn fmt_age(age: Option<Duration>) -> String {
    match age {
        Some(d) => format!("{}ms", d.as_millis()),
        None => "n/a".to_string(),
    }
}

/// First of the given environment variables that is set to something
fn env_nonempty(names: &[&str]) -> Option<String> {
    names
        .iter()
        .filter_map(|name| std::env::var(name).ok())
        .map(|value| value.trim().to_string())
        .find(|value| !value.is_empty())
}

impl SmartMeterEmulator {
    pub fn new() -> (Self, Sender<Readings>) {
        // Insert some test data as register values.
        let mut input_registers = HashMap::new();
        input_registers.insert(0, 1234);
        input_registers.insert(1, 5678);
        let mut holding_registers = HashMap::new();
        // Seed in all the constant values that are used for the device
        // Well-known value. Uniquely identifies this as a SunSpec Modbus Map
        let sun_spec_values: [u16; 101] = [
            0x5375, 0x6e53, // Sun Spec marker
            1, 65, // Num registers
            70, 114, 111, 110, 105, 117, 115, 0, 0, 0, 0, 0, 0, 0, 0, 0, 83, 109, 97, 114, 116, 32,
            77, 101, 116, 101, 114, 32, 54, 51, 65, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
            0, 48, 48, 48, 48, 48, 48, 48, 49, 0, 0, 0, 0, 0, 0, 0, 0,   //Block2
            240, // Modbus address
            213, // Y connected 3 phase (ABCN)
            124, //End of static values
            0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
            0,
        ];
        for (index, item) in sun_spec_values.iter().enumerate() {
            holding_registers.insert(40000 + index as u16, *item);
        }
        // Several emulated meters on one inverter must not look identical to it.
        // The serial number defaults to the Modbus address the inverter uses for this meter (see `serial_for_unit`),
        // FRONIUS_SERIAL fixes it. FRONIUS_MODBUS_ADDRESS sets the device address in the identity block.
        let explicit_serial = env_nonempty(&["FRONIUS_SERIAL"]);
        let device_address = match env_nonempty(&["FRONIUS_MODBUS_ADDRESS", "FRONIUS_DEVICE_ADDRESS"]) {
            Some(value) => value.parse().unwrap_or_else(|_| {
                warn_log!("FRONIUS_MODBUS_ADDRESS `{value}` is not a number, using {DEFAULT_DEVICE_ADDRESS}");
                DEFAULT_DEVICE_ADDRESS
            }),
            None => DEFAULT_DEVICE_ADDRESS,
        };
        match &explicit_serial {
            Some(serial) => println!(
                "Emulated meter serial `{serial}` (FRONIUS_SERIAL), SunSpec device address {device_address}"
            ),
            None => println!(
                "Emulated meter serial: the Modbus address of the request, e.g. `{}`; SunSpec device address {device_address}",
                serial_for_unit(2).unwrap_or_default()
            ),
        }
        let auto_serial = explicit_serial.is_none();
        apply_identity(
            &mut holding_registers,
            explicit_serial.as_deref().unwrap_or(DEFAULT_SERIAL),
            device_address,
        );
        // Second set of filler readings
        let sun_spec_values_2: [u16; 16] = [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];
        for (index, item) in sun_spec_values_2.iter().enumerate() {
            holding_registers.insert(40129 + index as u16, *item);
        }
        // 0 fill the "readings" address sapce
        for index in 40071..40161 {
            holding_registers.insert(index as u16, 0);
        }

        //Misc filler

        holding_registers.insert(40193, 0);
        holding_registers.insert(40194, 0);
        holding_registers.insert(40195, 0xFFFF); // Terminates the readings blocks
        holding_registers.insert(40196, 0);

        holding_registers.insert(0, 1); // Sunspec model common
        holding_registers.insert(1, 0); // Length of registers
        holding_registers.insert(11, 0);
        holding_registers.insert(12, 0);
        // Not SunSpec, so return 0 to mark us as SunSpec
        holding_registers.insert(768, 0);
        holding_registers.insert(1706, 0);
        holding_registers.insert(50000, 0);
        holding_registers.insert(50001, 0);

        // To handle incoming data updates, we use an MPSC channel for comms
        let (tx, rx) = mpsc::channel(128);
        let state = Arc::new(tokio::sync::Mutex::new(MeterState {
            registers: holding_registers,
            auto_serial,
            last_power_update: None,
            last_inverter_read: None,
        }));
        let handler_state = state.clone();
        tokio::spawn(async move {
            Self::handle_incoming_register_events(rx, handler_state).await;
        });

        //Return server & channel for readings
        (Self { state }, tx)
    }

    async fn handle_incoming_register_events(
        mut events: Receiver<Readings>,
        state: Arc<tokio::sync::Mutex<MeterState>>,
    ) {
        println!("Starting readinger updates handler task");

        let data_update_timeout = Duration::from_secs(30);
        while let Ok(Some(reading)) = timeout(data_update_timeout, events.recv()).await {
            let mut state = state.lock().await;
            apply_reading(&mut state, reading);
        }
        warn_log!(
            "No Raw reading updates in 30s, exiting. Last Shelly error: {}",
            crate::debug::last_error_description()
        );
        process::exit(1);
    }
}

fn apply_reading(state: &mut MeterState, reading: Readings) {
    let regs = &mut state.registers;
    match reading {
        Readings::Batch(readings) => {
            for r in readings {
                apply_reading(state, r);
            }
        }
        Readings::NetACCurrent(v) => set_f32(regs, REG_AC_CURRENT_TOTAL, v),
        Readings::AveragePhaseVoltage(v) => set_f32(regs, REG_VOLTAGE_LN_AVG, v),
        Readings::AverageLLVoltage(v) => set_f32(regs, REG_VOLTAGE_LL_AVG, v),
        Readings::PhaseACurrent(v) => set_f32(regs, REG_AC_CURRENT_A, v),
        Readings::PhaseBCurrent(v) => set_f32(regs, REG_AC_CURRENT_B, v),
        Readings::PhaseCCurrent(v) => set_f32(regs, REG_AC_CURRENT_C, v),
        Readings::PhaseAVoltage(v) => set_f32(regs, REG_VOLTAGE_A, v),
        Readings::PhaseBVoltage(v) => set_f32(regs, REG_VOLTAGE_B, v),
        Readings::PhaseCVoltage(v) => set_f32(regs, REG_VOLTAGE_C, v),
        Readings::PhaseAWatts(v) => set_f32(regs, REG_POWER_A, v),
        Readings::PhaseBWatts(v) => set_f32(regs, REG_POWER_B, v),
        Readings::PhaseCWatts(v) => set_f32(regs, REG_POWER_C, v),
        Readings::PhaseABVoltage(v) => set_f32(regs, REG_VOLTAGE_AB, v),
        Readings::PhaseBCVoltage(v) => set_f32(regs, REG_VOLTAGE_BC, v),
        Readings::PhaseCAVoltage(v) => set_f32(regs, REG_VOLTAGE_CA, v),
        Readings::Frequency(v) => set_f32(regs, REG_FREQUENCY, v),
        Readings::TotalRealPower(v) => {
            set_f32(regs, REG_POWER_TOTAL, v);
            state.last_power_update = Some(Instant::now());
        }
        Readings::ApparentPower(v) => set_f32(regs, REG_VA_TOTAL, v),
        Readings::PhaseAVA(v) => set_f32(regs, REG_VA_A, v),
        Readings::PhaseBVA(v) => set_f32(regs, REG_VA_B, v),
        Readings::PhaseCVA(v) => set_f32(regs, REG_VA_C, v),
        Readings::ReactivePower(v) => set_f32(regs, REG_VAR_TOTAL, v),
        Readings::PhaseAVAR(v) => set_f32(regs, REG_VAR_A, v),
        Readings::PhaseBVAR(v) => set_f32(regs, REG_VAR_B, v),
        Readings::PhaseCVAR(v) => set_f32(regs, REG_VAR_C, v),
        Readings::PowerFactorTotal(v) => set_f32(regs, REG_PF_TOTAL, v),
        Readings::PhaseAPF(v) => set_f32(regs, REG_PF_A, v),
        Readings::PhaseBPF(v) => set_f32(regs, REG_PF_B, v),
        Readings::PhaseCPF(v) => set_f32(regs, REG_PF_C, v),
        Readings::TotalWhExport(v) => set_f32(regs, REG_WH_EXPORT, v),
        Readings::TotalWhImport(v) => set_f32(regs, REG_WH_IMPORT, v),
    }
}

/// Only ever overwrites registers that already exist, so a wrong address can't invent new ones
fn set_f32(regs: &mut HashMap<u16, u16>, register: u16, value: f32) {
    let bits = value.to_bits();
    for (reg, word) in [
        (register, (bits >> 16) as u16),
        (register + 1, (bits & 0xFFFF) as u16),
    ] {
        regs.entry(reg).and_modify(|entry| *entry = word);
    }
}

fn get_f32(regs: &HashMap<u16, u16>, register: u16) -> f32 {
    let hi = regs.get(&register).copied().unwrap_or_default() as u32;
    let lo = regs.get(&(register + 1)).copied().unwrap_or_default() as u32;
    f32::from_bits(hi << 16 | lo)
}

/// Helper function implementing reading registers from a HashMap.
fn register_read(
    registers: &HashMap<u16, u16>,
    addr: u16,
    cnt: u16,
) -> Result<Vec<u16>, tokio_modbus::ExceptionCode> {
    let mut response_values = vec![0; cnt.into()];
    for i in 0..cnt {
        let reg_addr = addr + i;
        if let Some(r) = registers.get(&reg_addr) {
            response_values[i as usize] = *r;
        } else {
            println!(
                "SERVER: Exception::IllegalDataAddress, can't handle read of register {reg_addr}/0x{reg_addr:X}"
            );
            return Err(tokio_modbus::ExceptionCode::IllegalDataAddress);
        }
    }
    Ok(response_values)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_state() -> MeterState {
        let mut registers = HashMap::new();
        for index in 40071..40161 {
            registers.insert(index as u16, 0);
        }
        MeterState {
            registers,
            auto_serial: true,
            last_power_update: None,
            last_inverter_read: None,
        }
    }

    #[test]
    fn registers_are_sunspec_float_big_endian() {
        let mut state = test_state();
        apply_reading(&mut state, Readings::TotalRealPower(-1234.5));
        assert_eq!(get_f32(&state.registers, REG_POWER_TOTAL), -1234.5);
        let bits = (-1234.5f32).to_bits();
        assert_eq!(state.registers[&40097], (bits >> 16) as u16);
        assert_eq!(state.registers[&40098], (bits & 0xFFFF) as u16);
        assert!(state.last_power_update.is_some());
    }

    #[test]
    fn phase_c_va_has_the_right_address() {
        let mut state = test_state();
        apply_reading(&mut state, Readings::PhaseCVA(321.0));
        assert_eq!(get_f32(&state.registers, 40111), 321.0);
        // And the neighbours are untouched
        assert_eq!(get_f32(&state.registers, 40109), 0.0);
        assert_eq!(get_f32(&state.registers, 40113), 0.0);
    }

    #[test]
    fn every_reading_lands_on_its_own_register() {
        let mut state = test_state();
        let readings = vec![
            (Readings::NetACCurrent(1.0), 40071),
            (Readings::PhaseACurrent(2.0), 40073),
            (Readings::PhaseBCurrent(3.0), 40075),
            (Readings::PhaseCCurrent(4.0), 40077),
            (Readings::AveragePhaseVoltage(5.0), 40079),
            (Readings::PhaseAVoltage(6.0), 40081),
            (Readings::PhaseBVoltage(7.0), 40083),
            (Readings::PhaseCVoltage(8.0), 40085),
            (Readings::Frequency(9.0), 40095),
            (Readings::TotalRealPower(10.0), 40097),
            (Readings::PhaseAWatts(11.0), 40099),
            (Readings::PhaseBWatts(12.0), 40101),
            (Readings::PhaseCWatts(13.0), 40103),
            (Readings::ApparentPower(14.0), 40105),
            (Readings::PhaseAVA(15.0), 40107),
            (Readings::PhaseBVA(16.0), 40109),
            (Readings::PhaseCVA(17.0), 40111),
            (Readings::ReactivePower(18.0), 40113),
            (Readings::PhaseAVAR(19.0), 40115),
            (Readings::PhaseBVAR(20.0), 40117),
            (Readings::PhaseCVAR(21.0), 40119),
            (Readings::PowerFactorTotal(22.0), 40121),
            (Readings::PhaseAPF(23.0), 40123),
            (Readings::PhaseBPF(24.0), 40125),
            (Readings::PhaseCPF(25.0), 40127),
            (Readings::TotalWhExport(26.0), 40129),
            (Readings::TotalWhImport(27.0), 40137),
        ];
        // All in one batch, like the fetcher does it
        let (batch, expected): (Vec<_>, Vec<_>) = readings.into_iter().unzip();
        apply_reading(&mut state, Readings::Batch(batch));
        for (i, reg) in expected.into_iter().enumerate() {
            let expected_value = (i + 1) as f32;
            assert_eq!(
                get_f32(&state.registers, reg),
                expected_value,
                "register {reg}"
            );
        }
    }

    fn serial_text(registers: &HashMap<u16, u16>) -> String {
        (REG_SERIAL_FIRST..=REG_SERIAL_LAST)
            .map(|r| registers[&r])
            .filter(|c| *c != 0)
            .map(|c| c as u8 as char)
            .collect()
    }

    #[test]
    fn identity_is_configurable_and_keeps_the_old_default() {
        let mut registers: HashMap<u16, u16> = (40000..40100).map(|r| (r, 7)).collect();
        apply_identity(&mut registers, DEFAULT_SERIAL, DEFAULT_DEVICE_ADDRESS);
        assert_eq!(serial_text(&registers), "00000001");
        assert_eq!(registers[&40068], 240);
        // The previous hard coded block, register for register
        let old: [u16; 16] = [48, 48, 48, 48, 48, 48, 48, 49, 0, 0, 0, 0, 0, 0, 0, 0];
        for (i, expected) in old.iter().enumerate() {
            assert_eq!(registers[&(40052 + i as u16)], *expected);
        }

        apply_identity(&mut registers, "BATT-0002", 2);
        assert_eq!(serial_text(&registers), "BATT-0002");
        assert_eq!(registers[&40068], 2);
        // Neighbouring registers are untouched, and over long serials are cut off
        assert_eq!(registers[&40051], 7);
        assert_eq!(registers[&40069], 7);
        apply_identity(&mut registers, "ABCDEFGHIJKLMNOPQRSTUV", 3);
        assert_eq!(serial_text(&registers), "ABCDEFGHIJKLMNOP");
        assert_eq!(registers[&40069], 7);
    }

    #[test]
    fn serial_follows_the_modbus_address() {
        assert_eq!(serial_for_unit(1).as_deref(), Some("00000001"));
        assert_eq!(serial_for_unit(2).as_deref(), Some("00000002"));
        assert_eq!(serial_for_unit(247).as_deref(), Some("00000247"));
        // Broadcast, reserved and the "any unit" id 255 don't name a meter
        for unit in [0, 248, 255] {
            assert_eq!(serial_for_unit(unit), None, "{unit}");
        }
    }

    #[test]
    fn serial_overlay_only_touches_the_serial_registers() {
        // The whole common block as the inverter reads it
        let mut block = vec![7u16; 66];
        assert!(overlay_serial(&mut block, 40004, "00000002"));
        let at = |reg: usize| block[reg - 40004];
        assert_eq!(at(40051), 7);
        let serial: String = (40052..=40067)
            .map(|r| at(r))
            .filter(|c| *c != 0)
            .map(|c| c as u8 as char)
            .collect();
        assert_eq!(serial, "00000002");
        assert_eq!(at(40060), 0, "unused serial registers are zero");
        assert_eq!(at(40068), 7, "device address is not part of the overlay");
        assert_eq!(at(40069), 7);

        // Reads that only partly overlap
        let mut part = vec![7u16; 4];
        assert!(overlay_serial(&mut part, 40050, "00000002"));
        assert_eq!(part, vec![7, 7, '0' as u16, '0' as u16]);
        let mut tail = vec![7u16; 4];
        assert!(overlay_serial(&mut tail, 40066, "00000002"));
        assert_eq!(tail, vec![0, 0, 7, 7]);

        // Reads that don't touch it, including the measurements
        for (addr, count) in [(40071, 58), (40000, 2), (40068, 2), (40193, 2), (40129, 32)] {
            let mut other = vec![9u16; count];
            assert!(
                !overlay_serial(&mut other, addr, "00000002"),
                "{addr}/{count}"
            );
            assert!(other.iter().all(|v| *v == 9));
        }
        // A read up to the end of the address space must not overflow
        let mut edge = vec![1u16; 2];
        assert!(!overlay_serial(&mut edge, 65535, "00000002"));
    }

    async fn serial_seen_by(emulator: &SmartMeterEmulator, unit: u8) -> String {
        use tokio_modbus::server::Service;
        let request = SlaveRequest {
            slave: unit,
            request: Request::ReadHoldingRegisters(40052, 16),
        };
        match emulator.call(request).await.unwrap() {
            Response::ReadHoldingRegisters(values) => values
                .into_iter()
                .filter(|c| *c != 0)
                .map(|c| c as u8 as char)
                .collect(),
            other => panic!("unexpected {other:?}"),
        }
    }

    fn emulator(auto_serial: bool, serial: &str) -> SmartMeterEmulator {
        let mut state = test_state();
        state.auto_serial = auto_serial;
        apply_identity(&mut state.registers, serial, DEFAULT_DEVICE_ADDRESS);
        SmartMeterEmulator {
            state: Arc::new(tokio::sync::Mutex::new(state)),
        }
    }

    #[tokio::test]
    async fn every_unit_id_gets_its_own_serial() {
        let emulator = emulator(true, DEFAULT_SERIAL);
        assert_eq!(serial_seen_by(&emulator, 1).await, "00000001");
        assert_eq!(serial_seen_by(&emulator, 2).await, "00000002");
        assert_eq!(serial_seen_by(&emulator, 17).await, "00000017");
        // No usable unit id: the register file's serial
        assert_eq!(serial_seen_by(&emulator, 255).await, "00000001");
    }

    #[tokio::test]
    async fn explicit_serial_wins_over_the_unit_id() {
        let emulator = emulator(false, "BATT-0002");
        assert_eq!(serial_seen_by(&emulator, 1).await, "BATT-0002");
        assert_eq!(serial_seen_by(&emulator, 2).await, "BATT-0002");
    }

    #[tokio::test]
    async fn measurements_and_errors_are_unchanged() {
        use tokio_modbus::server::Service;
        let emulator = emulator(true, DEFAULT_SERIAL);
        let read = |addr, count| SlaveRequest {
            slave: 2,
            request: Request::ReadInputRegisters(addr, count),
        };
        match emulator.call(read(40071, 58)).await.unwrap() {
            Response::ReadInputRegisters(values) => assert_eq!(values.len(), 58),
            other => panic!("unexpected {other:?}"),
        }
        assert!(emulator.call(read(50, 2)).await.is_err());
        let write = SlaveRequest {
            slave: 2,
            request: Request::WriteSingleRegister(40071, 1),
        };
        assert!(emulator.call(write).await.is_err());
    }

    #[test]
    fn unknown_register_reads_are_refused() {
        let state = test_state();
        assert!(register_read(&state.registers, 40071, 58).is_ok());
        assert!(register_read(&state.registers, 50, 2).is_err());
    }
}
