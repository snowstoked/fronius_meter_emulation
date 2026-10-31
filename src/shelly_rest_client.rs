//! Reads a Shelly Gen2+ device over its HTTP RPC interface instead of Modbus.
//!
//! The kind of device is detected from the components it reports (`Shelly.GetStatus`):
//! - `em:N` (Pro 3EM): one `EM.GetStatus` call returns the total and all three phases from the same sample.
//!   Energy counters come from `EMData.GetStatus`.
//! - `switch:N` (Pro 1PM, Plus 1PM, Pro 2PM, …): one `Switch.GetStatus` call returns power, voltage, current,
//!   frequency and the energy counters. It is single phase, so it is reported as phase A.
//!
//! Authentication on the Shelly must be disabled (digest auth is not implemented).

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
}

pub struct ShellyRestClient {
    client: reqwest::Client,
    base_url: String,
    /// Explicit component id from `SHELLY_ID`, otherwise the lowest one found
    wanted_id: Option<u32>,
    component: Option<(Kind, u32)>,
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
        }
    }

    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// True if the energy counters have to be read separately from the status
    pub fn needs_energy_read(&self) -> bool {
        self.component.map(|(kind, _)| kind) != Some(Kind::Switch)
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
        }
    }

    /// Import and export energy counters (Wh), only for devices that don't report them in the status
    pub async fn read_energy(&mut self) -> anyhow::Result<Measurements> {
        let (kind, id) = self.component().await?;
        if kind == Kind::Switch {
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
        let status: serde_json::Value = self.rpc("Shelly.GetStatus", None).await.context(
            "Cant detect the Shelly type (is this a Gen2+ device, and is authentication disabled?)",
        )?;
        let component = detect_component(&status, self.wanted_id)?;
        println!(
            "Detected Shelly component {}:{}",
            match component.0 {
                Kind::Em => "em",
                Kind::Switch => "switch",
            },
            component.1
        );
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
        let started = Instant::now();
        let response = self
            .client
            .get(&url)
            .send()
            .await
            .with_context(|| format!("GET {url}"))?;
        let status = response.status();
        let body = response
            .text()
            .await
            .with_context(|| format!("GET {url}"))?;
        debug_log!(
            "Shelly {method} answered {status} in {}ms: {}",
            started.elapsed().as_millis(),
            truncate(&body.replace('\n', "").replace("  ", ""), DEBUG_BODY_LIMIT)
        );
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
        // Detection fails (old Gen1 style device without the RPC interface), and is retried next time
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
