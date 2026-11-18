//! Reads a Shelly over its HTTP interface instead of Modbus.
//!
//! Gen2+ devices use the RPC interface.
//! The kind of device is detected from the components it reports (`Shelly.GetStatus`):
//! - `em:N` (Pro 3EM): one `EM.GetStatus` call returns the total and all three phases from the same sample.
//!   Energy counters come from `EMData.GetStatus`.
//! - `switch:N` (Pro 1PM, Plus 1PM, Pro 2PM, …): one `Switch.GetStatus` call returns power, voltage, current,
//!   frequency and the energy counters. It is single phase, so it is reported as phase A.
//!
//!
//! Gen1 devices have no `/rpc` (it answers 404), so the emulator falls back to `GET /status`:
//! - `meters` (Shelly 1PM, Plug S, …): one channel, reported as phase A. These devices report neither voltage nor
//!   frequency, so `SHELLY_VOLTAGE` (default 230 V) and `SHELLY_FREQUENCY` (default 50 Hz) are assumed, and the
//!   current is derived from the power.
//! - `emeters` (Shelly EM, 3EM): one entry per phase, missing or invalid ones are zero.
//!
//! Gen2+ authentication must be disabled (digest auth is not implemented). Gen1 supports basic auth through
//! `SHELLY_USER` / `SHELLY_PASSWORD`.

use std::time::{Duration, Instant};

use anyhow::{Context, bail};
use serde_derive::Deserialize;

use crate::{
    debug::debug_log,
    meter_data::{Measurements, PhaseMeasurements},
};

const HTTP_TIMEOUT: Duration = Duration::from_secs(3);
/// Debug logs only show the start of long replies (`Shelly.GetStatus` is several kB)
const DEBUG_BODY_LIMIT: usize = 700;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// Energy meter with its own phases (Pro 3EM)
    Em,
    /// Relay with power metering (Pro 1PM, Plus 1PM, …)
    Switch,
    /// Gen1 single channel meter (Shelly 1PM): `meters[N]` of `/status`
    Gen1Meter,
    /// Gen1 energy meter (Shelly EM, 3EM): `emeters` of `/status`
    Gen1Em,
}

pub struct ShellyRestClient {
    client: reqwest::Client,
    base_url: String,
    /// Explicit component id from `SHELLY_ID`, otherwise the lowest one found
    wanted_id: Option<u32>,
    component: Option<(Kind, u32)>,
    gen1: Gen1Settings,
}

/// What a Gen1 device does not report itself
#[derive(Debug, Clone)]
struct Gen1Settings {
    voltage: f32,
    frequency: f32,
    auth: Option<(String, String)>,
}

impl Gen1Settings {
    fn from_env() -> Self {
        let number = |name: &str, default: f32| {
            std::env::var(name)
                .ok()
                .and_then(|v| v.trim().parse::<f32>().ok())
                .filter(|v| v.is_finite() && *v > 0.0)
                .unwrap_or(default)
        };
        let auth = std::env::var("SHELLY_USER")
            .ok()
            .filter(|u| !u.is_empty())
            .map(|user| (user, std::env::var("SHELLY_PASSWORD").unwrap_or_default()));
        Self {
            voltage: number("SHELLY_VOLTAGE", 230.0),
            frequency: number("SHELLY_FREQUENCY", 50.0),
            auth,
        }
    }
}

#[derive(Debug, Deserialize)]
struct Gen1Status {
    meters: Option<Vec<Gen1Meter>>,
    emeters: Option<Vec<Gen1EMeter>>,
}

#[derive(Debug, Deserialize)]
struct Gen1Meter {
    /// W, positive = the load consumes
    power: Option<f32>,
    is_valid: Option<bool>,
    /// Watt-minutes
    total: Option<f32>,
}

#[derive(Debug, Deserialize)]
struct Gen1EMeter {
    power: Option<f32>,
    reactive: Option<f32>,
    pf: Option<f32>,
    current: Option<f32>,
    voltage: Option<f32>,
    is_valid: Option<bool>,
    /// Wh imported
    total: Option<f32>,
    /// Wh exported
    total_returned: Option<f32>,
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
        Self {
            client,
            base_url,
            wanted_id,
            component: None,
            gen1: Gen1Settings::from_env(),
        }
    }

    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// True if the energy counters have to be read separately from the status
    pub fn needs_energy_read(&self) -> bool {
        // Only the Gen2 energy meter keeps its counters in a separate component. Unknown (not detected yet) counts as yes.
        matches!(self.component, None | Some((Kind::Em, _)))
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
            Kind::Gen1Meter | Kind::Gen1Em => {
                let status: Gen1Status = self.gen1_status().await?;
                if kind == Kind::Gen1Meter {
                    status.meter_measurements(id as usize, &self.gen1)
                } else {
                    status.emeter_measurements(&self.gen1)
                }
            }
        }
    }

    /// Import and export energy counters (Wh), only for devices that don't report them in the status
    pub async fn read_energy(&mut self) -> anyhow::Result<Measurements> {
        let (kind, id) = self.component().await?;
        if kind != Kind::Em {
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
        let (http, body) = self
            .get("/rpc/Shelly.GetStatus", "Shelly.GetStatus")
            .await?;
        let component = if http == reqwest::StatusCode::NOT_FOUND {
            // No RPC interface: a Gen1 device
            let status: serde_json::Value = self.gen1_json().await.context(
                "Cant detect the Shelly type (/rpc and /status both failed: is the address right, and are SHELLY_USER / SHELLY_PASSWORD needed?)",
            )?;
            detect_gen1(&status, self.wanted_id)?
        } else {
            if !http.is_success() {
                bail!(
                    "GET {}/rpc/Shelly.GetStatus returned {http}: {} (is this a Gen2+ device, and is authentication disabled?)",
                    self.base_url,
                    truncate(&body, 300)
                );
            }
            let status: serde_json::Value = serde_json::from_str(&body)
                .context("Unexpected reply to Shelly.GetStatus (is this a Gen2+ device?)")?;
            detect_component(&status, self.wanted_id)?
        };
        println!(
            "Detected Shelly component {}:{}",
            match component.0 {
                Kind::Em => "em",
                Kind::Switch => "switch",
                Kind::Gen1Meter => "gen1 meters",
                Kind::Gen1Em => "gen1 emeters",
            },
            component.1
        );
        self.component = Some(component);
        Ok(component)
    }

    async fn gen1_json<T: serde::de::DeserializeOwned>(&self) -> anyhow::Result<T> {
        let (status, body) = self.get("/status", "status").await?;
        if !status.is_success() {
            bail!(
                "GET {}/status returned {status}: {}",
                self.base_url,
                truncate(&body, 300)
            );
        }
        serde_json::from_str(&body).with_context(|| {
            format!(
                "Unexpected reply from {}/status: {}",
                self.base_url,
                truncate(&body, DEBUG_BODY_LIMIT)
            )
        })
    }

    async fn gen1_status(&self) -> anyhow::Result<Gen1Status> {
        self.gen1_json().await
    }

    async fn rpc<T: serde::de::DeserializeOwned>(
        &self,
        method: &str,
        id: Option<u32>,
    ) -> anyhow::Result<T> {
        let path = match id {
            Some(id) => format!("/rpc/{method}?id={id}"),
            None => format!("/rpc/{method}"),
        };
        let url = format!("{}{path}", self.base_url);
        let (status, body) = self.get(&path, method).await?;
        if !status.is_success() {
            bail!("GET {url} returned {status}: {}", truncate(&body, 300));
        }
        serde_json::from_str(&body).with_context(|| {
            format!(
                "Unexpected reply from {url}: {}",
                truncate(&body, DEBUG_BODY_LIMIT)
            )
        })
    }

    /// One GET, returning the HTTP status and body whatever the status is
    async fn get(&self, path: &str, what: &str) -> anyhow::Result<(reqwest::StatusCode, String)> {
        let url = format!("{}{path}", self.base_url);
        let started = Instant::now();
        let mut request = self.client.get(&url);
        if let Some((user, password)) = &self.gen1.auth {
            request = request.basic_auth(user, Some(password));
        }
        let response = request.send().await.with_context(|| format!("GET {url}"))?;
        let status = response.status();
        let body = response
            .text()
            .await
            .with_context(|| format!("GET {url}"))?;
        debug_log!(
            "Shelly {what} answered {status} in {}ms: {}",
            started.elapsed().as_millis(),
            truncate(&body.replace('\n', "").replace("  ", ""), DEBUG_BODY_LIMIT)
        );
        Ok((status, body))
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

/// Picks the channel out of a Gen1 `/status` reply. `meters` (1PM and similar) wins over `emeters` only if there are
/// no `emeters`, because an EM also lists a `meters` array on some firmware versions.
fn detect_gen1(status: &serde_json::Value, wanted_id: Option<u32>) -> anyhow::Result<(Kind, u32)> {
    let len = |key: &str| {
        status
            .get(key)
            .and_then(|v| v.as_array())
            .map_or(0, Vec::len)
    };
    if len("emeters") > 0 {
        return Ok((Kind::Gen1Em, 0));
    }
    let meters = len("meters");
    let id = wanted_id.unwrap_or(0);
    if meters > 0 && (id as usize) < meters {
        return Ok((Kind::Gen1Meter, id));
    }
    bail!(
        "No supported Gen1 meter found in /status ({meters} meters{}, no emeters)",
        wanted_id
            .map(|id| format!(", SHELLY_ID={id}"))
            .unwrap_or_default()
    )
}

impl Gen1Status {
    /// Shelly 1PM and similar: one channel, reported as phase A
    fn meter_measurements(
        self,
        id: usize,
        settings: &Gen1Settings,
    ) -> anyhow::Result<Measurements> {
        let Some(meter) = self.meters.and_then(|m| m.into_iter().nth(id)) else {
            bail!("/status has no meters[{id}]");
        };
        let Some(power) = meter.power else {
            bail!("/status meters[{id}] has no power");
        };
        if meter.is_valid == Some(false) {
            bail!("The Shelly reports meters[{id}] as not valid");
        }
        let voltage = settings.voltage;
        // Without a reactive power value the load is treated as purely active
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
            frequency: Some(settings.frequency),
            // The counter is in watt-minutes and only counts consumption
            energy: meter.total.map(|wm| (wm / 60.0, 0.0)),
        })
    }

    /// Shelly EM / 3EM: one entry per phase. Missing or invalid ones are zero.
    fn emeter_measurements(self, settings: &Gen1Settings) -> anyhow::Result<Measurements> {
        let emeters = self.emeters.unwrap_or_default();
        if emeters.is_empty() {
            bail!("/status has no emeters");
        }
        let mut phases = [PhaseMeasurements::default(); 3];
        let (mut total, mut import, mut export) = (0.0, 0.0, 0.0);
        for (slot, em) in phases.iter_mut().zip(&emeters) {
            if em.is_valid == Some(false) {
                continue;
            }
            let power = em.power.unwrap_or(0.0);
            let apparent = power.hypot(em.reactive.unwrap_or(0.0));
            *slot = PhaseMeasurements {
                voltage: em.voltage.unwrap_or(0.0),
                current: em.current.unwrap_or(0.0),
                active_power: power,
                apparent_power: apparent,
                power_factor: em.pf.unwrap_or(0.0),
            };
            total += power;
            import += em.total.unwrap_or(0.0);
            export += em.total_returned.unwrap_or(0.0);
        }
        Ok(Measurements {
            total_active_power: Some(total),
            phases: phases.map(Some),
            frequency: Some(settings.frequency),
            energy: Some((import, export)),
        })
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

    #[tokio::test]
    async fn errors_are_reported_not_swallowed() {
        // Neither /rpc nor /status exist, detection fails and is retried next time
        let mut server = mockito::Server::new_async().await;
        server
            .mock("GET", "/rpc/Shelly.GetStatus")
            .with_status(404)
            .with_body("Not found")
            .expect(2)
            .create();
        server
            .mock("GET", "/status")
            .with_status(404)
            .with_body("Not found")
            .expect(2)
            .create();
        let mut client = ShellyRestClient::new(&server.url());
        let err = format!("{:#}", client.read_status().await.unwrap_err());
        assert!(err.contains("404") && err.contains("/status"), "{err}");
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

    const GEN1_1PM: &str = r#"{
        "wifi_sta": {"connected": true}, "relays": [{"ison": true}],
        "meters": [{"power": 123.45, "overpower": 0.0, "is_valid": true, "timestamp": 1700000000,
                    "counters": [1.0, 2.0, 3.0], "total": 6000}],
        "temperature": 40.0
    }"#;

    const GEN1_3EM: &str = r#"{
        "meters": [{"power": 1.0}],
        "emeters": [
            {"power": 300.0, "reactive": 100.0, "pf": 0.95, "current": 1.4, "voltage": 230.5, "is_valid": true, "total": 1000.0, "total_returned": 10.0},
            {"power": -50.0, "reactive": 0.0, "pf": -1.0, "current": 0.2, "voltage": 231.0, "is_valid": true, "total": 2000.0, "total_returned": 20.0},
            {"power": 999.0, "is_valid": false, "total": 5.0}
        ]
    }"#;

    async fn gen1_server(status: &str) -> mockito::ServerGuard {
        let mut server = mockito::Server::new_async().await;
        server
            .mock("GET", "/rpc/Shelly.GetStatus")
            .with_status(404)
            .with_body("Not found")
            .expect(1) // detected once
            .create();
        server
            .mock("GET", "/status")
            .with_status(200)
            .with_body(status)
            .create();
        server
    }

    #[tokio::test]
    async fn reads_a_gen1_1pm_as_phase_a() {
        let server = gen1_server(GEN1_1PM).await;
        let mut client = ShellyRestClient::new(&server.url());
        let m = client.read_status().await.unwrap();
        let _ = client.read_status().await.unwrap();
        assert!(!client.needs_energy_read());

        assert_eq!(m.total_active_power, Some(123.45));
        let phases = m.all_phases().unwrap();
        assert_eq!(phases[0].active_power, 123.45);
        assert_eq!(phases[0].voltage, 230.0);
        assert!((phases[0].current - 123.45 / 230.0).abs() < 1e-4);
        assert_eq!(phases[1], PhaseMeasurements::default());
        assert_eq!(phases[2], PhaseMeasurements::default());
        assert_eq!(m.frequency, Some(50.0));
        // Watt-minutes to Wh
        assert_eq!(m.energy, Some((100.0, 0.0)));
        assert_eq!(client.read_energy().await.unwrap(), Measurements::default());
    }

    #[tokio::test]
    async fn gen1_1pm_invalid_meter_is_an_error() {
        let server = gen1_server(r#"{"meters":[{"power":5.0,"is_valid":false}]}"#).await;
        let err = ShellyRestClient::new(&server.url())
            .read_status()
            .await
            .unwrap_err();
        assert!(err.to_string().contains("not valid"), "{err}");
    }

    #[tokio::test]
    async fn reads_a_gen1_3em() {
        let server = gen1_server(GEN1_3EM).await;
        let m = ShellyRestClient::new(&server.url())
            .read_status()
            .await
            .unwrap();
        assert_eq!(m.total_active_power, Some(250.0));
        let phases = m.all_phases().unwrap();
        assert_eq!(phases[0].voltage, 230.5);
        assert!((phases[0].apparent_power - 300.0_f32.hypot(100.0)).abs() < 1e-3);
        assert_eq!(phases[1].active_power, -50.0);
        // An invalid channel is zero
        assert_eq!(phases[2], PhaseMeasurements::default());
        assert_eq!(m.energy, Some((3000.0, 30.0)));
    }

    #[test]
    fn gen1_detection() {
        let v = |s: &str| serde_json::from_str::<serde_json::Value>(s).unwrap();
        assert_eq!(detect_gen1(&v(GEN1_3EM), None).unwrap(), (Kind::Gen1Em, 0));
        assert_eq!(
            detect_gen1(&v(GEN1_1PM), None).unwrap(),
            (Kind::Gen1Meter, 0)
        );
        assert!(detect_gen1(&v(GEN1_1PM), Some(1)).is_err());
        assert!(detect_gen1(&v(r#"{"relays":[]}"#), None).is_err());
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
