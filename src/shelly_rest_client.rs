//! Reads a Shelly device over HTTP instead of Modbus.
//!
//! Gen2+ devices are read over their RPC interface. The kind of device is detected from the components it reports
//! (`Shelly.GetStatus`):
//! - `em:N` (Pro 3EM): one `EM.GetStatus` call returns the total and all three phases from the same sample.
//!   Energy counters come from `EMData.GetStatus`.
//! - `switch:N` (Pro 1PM, Plus 1PM, Pro 2PM, …): one `Switch.GetStatus` call returns power, voltage, current,
//!   frequency and the energy counters. It is single phase, so it is reported as phase A.
//!
//! Authentication on a Gen2+ Shelly must be disabled (digest auth is not implemented).
//!
//! Gen1 energy meters (Shelly 3EM `SHEM-3`, Shelly EM `SHEM`) have no RPC interface. They are recognised by the 404
//! that `/rpc/Shelly.GetStatus` gives, followed by a `/status` reply with an `emeters` array. That one call returns
//! the total, all phases and the energy counters. The Gen1 meters don't report the grid frequency, so a nominal one
//! (50 Hz, or `SHELLY_FREQUENCY`) is reported. A Gen1 Shelly may use HTTP basic authentication
//! (`SHELLY_USER` / `SHELLY_PASSWORD`).
//!
//! Gen1 single channel devices (Shelly 1PM, Plug S, …) have a `meters` array and no `emeters`. They are reported as
//! phase A, phases B and C are zero. They report neither voltage nor frequency, so `SHELLY_VOLTAGE` (230 V) and
//! `SHELLY_FREQUENCY` (50 Hz) are assumed, and the current is derived from the power.

use std::{
    fmt,
    time::{Duration, Instant},
};

use anyhow::{Context, bail};
use reqwest::StatusCode;
use serde_derive::Deserialize;

use crate::{
    debug::{debug_log, warn_log},
    meter_data::{Measurements, PhaseMeasurements},
};

const HTTP_TIMEOUT: Duration = Duration::from_secs(3);
/// Debug logs only show the start of long replies (`Shelly.GetStatus` is several kB)
const DEBUG_BODY_LIMIT: usize = 700;
/// Reported for devices that don't measure the grid frequency (Gen1 energy meters)
const DEFAULT_FREQUENCY_HZ: f32 = 50.0;
/// Reported for Gen1 single channel devices, which don't measure the voltage
const DEFAULT_VOLTAGE_V: f32 = 230.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// Energy meter with its own phases (Pro 3EM)
    Em,
    /// Relay with power metering (Pro 1PM, Plus 1PM, …)
    Switch,
    /// Gen1 device read from `/status`: energy meter with an `emeters` array (3EM, EM) or single channel device with
    /// a `meters` array (1PM)
    Gen1,
}

/// A reply with an HTTP error status. A type of its own, so that "the RPC interface does not exist" (404) can be told
/// apart from other failures.
#[derive(Debug)]
struct HttpError {
    url: String,
    status: StatusCode,
    body: String,
}

impl fmt::Display for HttpError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "GET {} returned {}: {}",
            self.url,
            self.status,
            truncate(&self.body, 300)
        )
    }
}

impl std::error::Error for HttpError {}

fn is_not_found(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<HttpError>()
        .is_some_and(|e| e.status == StatusCode::NOT_FOUND)
}

pub struct ShellyRestClient {
    client: reqwest::Client,
    base_url: String,
    /// Explicit component id from `SHELLY_ID`, otherwise the lowest one found
    wanted_id: Option<u32>,
    component: Option<(Kind, u32)>,
    /// HTTP basic authentication, only used for Gen1 devices
    gen1_auth: Option<(String, String)>,
    /// Reported for Gen1 devices, which can't measure it
    gen1_frequency: f32,
    /// Reported for Gen1 single channel devices, which can't measure it
    gen1_voltage: f32,
}

#[derive(Debug, Deserialize)]
struct EmStatus {
    total_act_power: Option<f32>,
    a_voltage: Option<f32>,
    a_current: Option<f32>,
    a_act_power: Option<f32>,
    a_aprt_power: Option<f32>,
    a_pf: Option<f32>,
    a_freq: Option<f32>,
    b_voltage: Option<f32>,
    b_current: Option<f32>,
    b_act_power: Option<f32>,
    b_aprt_power: Option<f32>,
    b_pf: Option<f32>,
    b_freq: Option<f32>,
    c_voltage: Option<f32>,
    c_current: Option<f32>,
    c_act_power: Option<f32>,
    c_aprt_power: Option<f32>,
    c_pf: Option<f32>,
    c_freq: Option<f32>,
    errors: Option<Vec<String>>,
}

#[derive(Debug, Deserialize)]
struct EmDataStatus {
    total_act: Option<f32>,
    total_act_ret: Option<f32>,
}

#[derive(Debug, Deserialize)]
struct SwitchStatus {
    /// Active power in W, positive = the load consumes
    apower: Option<f32>,
    voltage: Option<f32>,
    current: Option<f32>,
    freq: Option<f32>,
    pf: Option<f32>,
    aenergy: Option<SwitchEnergy>,
    ret_aenergy: Option<SwitchEnergy>,
    errors: Option<Vec<String>>,
}

#[derive(Debug, Deserialize)]
struct SwitchEnergy {
    /// Wh
    total: Option<f32>,
}

/// The parts of a Gen1 `/status` that are used
#[derive(Debug, Deserialize)]
struct Gen1Status {
    /// Only the 3EM reports a total
    total_power: Option<f32>,
    /// One entry per measuring channel: three for a 3EM (phases A, B, C), two for a Shelly EM
    emeters: Option<Vec<Gen1Emeter>>,
    /// Single channel devices (1PM). Some energy meters list this as well, `emeters` wins then.
    meters: Option<Vec<Gen1Meter>>,
}

#[derive(Debug, Deserialize)]
struct Gen1Meter {
    /// Active power in W, positive = the load consumes
    power: Option<f32>,
    /// False if the channel has no usable measurement
    is_valid: Option<bool>,
    /// Consumed energy in watt-minutes
    total: Option<f32>,
}

#[derive(Debug, Deserialize)]
struct Gen1Emeter {
    /// Active power in W, positive = import
    power: Option<f32>,
    reactive: Option<f32>,
    pf: Option<f32>,
    /// Not reported by the Shelly EM
    current: Option<f32>,
    voltage: Option<f32>,
    /// False if the channel has no usable measurement (no voltage, for example)
    is_valid: Option<bool>,
    /// Imported energy in Wh
    total: Option<f32>,
    /// Exported energy in Wh
    total_returned: Option<f32>,
}

impl ShellyRestClient {
    /// `target` is the Shelly address: `192.168.1.5`, `192.168.1.5:80` or a full `http://…` URL
    pub fn new(target: &str) -> Self {
        let target = target.trim().trim_end_matches('/');
        let base_url = if target.contains("://") {
            target.to_string()
        } else {
            format!("http://{target}")
        };
        let client = reqwest::Client::builder()
            .timeout(HTTP_TIMEOUT)
            .connect_timeout(HTTP_TIMEOUT)
            .build()
            .expect("Cant build http client");
        // SHELLY_EM_ID is the old name of SHELLY_ID
        let wanted_id = ["SHELLY_ID", "SHELLY_EM_ID"]
            .iter()
            .find_map(|name| std::env::var(name).ok())
            .and_then(|v| v.trim().parse().ok());
        // Gen1 devices use basic auth with the user name from the restricted login (default `admin`)
        let gen1_auth = std::env::var("SHELLY_PASSWORD")
            .ok()
            .filter(|password| !password.is_empty())
            .map(|password| {
                let user = std::env::var("SHELLY_USER")
                    .ok()
                    .filter(|user| !user.trim().is_empty())
                    .unwrap_or_else(|| "admin".to_string());
                (user, password)
            });
        let gen1_frequency = parse_frequency(std::env::var("SHELLY_FREQUENCY").ok().as_deref());
        let gen1_voltage = parse_voltage(std::env::var("SHELLY_VOLTAGE").ok().as_deref());
        Self {
            client,
            base_url,
            wanted_id,
            component: None,
            gen1_auth,
            gen1_frequency,
            gen1_voltage,
        }
    }

    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// True if the energy counters have to be read separately from the status
    pub fn needs_energy_read(&self) -> bool {
        !matches!(
            self.component.map(|(kind, _)| kind),
            Some(Kind::Switch | Kind::Gen1)
        )
    }

    /// Total power, per phase values, frequency (and for switches also the energy), all from the same Shelly sample
    pub async fn read_status(&mut self) -> anyhow::Result<Measurements> {
        let (kind, id) = self.component().await?;
        match kind {
            Kind::Em => {
                let status: EmStatus = self.rpc("EM.GetStatus", Some(id)).await?;
                status.into_measurements()
            }
            Kind::Switch => {
                let status: SwitchStatus = self.rpc("Switch.GetStatus", Some(id)).await?;
                status.into_measurements()
            }
            Kind::Gen1 => {
                let status: Gen1Status = self.gen1_status().await?;
                status.into_measurements(self.gen1_frequency, self.gen1_voltage)
            }
        }
    }

    /// Import and export energy counters (Wh), only for devices that don't report them in the status
    pub async fn read_energy(&mut self) -> anyhow::Result<Measurements> {
        let (kind, id) = self.component().await?;
        if matches!(kind, Kind::Switch | Kind::Gen1) {
            return Ok(Measurements::default());
        }
        let data: EmDataStatus = self.rpc("EMData.GetStatus", Some(id)).await?;
        match (data.total_act, data.total_act_ret) {
            (Some(import), Some(export)) => Ok(Measurements {
                energy: Some((import, export)),
                ..Default::default()
            }),
            _ => bail!("EMData.GetStatus reply has no total_act / total_act_ret"),
        }
    }

    /// Works out (once) what kind of device this is. A failure is retried on the next call.
    async fn component(&mut self) -> anyhow::Result<(Kind, u32)> {
        if let Some(component) = self.component {
            return Ok(component);
        }
        let component = match self.rpc("Shelly.GetStatus", None).await {
            Ok(status) => {
                let status: serde_json::Value = status;
                detect_component(&status, self.wanted_id)?
            }
            // No RPC interface at all: this can be a Gen1 device
            Err(rpc_error) if is_not_found(&rpc_error) => {
                let gen1 = self.gen1_status().await.map_err(|gen1_error| {
                    if gen1_error
                        .downcast_ref::<HttpError>()
                        .is_some_and(|e| e.status == StatusCode::UNAUTHORIZED)
                    {
                        gen1_error.context(
                            "Cant detect the Shelly type. It looks like a Gen1 device with authentication, set SHELLY_USER and SHELLY_PASSWORD",
                        )
                    } else {
                        anyhow::anyhow!(
                            "Cant detect the Shelly type: not a Gen2+ device ({rpc_error}), and not a supported Gen1 energy meter ({gen1_error:#})"
                        )
                    }
                })?;
                if gen1.has_emeters() {
                    println!(
                        "Detected Gen1 Shelly energy meter with {} measuring channel(s)",
                        gen1.emeters.as_ref().map_or(0, Vec::len)
                    );
                } else {
                    println!("Detected Gen1 Shelly single channel meter (reported as phase A)");
                }
                (Kind::Gen1, 0)
            }
            Err(error) => {
                return Err(error.context(
                    "Cant detect the Shelly type (is this a Gen2+ device, and is authentication disabled?)",
                ));
            }
        };
        if component.0 != Kind::Gen1 {
            println!(
                "Detected Shelly component {}:{}",
                match component.0 {
                    Kind::Em => "em",
                    _ => "switch",
                },
                component.1
            );
        }
        self.component = Some(component);
        Ok(component)
    }

    async fn rpc<T: serde::de::DeserializeOwned>(
        &self,
        method: &str,
        id: Option<u32>,
    ) -> anyhow::Result<T> {
        let url = match id {
            Some(id) => format!("{}/rpc/{method}?id={id}", self.base_url),
            None => format!("{}/rpc/{method}", self.base_url),
        };
        self.get_json(method, url, false).await
    }

    /// The `/status` of a Gen1 device
    async fn gen1_status(&self) -> anyhow::Result<Gen1Status> {
        let url = format!("{}/status", self.base_url);
        let status: Gen1Status = self.get_json("Gen1 /status", url, true).await?;
        if status.has_emeters() || status.meters.as_deref().is_some_and(|m| !m.is_empty()) {
            Ok(status)
        } else {
            bail!(
                "the /status reply has no emeters or meters (supported Gen1 devices are the Shelly 3EM, EM and 1PM)"
            )
        }
    }

    async fn get_json<T: serde::de::DeserializeOwned>(
        &self,
        label: &str,
        url: String,
        basic_auth: bool,
    ) -> anyhow::Result<T> {
        let started = Instant::now();
        let mut request = self.client.get(&url);
        if let (true, Some((user, password))) = (basic_auth, &self.gen1_auth) {
            request = request.basic_auth(user, Some(password));
        }
        let response = request.send().await.with_context(|| format!("GET {url}"))?;
        let status = response.status();
        let body = response
            .text()
            .await
            .with_context(|| format!("GET {url}"))?;
        debug_log!(
            "Shelly {label} answered {status} in {}ms: {}",
            started.elapsed().as_millis(),
            truncate(&body.replace('\n', "").replace("  ", ""), DEBUG_BODY_LIMIT)
        );
        if !status.is_success() {
            return Err(HttpError { url, status, body }.into());
        }
        serde_json::from_str(&body).with_context(|| {
            format!(
                "Unexpected reply from {url}: {}",
                truncate(&body, DEBUG_BODY_LIMIT)
            )
        })
    }
}

/// `SHELLY_FREQUENCY`: a plausible grid frequency, otherwise the default
fn parse_frequency(value: Option<&str>) -> f32 {
    match value.and_then(|v| v.trim().parse::<f32>().ok()) {
        Some(hz) if (40.0..=70.0).contains(&hz) => hz,
        Some(hz) => {
            warn_log!(
                "SHELLY_FREQUENCY={hz} is not a plausible grid frequency, using {DEFAULT_FREQUENCY_HZ} Hz"
            );
            DEFAULT_FREQUENCY_HZ
        }
        None => DEFAULT_FREQUENCY_HZ,
    }
}

/// `SHELLY_VOLTAGE`: a plausible mains voltage, otherwise the default
fn parse_voltage(value: Option<&str>) -> f32 {
    match value.and_then(|v| v.trim().parse::<f32>().ok()) {
        Some(volts) if (100.0..=300.0).contains(&volts) => volts,
        Some(volts) => {
            warn_log!(
                "SHELLY_VOLTAGE={volts} is not a plausible mains voltage, using {DEFAULT_VOLTAGE_V} V"
            );
            DEFAULT_VOLTAGE_V
        }
        None => DEFAULT_VOLTAGE_V,
    }
}

fn truncate(text: &str, max: usize) -> String {
    match text.char_indices().nth(max) {
        Some((end, _)) => format!("{}…", &text[..end]),
        None => text.to_string(),
    }
}

/// Picks the component to read out of a `Shelly.GetStatus` reply (a JSON object with keys like `em:0`, `switch:1`).
/// Energy meters win over switches; within a kind the wanted id, or else the lowest id.
fn detect_component(
    status: &serde_json::Value,
    wanted_id: Option<u32>,
) -> anyhow::Result<(Kind, u32)> {
    let Some(object) = status.as_object() else {
        bail!("Shelly.GetStatus did not return a JSON object");
    };
    let mut found: Vec<(Kind, u32)> = Vec::new();
    for key in object.keys() {
        let Some((name, id)) = key.split_once(':') else {
            continue;
        };
        let Ok(id) = id.parse::<u32>() else { continue };
        match name {
            "em" => found.push((Kind::Em, id)),
            "switch" => found.push((Kind::Switch, id)),
            _ => {}
        }
    }
    found.sort_by_key(|(kind, id)| (*kind != Kind::Em, *id));
    let choice = match wanted_id {
        Some(wanted) => found.iter().copied().find(|(_, id)| *id == wanted),
        None => found.first().copied(),
    };
    match choice {
        Some(choice) => Ok(choice),
        None => {
            let mut components: Vec<&str> = object
                .keys()
                .filter(|k| k.contains(':'))
                .map(String::as_str)
                .collect();
            components.sort();
            bail!(
                "No supported component (em:N or switch:N{}) found, the Shelly has: {}",
                wanted_id
                    .map(|id| format!(" with id {id}"))
                    .unwrap_or_default(),
                components.join(", ")
            )
        }
    }
}

impl EmStatus {
    fn into_measurements(self) -> anyhow::Result<Measurements> {
        let Some(total) = self.total_act_power else {
            bail!("EM.GetStatus reply has no total_act_power (is SHELLY_ID right?)");
        };
        if let Some(errors) = self.errors.as_ref().filter(|e| !e.is_empty()) {
            crate::debug::warn_log!("Shelly reports errors: {errors:?}");
        }
        let phase = |v: Option<f32>,
                     i: Option<f32>,
                     p: Option<f32>,
                     s: Option<f32>,
                     pf: Option<f32>|
         -> Option<PhaseMeasurements> {
            Some(PhaseMeasurements {
                voltage: v?,
                current: i?,
                active_power: p?,
                apparent_power: s?,
                power_factor: pf.unwrap_or(0.0),
            })
        };
        Ok(Measurements {
            total_active_power: Some(total),
            phases: [
                phase(
                    self.a_voltage,
                    self.a_current,
                    self.a_act_power,
                    self.a_aprt_power,
                    self.a_pf,
                ),
                phase(
                    self.b_voltage,
                    self.b_current,
                    self.b_act_power,
                    self.b_aprt_power,
                    self.b_pf,
                ),
                phase(
                    self.c_voltage,
                    self.c_current,
                    self.c_act_power,
                    self.c_aprt_power,
                    self.c_pf,
                ),
            ],
            frequency: self.a_freq.or(self.b_freq).or(self.c_freq),
            energy: None,
        })
    }
}

impl SwitchStatus {
    /// A single phase device. It is reported as phase A, the other two phases are zero.
    fn into_measurements(self) -> anyhow::Result<Measurements> {
        let Some(power) = self.apower else {
            bail!(
                "Switch.GetStatus reply has no apower (does this device have power metering, and is SHELLY_ID right?)"
            );
        };
        if let Some(errors) = self.errors.as_ref().filter(|e| !e.is_empty()) {
            crate::debug::warn_log!("Shelly reports errors: {errors:?}");
        }
        let phase_a = match (self.voltage, self.current) {
            (Some(voltage), Some(current)) => {
                let apparent = voltage * current;
                Some(PhaseMeasurements {
                    voltage,
                    current,
                    active_power: power,
                    apparent_power: apparent,
                    power_factor: self.pf.unwrap_or(if apparent > 1.0 {
                        (power / apparent).clamp(-1.0, 1.0)
                    } else {
                        0.0
                    }),
                })
            }
            _ => None,
        };
        let phases = match phase_a {
            Some(a) => [
                Some(a),
                Some(PhaseMeasurements::default()),
                Some(PhaseMeasurements::default()),
            ],
            None => [None, None, None],
        };
        let energy = self.aenergy.and_then(|e| e.total).map(|import| {
            let export = self.ret_aenergy.and_then(|e| e.total).unwrap_or(0.0);
            (import, export)
        });
        Ok(Measurements {
            total_active_power: Some(power),
            phases,
            frequency: self.freq,
            energy,
        })
    }
}

impl Gen1Emeter {
    fn phase(&self) -> PhaseMeasurements {
        if self.is_valid == Some(false) {
            return PhaseMeasurements::default();
        }
        let power = self.power.unwrap_or(0.0);
        let voltage = self.voltage.unwrap_or(0.0);
        let apparent = match (self.reactive, self.current) {
            (Some(reactive), _) => power.hypot(reactive),
            (None, Some(current)) if voltage > 1.0 => voltage * current,
            _ => power.abs(),
        };
        let current = self.current.unwrap_or(if voltage > 1.0 {
            apparent / voltage
        } else {
            0.0
        });
        PhaseMeasurements {
            voltage,
            current,
            active_power: power,
            apparent_power: apparent,
            power_factor: self.pf.unwrap_or(if apparent > 1.0 {
                (power / apparent).clamp(-1.0, 1.0)
            } else {
                0.0
            }),
        }
    }
}

impl Gen1Status {
    fn has_emeters(&self) -> bool {
        self.emeters.as_deref().is_some_and(|e| !e.is_empty())
    }

    /// A single channel device (Shelly 1PM): phase A carries the power, phases B and C are zero. There is no voltage
    /// or current in the reply, so the configured voltage is assumed and the current follows from the power.
    fn meter_measurements(self, frequency: f32, voltage: f32) -> anyhow::Result<Measurements> {
        let Some(meter) = self.meters.and_then(|m| m.into_iter().next()) else {
            bail!("The Gen1 /status reply has no meters");
        };
        let Some(power) = meter.power else {
            bail!("The Gen1 /status meters[0] has no power");
        };
        if meter.is_valid == Some(false) {
            bail!("The Shelly reports meters[0] as not valid");
        }
        // No reactive power is reported, the load is treated as purely active
        let apparent = power.abs();
        let phase_a = PhaseMeasurements {
            voltage,
            current: apparent / voltage,
            active_power: power,
            apparent_power: apparent,
            power_factor: if apparent > 1.0 { power.signum() } else { 0.0 },
        };
        Ok(Measurements {
            total_active_power: Some(power),
            phases: [
                Some(phase_a),
                Some(PhaseMeasurements::default()),
                Some(PhaseMeasurements::default()),
            ],
            frequency: Some(frequency),
            // The counter is in watt-minutes and only counts consumption
            energy: meter.total.map(|wm| (wm / 60.0, 0.0)),
        })
    }

    /// The channels become the phases A, B and C. A device with fewer channels (Shelly EM) reports zeros for the
    /// rest, and a channel that is not valid is reported as zeros too. The energy is summed over the channels.
    fn into_measurements(self, frequency: f32, voltage: f32) -> anyhow::Result<Measurements> {
        if !self.has_emeters() {
            return self.meter_measurements(frequency, voltage);
        }
        let Some(emeters) = self.emeters.filter(|e| !e.is_empty()) else {
            bail!("The Gen1 /status reply has no emeters");
        };
        if emeters.len() > 3 {
            warn_log!(
                "The Shelly reports {} emeters, only the first three are used",
                emeters.len()
            );
        }
        let emeters = &emeters[..emeters.len().min(3)];
        let mut phases = [Some(PhaseMeasurements::default()); 3];
        for (slot, emeter) in phases.iter_mut().zip(emeters) {
            *slot = Some(emeter.phase());
        }
        let total = self.total_power.unwrap_or_else(|| {
            emeters
                .iter()
                .filter(|e| e.is_valid != Some(false))
                .filter_map(|e| e.power)
                .sum()
        });
        let imported: Vec<f32> = emeters.iter().filter_map(|e| e.total).collect();
        let energy = if imported.is_empty() {
            None
        } else {
            Some((
                imported.iter().sum(),
                emeters.iter().filter_map(|e| e.total_returned).sum(),
            ))
        };
        Ok(Measurements {
            total_active_power: Some(total),
            phases,
            frequency: Some(frequency),
            energy,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const EM_STATUS: &str = r#"{
        "id": 0,
        "a_current": 1.5, "a_voltage": 231.2, "a_act_power": 300.5, "a_aprt_power": 346.0, "a_pf": 0.87, "a_freq": 50.01,
        "b_current": 0.5, "b_voltage": 230.1, "b_act_power": -100.0, "b_aprt_power": 115.0, "b_pf": -0.9, "b_freq": 50.01,
        "c_current": 2.0, "c_voltage": 229.9, "c_act_power": 400.0, "c_aprt_power": 460.0, "c_pf": 0.86, "c_freq": 50.0,
        "n_current": null,
        "total_current": 4.0, "total_act_power": 600.5, "total_aprt_power": 921.0,
        "user_calibrated_phase": []
    }"#;

    const SWITCH_STATUS: &str = r#"{
        "id": 0, "source": "init", "output": true,
        "apower": 1200.5, "voltage": 231.4, "current": 5.4, "freq": 49.98,
        "aenergy": {"total": 54321.123, "by_minute": [1.0, 2.0, 3.0], "minute_ts": 1700000000},
        "ret_aenergy": {"total": 321.5, "by_minute": [0.0, 0.0, 0.0], "minute_ts": 1700000000},
        "temperature": {"tC": 40.1, "tF": 104.2}
    }"#;

    fn device_status(keys: &[&str]) -> serde_json::Value {
        let mut object = serde_json::Map::new();
        for key in keys {
            object.insert(key.to_string(), serde_json::json!({"id": 0}));
        }
        serde_json::Value::Object(object)
    }

    #[tokio::test]
    async fn reads_everything_from_one_status_call() {
        let mut server = mockito::Server::new_async().await;
        server
            .mock("GET", "/rpc/Shelly.GetStatus")
            .with_status(200)
            .with_body(r#"{"sys":{},"em:0":{"id":0},"emdata:0":{"id":0}}"#)
            .create();
        let mock = server
            .mock("GET", "/rpc/EM.GetStatus?id=0")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(EM_STATUS)
            .expect(1)
            .create();

        let mut client = ShellyRestClient::new(&server.url());
        let m = client.read_status().await.unwrap();
        mock.assert();

        assert_eq!(m.total_active_power, Some(600.5));
        let phases = m.all_phases().unwrap();
        assert_eq!(phases[0].voltage, 231.2);
        assert_eq!(phases[1].active_power, -100.0);
        assert_eq!(phases[2].current, 2.0);
        assert_eq!(phases[0].apparent_power, 346.0);
        assert_eq!(m.frequency, Some(50.01));
        assert_eq!(m.energy, None);
        assert!(client.needs_energy_read());
    }

    #[tokio::test]
    async fn reads_a_single_phase_switch() {
        let mut server = mockito::Server::new_async().await;
        server
            .mock("GET", "/rpc/Shelly.GetStatus")
            .with_status(200)
            .with_body(r#"{"sys":{},"wifi":{},"switch:0":{"id":0},"input:0":{"id":0}}"#)
            .expect(1) // detected once, not on every poll
            .create();
        server
            .mock("GET", "/rpc/Switch.GetStatus?id=0")
            .with_status(200)
            .with_body(SWITCH_STATUS)
            .expect(2)
            .create();

        let mut client = ShellyRestClient::new(&server.url());
        let m = client.read_status().await.unwrap();
        let _ = client.read_status().await.unwrap();
        assert!(!client.needs_energy_read());

        assert_eq!(m.total_active_power, Some(1200.5));
        let phases = m.all_phases().unwrap();
        assert_eq!(phases[0].voltage, 231.4);
        assert_eq!(phases[0].current, 5.4);
        assert_eq!(phases[0].active_power, 1200.5);
        assert!((phases[0].apparent_power - 231.4 * 5.4).abs() < 0.01);
        // pf is derived when the Shelly doesn't report it
        assert!((phases[0].power_factor - 1200.5 / (231.4 * 5.4)).abs() < 0.001);
        assert_eq!(phases[1], PhaseMeasurements::default());
        assert_eq!(phases[2], PhaseMeasurements::default());
        assert_eq!(m.frequency, Some(49.98));
        assert_eq!(m.energy, Some((54321.123, 321.5)));
        // Nothing to read separately
        assert_eq!(client.read_energy().await.unwrap(), Measurements::default());
    }

    #[test]
    fn switch_without_return_energy_or_voltage() {
        let status: SwitchStatus =
            serde_json::from_str(r#"{"id":0,"apower":50.0,"aenergy":{"total":10.0}}"#).unwrap();
        let m = status.into_measurements().unwrap();
        assert_eq!(m.total_active_power, Some(50.0));
        // Without voltage and current there are no phase values, but power and energy still go out
        assert!(m.all_phases().is_none());
        assert_eq!(m.energy, Some((10.0, 0.0)));

        let status: SwitchStatus = serde_json::from_str(r#"{"id":0,"output":false}"#).unwrap();
        assert!(status.into_measurements().is_err());
    }

    #[test]
    fn switch_reported_power_factor_wins() {
        let status: SwitchStatus =
            serde_json::from_str(r#"{"apower":100.0,"voltage":230.0,"current":1.0,"pf":0.5}"#)
                .unwrap();
        let m = status.into_measurements().unwrap();
        assert_eq!(m.phases[0].unwrap().power_factor, 0.5);
    }

    #[test]
    fn detection_prefers_em_then_lowest_id() {
        let kinds = |keys: &[&str], wanted| detect_component(&device_status(keys), wanted).unwrap();
        assert_eq!(kinds(&["sys", "em:0", "emdata:0"], None), (Kind::Em, 0));
        assert_eq!(kinds(&["switch:1", "switch:0"], None), (Kind::Switch, 0));
        assert_eq!(kinds(&["switch:1", "switch:0"], Some(1)), (Kind::Switch, 1));
        assert_eq!(kinds(&["switch:0", "em:0"], None), (Kind::Em, 0));
    }

    #[test]
    fn detection_failure_lists_what_the_shelly_has() {
        let err = detect_component(&device_status(&["sys", "em1:0", "input:0"]), None)
            .unwrap_err()
            .to_string();
        assert!(err.contains("em1:0") && err.contains("input:0"), "{err}");
        let err = detect_component(&device_status(&["switch:0"]), Some(3))
            .unwrap_err()
            .to_string();
        assert!(err.contains("id 3"), "{err}");
    }

    #[tokio::test]
    async fn missing_frequency_and_phases_are_not_fatal() {
        let mut server = mockito::Server::new_async().await;
        server
            .mock("GET", "/rpc/Shelly.GetStatus")
            .with_status(200)
            .with_body(r#"{"em:0":{}}"#)
            .create();
        server
            .mock("GET", "/rpc/EM.GetStatus?id=0")
            .with_status(200)
            .with_body(r#"{"id":0,"total_act_power":42}"#)
            .create();
        let m = ShellyRestClient::new(&server.url())
            .read_status()
            .await
            .unwrap();
        assert_eq!(m.total_active_power, Some(42.0));
        assert!(m.all_phases().is_none());
        assert_eq!(m.frequency, None);
    }

    #[tokio::test]
    async fn reads_energy_counters() {
        let mut server = mockito::Server::new_async().await;
        server
            .mock("GET", "/rpc/Shelly.GetStatus")
            .with_status(200)
            .with_body(r#"{"em:0":{},"emdata:0":{}}"#)
            .create();
        server
            .mock("GET", "/rpc/EMData.GetStatus?id=0")
            .with_status(200)
            .with_body(
                r#"{"id":0,"a_total_act_energy":1.0,"total_act":123456.7,"total_act_ret":8910.1}"#,
            )
            .create();
        let m = ShellyRestClient::new(&server.url())
            .read_energy()
            .await
            .unwrap();
        assert_eq!(m.energy, Some((123456.7, 8910.1)));
    }

    // A Shelly 3EM (SHEM-3), trimmed to the fields that matter
    const GEN1_3EM_STATUS: &str = r#"{
        "wifi_sta": {"connected": true}, "time": "00:50",
        "emeters": [
            {"power": 300.5, "reactive": 120.0, "pf": 0.93, "current": 1.4, "voltage": 231.2, "is_valid": true, "total": 1000.5, "total_returned": 10.5},
            {"power": -100.0, "reactive": 50.0, "pf": -0.89, "current": 0.5, "voltage": 230.1, "is_valid": true, "total": 2000.0, "total_returned": 20.0},
            {"power": 400.0, "reactive": 0.0, "pf": 1.0, "current": 1.75, "voltage": 229.9, "is_valid": true, "total": 3000.0, "total_returned": 30.0}
        ],
        "total_power": 600.5
    }"#;

    #[tokio::test]
    async fn reads_a_gen1_3em_when_there_is_no_rpc_interface() {
        let mut server = mockito::Server::new_async().await;
        let rpc = server
            .mock("GET", "/rpc/Shelly.GetStatus")
            .with_status(404)
            .with_body("Not Found")
            .expect(1) // detected once, not on every poll
            .create();
        // One call to recognise the device, then one per read
        let status = server
            .mock("GET", "/status")
            .with_status(200)
            .with_body(GEN1_3EM_STATUS)
            .expect(3)
            .create();

        let mut client = ShellyRestClient::new(&server.url());
        let m = client.read_status().await.unwrap();
        let _ = client.read_status().await.unwrap();
        rpc.assert();
        status.assert();

        assert_eq!(m.total_active_power, Some(600.5));
        let phases = m.all_phases().unwrap();
        assert_eq!(phases[0].voltage, 231.2);
        assert_eq!(phases[0].current, 1.4);
        assert_eq!(phases[0].active_power, 300.5);
        assert!((phases[0].apparent_power - 300.5f32.hypot(120.0)).abs() < 0.01);
        assert_eq!(phases[0].power_factor, 0.93);
        assert_eq!(phases[1].active_power, -100.0);
        assert_eq!(phases[1].power_factor, -0.89);
        assert_eq!(phases[2].current, 1.75);
        // The Gen1 3EM has no frequency, a nominal one is reported
        assert_eq!(m.frequency, Some(DEFAULT_FREQUENCY_HZ));
        // Energy is in the status: summed over the phases, nothing separate to read
        assert_eq!(m.energy, Some((6000.5, 60.5)));
        assert!(!client.needs_energy_read());
        assert_eq!(client.read_energy().await.unwrap(), Measurements::default());
    }

    const GEN1_1PM_STATUS: &str = r#"{
        "wifi_sta": {"connected": true}, "relays": [{"ison": true}],
        "meters": [{"power": 123.45, "overpower": 0.0, "is_valid": true, "timestamp": 1700000000,
                    "counters": [1.0, 2.0, 3.0], "total": 6000}],
        "temperature": 40.0
    }"#;

    #[tokio::test]
    async fn reads_a_gen1_1pm_as_phase_a() {
        let mut server = mockito::Server::new_async().await;
        server
            .mock("GET", "/rpc/Shelly.GetStatus")
            .with_status(404)
            .with_body("Not Found")
            .expect(1)
            .create();
        server
            .mock("GET", "/status")
            .with_status(200)
            .with_body(GEN1_1PM_STATUS)
            .create();

        let mut client = ShellyRestClient::new(&server.url());
        let m = client.read_status().await.unwrap();
        assert!(!client.needs_energy_read());

        assert_eq!(m.total_active_power, Some(123.45));
        let phases = m.all_phases().unwrap();
        assert_eq!(phases[0].active_power, 123.45);
        assert_eq!(phases[0].voltage, DEFAULT_VOLTAGE_V);
        assert!((phases[0].current - 123.45 / 230.0).abs() < 1e-4);
        assert_eq!(phases[0].power_factor, 1.0);
        assert_eq!(phases[1], PhaseMeasurements::default());
        assert_eq!(phases[2], PhaseMeasurements::default());
        assert_eq!(m.frequency, Some(DEFAULT_FREQUENCY_HZ));
        // Watt-minutes to Wh, consumption only
        assert_eq!(m.energy, Some((100.0, 0.0)));
        assert_eq!(client.read_energy().await.unwrap(), Measurements::default());
    }

    #[test]
    fn gen1_1pm_invalid_or_empty_meter_is_an_error() {
        let status = |json: &str| serde_json::from_str::<Gen1Status>(json).unwrap();
        let err = status(r#"{"meters":[{"power":5.0,"is_valid":false}]}"#)
            .into_measurements(50.0, 230.0)
            .unwrap_err();
        assert!(err.to_string().contains("not valid"), "{err}");
        assert!(
            status(r#"{"meters":[{"is_valid":true}]}"#)
                .into_measurements(50.0, 230.0)
                .is_err()
        );
    }

    #[test]
    fn gen1_emeters_win_over_a_meters_array() {
        let status: Gen1Status = serde_json::from_str(
            r#"{"meters":[{"power":1.0}],"emeters":[{"power":300.0,"voltage":230.0,"is_valid":true}]}"#,
        )
        .unwrap();
        let m = status.into_measurements(50.0, 230.0).unwrap();
        assert_eq!(m.total_active_power, Some(300.0));
    }

    #[test]
    fn voltage_setting_is_checked() {
        assert_eq!(parse_voltage(Some("235.5")), 235.5);
        assert_eq!(parse_voltage(None), DEFAULT_VOLTAGE_V);
        assert_eq!(parse_voltage(Some("abc")), DEFAULT_VOLTAGE_V);
        assert_eq!(parse_voltage(Some("5")), DEFAULT_VOLTAGE_V);
    }

    #[tokio::test]
    async fn gen1_sends_basic_auth_when_configured() {
        let mut server = mockito::Server::new_async().await;
        server
            .mock("GET", "/rpc/Shelly.GetStatus")
            .with_status(404)
            .create();
        // admin:secret
        let authorised = server
            .mock("GET", "/status")
            .match_header("authorization", "Basic YWRtaW46c2VjcmV0")
            .with_status(200)
            .with_body(GEN1_3EM_STATUS)
            .expect(2)
            .create();
        let mut client = ShellyRestClient::new(&server.url());
        client.gen1_auth = Some(("admin".into(), "secret".into()));
        client.read_status().await.unwrap();
        authorised.assert();
    }

    #[tokio::test]
    async fn gen1_with_authentication_says_what_to_set() {
        let mut server = mockito::Server::new_async().await;
        server
            .mock("GET", "/rpc/Shelly.GetStatus")
            .with_status(404)
            .create();
        server
            .mock("GET", "/status")
            .with_status(401)
            .with_body("Unauthorized")
            .create();
        let mut client = ShellyRestClient::new(&server.url());
        client.gen1_auth = None;
        let err = format!("{:#}", client.read_status().await.unwrap_err());
        assert!(
            err.contains("SHELLY_PASSWORD") && err.contains("401"),
            "{err}"
        );
    }

    #[tokio::test]
    async fn a_404_device_without_emeters_or_meters_is_not_taken_for_a_gen1_meter() {
        let mut server = mockito::Server::new_async().await;
        server
            .mock("GET", "/rpc/Shelly.GetStatus")
            .with_status(404)
            .create();
        server
            .mock("GET", "/status")
            .with_status(200)
            .with_body(r#"{"relays":[{"ison":false}],"wifi_sta":{}}"#)
            .create();
        let mut client = ShellyRestClient::new(&server.url());
        let err = format!("{:#}", client.read_status().await.unwrap_err());
        assert!(
            err.contains("no emeters or meters") && err.contains("Gen2"),
            "{err}"
        );
        assert!(client.needs_energy_read(), "nothing was detected");
    }

    #[test]
    fn gen1_shelly_em_with_two_channels_and_no_current() {
        let status: Gen1Status = serde_json::from_str(
            r#"{"emeters": [
                {"power": 230.0, "reactive": 0.0, "pf": 1.0, "voltage": 230.0, "is_valid": true, "total": 10.0, "total_returned": 1.0},
                {"power": 0.0, "reactive": 0.0, "pf": 0.0, "voltage": 230.0, "is_valid": true, "total": 5.0, "total_returned": 0.0}
            ]}"#,
        )
        .unwrap();
        let m = status.into_measurements(50.0, DEFAULT_VOLTAGE_V).unwrap();
        // No total_power in the reply: the sum of the channels
        assert_eq!(m.total_active_power, Some(230.0));
        let phases = m.all_phases().unwrap();
        // Current is derived from the apparent power
        assert!((phases[0].current - 1.0).abs() < 0.001);
        assert_eq!(phases[1].current, 0.0);
        // The missing third phase is zeros
        assert_eq!(phases[2], PhaseMeasurements::default());
        assert_eq!(m.energy, Some((15.0, 1.0)));
    }

    #[test]
    fn gen1_invalid_channel_reports_zeros_and_is_left_out_of_the_derived_total() {
        let status: Gen1Status = serde_json::from_str(
            r#"{"emeters": [
                {"power": 100.0, "voltage": 230.0, "current": 0.5, "is_valid": true},
                {"power": 999.0, "voltage": 230.0, "current": 4.0, "is_valid": false},
                {"power": 50.0, "voltage": 230.0, "current": 0.25, "is_valid": true}
            ]}"#,
        )
        .unwrap();
        let m = status.into_measurements(50.0, DEFAULT_VOLTAGE_V).unwrap();
        assert_eq!(m.total_active_power, Some(150.0));
        let phases = m.all_phases().unwrap();
        assert_eq!(phases[1], PhaseMeasurements::default());
        // Without reactive power the apparent power comes from voltage and current
        assert!((phases[0].apparent_power - 115.0).abs() < 0.01);
        // No counters in the reply: no energy
        assert_eq!(m.energy, None);
    }

    #[test]
    fn frequency_setting() {
        assert_eq!(parse_frequency(None), 50.0);
        assert_eq!(parse_frequency(Some("60")), 60.0);
        assert_eq!(parse_frequency(Some(" 49.95 ")), 49.95);
        // Garbage and implausible values fall back to the default
        assert_eq!(parse_frequency(Some("fifty")), DEFAULT_FREQUENCY_HZ);
        assert_eq!(parse_frequency(Some("0")), DEFAULT_FREQUENCY_HZ);
    }

    #[tokio::test]
    async fn errors_are_reported_not_swallowed() {
        // Detection fails (no RPC interface, and not a Gen1 energy meter either), and is retried next time
        let mut server = mockito::Server::new_async().await;
        server
            .mock("GET", "/rpc/Shelly.GetStatus")
            .with_status(404)
            .with_body("Not found")
            .expect(2)
            .create();
        let mut client = ShellyRestClient::new(&server.url());
        let err = format!("{:#}", client.read_status().await.unwrap_err());
        assert!(err.contains("404") && err.contains("Gen2"), "{err}");
        assert!(client.read_status().await.is_err());

        // A status call that fails
        let mut server = mockito::Server::new_async().await;
        server
            .mock("GET", "/rpc/Shelly.GetStatus")
            .with_status(200)
            .with_body(r#"{"em:0":{}}"#)
            .create();
        server
            .mock("GET", "/rpc/EM.GetStatus?id=0")
            .with_status(500)
            .with_body("boom")
            .create();
        let err = ShellyRestClient::new(&server.url())
            .read_status()
            .await
            .unwrap_err();
        assert!(err.to_string().contains("500"), "{err}");
    }

    #[test]
    fn url_normalisation() {
        assert_eq!(
            ShellyRestClient::new("192.168.1.5").base_url(),
            "http://192.168.1.5"
        );
        assert_eq!(
            ShellyRestClient::new("http://shelly.local/").base_url(),
            "http://shelly.local"
        );
    }

    #[test]
    fn truncation_is_char_safe() {
        assert_eq!(truncate("abc", 5), "abc");
        assert_eq!(truncate("abcdef", 3), "abc…");
        assert_eq!(truncate("äöüäöü", 2), "äö…");
    }
}
