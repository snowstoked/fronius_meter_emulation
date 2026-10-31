use std::{
    env,
    time::{Duration, Instant},
};

use crate::{
    debug::{debug_log, warn_log},
    home_assistant::HomeAssistantAPI,
    meter_data::Measurements,
    rolling_average::RollingAverage,
    shelly_3em_client::{SLOW_STEPS, Shelly3EMClient},
    shelly_rest_client::ShellyRestClient,
    smart_meter_emulator::Readings,
};
use tokio::{
    sync::mpsc::Sender,
    time::{self, MissedTickBehavior},
};

// Implements reading the Shelly unit and then adjusting power metrics

/// Net power steps bigger than this between two consecutive samples are worth a warning
const LARGE_POWER_STEP_W: f32 = 2000.0;
/// After a failed slow read, try again after this long (or the slow interval if that is shorter)
const SLOW_RETRY: Duration = Duration::from_secs(10);

pub struct DataFetcher {}

impl DataFetcher {
    pub fn new(output: Sender<Readings>) -> Self {
        tokio::spawn(async move {
            Self::worker(output).await;
        });
        Self {}
    }

    async fn worker(output: Sender<Readings>) {
        // 1. Open link to read from Shelly Unit
        // 2. Open link to read from HA
        let home_assistant_extra_import_sensor = env::var("HA_EXTRA_IMPORT").unwrap_or_default();
        let home_assistant_extra_export_sensor = env::var("HA_EXTRA_EXPORT").unwrap_or_default();
        let slow_meter_read_interval: Duration = Duration::from_secs(
            env::var("SLOW_METER_READ_INTERVAL_S")
                .unwrap_or("60".to_owned())
                .parse()
                .unwrap_or(60),
        );
        let poll_interval = Duration::from_millis(
            env::var("SHELLY_POLL_INTERVAL_MS")
                .ok()
                .and_then(|v| v.parse().ok())
                .filter(|ms| *ms >= 100)
                .unwrap_or(500),
        );
        println!("Slow meter read interval: {slow_meter_read_interval:?}");
        println!("Shelly poll interval: {poll_interval:?}");

        let mut source = Source::from_env();
        let invert = crate::debug::env_flag("SHELLY_INVERT");
        if invert {
            println!("SHELLY_INVERT: power direction is flipped");
        }
        let mut ha = HaOffset::new(
            HomeAssistantAPI::new(),
            home_assistant_extra_import_sensor,
            home_assistant_extra_export_sensor,
            parse_bool_safe(env::var("HA_SMOOTH").ok()),
        );

        if !ha.api.is_configured() {
            println!("Home assistant integration disabled")
        }

        println!("Running");
        let mut interval = time::interval(poll_interval);
        // If something stalls we want to carry on from now, not fire a burst of catch-up reads at the Shelly
        interval.set_missed_tick_behavior(MissedTickBehavior::Delay);
        let mut slow = SlowSchedule::new(slow_meter_read_interval);
        let mut state = Measurements::default();
        let mut last_net_power: Option<f32> = None;
        let mut last_cycle = Instant::now();
        let mut cycle: u64 = 0;
        loop {
            interval.tick().await; // Wait for next sample time
            cycle += 1;
            let cycle_started = Instant::now();
            let since_last_cycle = cycle_started.duration_since(last_cycle);
            last_cycle = cycle_started;
            if since_last_cycle > poll_interval * 3 + Duration::from_millis(250) {
                warn_log!(
                    "Cycle {cycle} started {}ms after the previous one (expected {}ms), power data was not refreshed in between",
                    since_last_cycle.as_millis(),
                    poll_interval.as_millis()
                );
            }

            // Net power is the one thing that is needed quickly
            let mut fast = match source.read_fast().await {
                Ok(m) => m,
                Err(e) => {
                    // Don't re-send the old value, it would hide the problem from the watchdog in the emulator
                    warn_log!("Cycle {cycle}: Shelly read failed: {e}");
                    crate::debug::record_error(format!("power read failed: {e}"));
                    continue;
                }
            };
            if invert {
                fast.invert();
            }
            let fast_ms = cycle_started.elapsed().as_millis();
            if fast_ms > 1000 {
                warn_log!("Cycle {cycle}: Shelly power read took {fast_ms}ms");
            }
            state.merge(fast);

            // Slow values (phases, frequency, energy) one small step at a time, so they never hold up the power
            let mut slow_note = String::new();
            if let Some(step) = slow.due_step(cycle_started, source.slow_steps()) {
                let step_started = Instant::now();
                match source.read_slow_step(step).await {
                    Ok(mut m) => {
                        if invert {
                            m.invert();
                        }
                        state.merge(m);
                        slow.step_done(true, source.slow_steps());
                    }
                    Err(e) => {
                        warn_log!("Cycle {cycle}: Shelly slow read step {step} failed: {e}");
                        crate::debug::record_error(format!("slow read step {step} failed: {e}"));
                        slow.step_done(false, source.slow_steps());
                    }
                }
                slow_note = format!(
                    " + slow step {step} {}ms",
                    step_started.elapsed().as_millis()
                );
            }

            let shelly_net_power = state
                .total_active_power
                .expect("fast read always provides power");
            let HaReading {
                import: ha_import,
                export: ha_export,
                offset: ha_offset,
            } = ha.read().await;
            let summed_power = shelly_net_power + ha_offset;

            if let Some(previous) = last_net_power
                && (summed_power - previous).abs() > LARGE_POWER_STEP_W
            {
                warn_log!(
                    "Cycle {cycle}: net power jumped from {previous:.0}W to {summed_power:.0}W in one sample (Shelly {shelly_net_power:.0}W)"
                );
            }
            last_net_power = Some(summed_power);

            debug_log!(
                "Cycle {cycle}: read {fast_ms}ms{slow_note} | net {summed_power:.1}W (shelly {shelly_net_power:.1}W, HA import {ha_import}W export {ha_export}W) | {}",
                state.summary()
            );

            // Everything goes out as one batch so the inverter never reads a mix of old and new values
            output
                .send(Readings::Batch(state.to_readings(summed_power)))
                .await
                .expect("Cant send readings to fake meter");
        }
    }
}

/// What Home Assistant contributed in one cycle
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct HaReading {
    pub import: f32,
    pub export: f32,
    /// `import - export` (smoothed if enabled); this is added to the Shelly's total power
    pub offset: f32,
}

/// Extra consumption/production that the Shelly doesn't see, taken from two Home Assistant sensors.
/// A sensor that can't be read (or isn't a number) counts as 0 W.
pub struct HaOffset {
    api: HomeAssistantAPI,
    import_sensor: String,
    export_sensor: String,
    smooth: bool,
    filter: RollingAverage,
}

impl HaOffset {
    pub fn new(
        api: HomeAssistantAPI,
        import_sensor: String,
        export_sensor: String,
        smooth: bool,
    ) -> Self {
        Self {
            api,
            import_sensor,
            export_sensor,
            smooth,
            filter: RollingAverage::default(),
        }
    }

    pub async fn read(&mut self) -> HaReading {
        if !self.api.is_configured() {
            return HaReading {
                import: 0.0,
                export: 0.0,
                offset: 0.0,
            };
        }
        let import = Self::read_sensor_or_null(&self.import_sensor, &mut self.api).await;
        let export = Self::read_sensor_or_null(&self.export_sensor, &mut self.api).await;
        let offset = if self.smooth {
            self.filter.add(import - export)
        } else {
            import - export
        };
        HaReading {
            import,
            export,
            offset,
        }
    }

    async fn read_sensor_or_null(sensor_name: &str, api: &mut HomeAssistantAPI) -> f32 {
        match api.read_sensor_value(sensor_name).await {
            Ok(res) => res.state.parse().unwrap_or_default(),
            Err(e) => {
                println!("Didn't read HA offset {e:?}");
                0.0
            }
        }
    }
}

/// Where the readings come from
enum Source {
    Modbus(Shelly3EMClient),
    Rest(ShellyRestClient),
}

impl Source {
    fn from_env() -> Self {
        let rest = env::var("SHELLY_REST").unwrap_or_default();
        if !rest.trim().is_empty() {
            let client = ShellyRestClient::new(&rest);
            println!("Reading Shelly over REST at `{}`", client.base_url());
            if env::var("SHELLY_MODBUS").is_ok() {
                println!("Both SHELLY_REST and SHELLY_MODBUS are set, using SHELLY_REST");
            }
            return Source::Rest(client);
        }
        let shelly_modbus = env::var("SHELLY_MODBUS").expect(
            "Required to add Shelly connection info (SHELLY_MODBUS=ip:port or SHELLY_REST=ip)",
        );
        println!("Reading Shelly over modbus at `{shelly_modbus}`");
        Source::Modbus(Shelly3EMClient::new(
            shelly_modbus
                .parse()
                .expect("SHELLY_MODBUS must be an ip:port pair, e.g. 192.168.1.5:502"),
        ))
    }

    async fn read_fast(&mut self) -> Result<Measurements, String> {
        match self {
            Source::Modbus(client) => client.read_fast().await.map_err(|e| e.to_string()),
            Source::Rest(client) => client.read_status().await.map_err(|e| format!("{e:#}")),
        }
    }

    /// How many calls make up one complete slow read
    fn slow_steps(&self) -> usize {
        match self {
            Source::Modbus(_) => SLOW_STEPS,
            // Everything but the energy counters already came with the fast read
            Source::Rest(client) if client.needs_energy_read() => 1,
            // A switch reports its energy together with the power
            Source::Rest(_) => 0,
        }
    }

    async fn read_slow_step(&mut self, step: usize) -> Result<Measurements, String> {
        match self {
            Source::Modbus(client) => client.read_slow_step(step).await.map_err(|e| e.to_string()),
            Source::Rest(client) => client.read_energy().await.map_err(|e| format!("{e:#}")),
        }
    }
}

/// Decides when the next piece of slow data is read. The first full read happens straight away at start-up, and a
/// cycle with failures is retried after a short delay instead of waiting for the whole slow interval.
struct SlowSchedule {
    interval: Duration,
    next_cycle: Instant,
    step: Option<usize>,
    failed: bool,
}

impl SlowSchedule {
    fn new(interval: Duration) -> Self {
        Self {
            interval,
            next_cycle: Instant::now(),
            step: None,
            failed: false,
        }
    }

    fn due_step(&mut self, now: Instant, steps: usize) -> Option<usize> {
        if self.step.is_none() && steps > 0 && now >= self.next_cycle {
            self.step = Some(0);
            self.failed = false;
        }
        self.step
    }

    fn step_done(&mut self, ok: bool, steps: usize) {
        self.failed |= !ok;
        let next = self.step.unwrap_or(0) + 1;
        if next >= steps {
            self.step = None;
            let wait = if self.failed {
                self.interval.min(SLOW_RETRY)
            } else {
                self.interval
            };
            self.next_cycle = Instant::now() + wait;
        } else {
            self.step = Some(next);
        }
    }
}

fn parse_bool_safe(val: Option<String>) -> bool {
    val.unwrap_or_default()
        .to_ascii_lowercase()
        .parse()
        .unwrap_or_default()
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_bool_safe() {
        // Test None input
        assert_eq!(parse_bool_safe(None), false);

        // Test empty string
        assert_eq!(parse_bool_safe(Some("".to_string())), false);

        // Test "true" variations
        assert_eq!(parse_bool_safe(Some("true".to_string())), true);
        assert_eq!(parse_bool_safe(Some("True".to_string())), true);
        assert_eq!(parse_bool_safe(Some("TRUE".to_string())), true);
        assert_eq!(parse_bool_safe(Some("TrUe".to_string())), true);

        // Test "false" variations
        assert_eq!(parse_bool_safe(Some("false".to_string())), false);
        assert_eq!(parse_bool_safe(Some("False".to_string())), false);
        assert_eq!(parse_bool_safe(Some("FALSE".to_string())), false);
        assert_eq!(parse_bool_safe(Some("FaLsE".to_string())), false);

        // Test invalid strings (should default to false)
        assert_eq!(parse_bool_safe(Some("yes".to_string())), false);
        assert_eq!(parse_bool_safe(Some("no".to_string())), false);
        assert_eq!(parse_bool_safe(Some("1".to_string())), false);
        assert_eq!(parse_bool_safe(Some("0".to_string())), false);
        assert_eq!(parse_bool_safe(Some("invalid".to_string())), false);
        assert_eq!(parse_bool_safe(Some("random text".to_string())), false);
    }

    #[test]
    fn slow_schedule_runs_all_steps_then_waits() {
        let mut s = SlowSchedule::new(Duration::from_secs(60));
        let now = Instant::now();
        // Straight away at start-up
        assert_eq!(s.due_step(now, 3), Some(0));
        s.step_done(true, 3);
        assert_eq!(s.due_step(now, 3), Some(1));
        s.step_done(true, 3);
        assert_eq!(s.due_step(now, 3), Some(2));
        s.step_done(true, 3);
        // Cycle complete, nothing due for a minute
        assert_eq!(s.due_step(now + Duration::from_secs(5), 3), None);
        assert_eq!(s.due_step(now + Duration::from_secs(61), 3), Some(0));
    }

    #[test]
    fn slow_schedule_retries_failures_sooner() {
        let mut s = SlowSchedule::new(Duration::from_secs(60));
        let now = Instant::now();
        assert_eq!(s.due_step(now, 1), Some(0));
        s.step_done(false, 1);
        assert_eq!(s.due_step(now + Duration::from_secs(5), 1), None);
        assert_eq!(s.due_step(now + Duration::from_secs(11), 1), Some(0));
    }

    // ---- Home Assistant offset ----

    use mockito::{Server, ServerGuard};

    fn sensor_body(entity: &str, state: &str) -> String {
        format!(
            r#"{{"entity_id":"{entity}","state":"{state}","last_changed":"x","last_reported":"x","last_updated":"x"}}"#
        )
    }

    async fn sensor(server: &mut ServerGuard, entity: &str, state: &str) -> mockito::Mock {
        server
            .mock("GET", format!("/api/states/{entity}").as_str())
            .match_header("Authorization", "Bearer tok")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(sensor_body(entity, state))
            .expect_at_least(0)
            .create_async()
            .await
    }

    fn offset_for(server: &ServerGuard, smooth: bool) -> HaOffset {
        HaOffset::new(
            HomeAssistantAPI::with_endpoint(server.url(), "tok".into()),
            "sensor.import".into(),
            "sensor.export".into(),
            smooth,
        )
    }

    #[tokio::test]
    async fn ha_not_configured_gives_zero_and_no_requests() {
        let mut ha = HaOffset::new(
            HomeAssistantAPI::with_endpoint(String::new(), String::new()),
            "sensor.import".into(),
            "sensor.export".into(),
            false,
        );
        let r = ha.read().await;
        assert_eq!(r, HaReading { import: 0.0, export: 0.0, offset: 0.0 });
        // Only one of the two set is still "not configured"
        let mut ha = HaOffset::new(
            HomeAssistantAPI::with_endpoint("http://x".into(), String::new()),
            "a".into(),
            "b".into(),
            true,
        );
        assert_eq!(ha.read().await.offset, 0.0);
    }

    #[tokio::test]
    async fn ha_offset_is_import_minus_export() {
        let mut server = Server::new_async().await;
        let _i = sensor(&mut server, "sensor.import", "300.5").await;
        let _e = sensor(&mut server, "sensor.export", "100").await;
        let r = offset_for(&server, false).read().await;
        assert_eq!(r.import, 300.5);
        assert_eq!(r.export, 100.0);
        assert_eq!(r.offset, 200.5);
    }

    #[tokio::test]
    async fn ha_export_larger_than_import_gives_negative_offset() {
        let mut server = Server::new_async().await;
        let _i = sensor(&mut server, "sensor.import", "0").await;
        let _e = sensor(&mut server, "sensor.export", "1200").await;
        assert_eq!(offset_for(&server, false).read().await.offset, -1200.0);
    }

    #[tokio::test]
    async fn ha_sends_bearer_token_and_requests_both_sensors() {
        let mut server = Server::new_async().await;
        let i = server
            .mock("GET", "/api/states/sensor.import")
            .match_header("Authorization", "Bearer tok")
            .with_body(sensor_body("sensor.import", "1"))
            .expect(1)
            .create_async()
            .await;
        let e = server
            .mock("GET", "/api/states/sensor.export")
            .match_header("Authorization", "Bearer tok")
            .with_body(sensor_body("sensor.export", "1"))
            .expect(1)
            .create_async()
            .await;
        offset_for(&server, false).read().await;
        i.assert_async().await;
        e.assert_async().await;
    }

    #[tokio::test]
    async fn ha_unavailable_or_non_numeric_state_counts_as_zero() {
        let mut server = Server::new_async().await;
        let _i = sensor(&mut server, "sensor.import", "unavailable").await;
        let _e = sensor(&mut server, "sensor.export", "250").await;
        let r = offset_for(&server, false).read().await;
        assert_eq!(r.import, 0.0);
        assert_eq!(r.offset, -250.0);
        // "unknown", empty and garbage are the same
        for state in ["unknown", "", "abc", "1,5"] {
            let mut s = Server::new_async().await;
            let _i = sensor(&mut s, "sensor.import", state).await;
            let _e = sensor(&mut s, "sensor.export", "0").await;
            assert_eq!(offset_for(&s, false).read().await.offset, 0.0, "{state:?}");
        }
    }

    #[tokio::test]
    async fn ha_http_errors_and_bad_json_count_as_zero() {
        let mut server = Server::new_async().await;
        let _i = server
            .mock("GET", "/api/states/sensor.import")
            .with_status(401)
            .with_body("{\"message\":\"401: Unauthorized\"}")
            .create_async()
            .await;
        let _e = server
            .mock("GET", "/api/states/sensor.export")
            .with_status(200)
            .with_body("not json")
            .create_async()
            .await;
        let r = offset_for(&server, false).read().await;
        assert_eq!((r.import, r.export, r.offset), (0.0, 0.0, 0.0));
    }

    #[tokio::test]
    async fn ha_unreachable_server_counts_as_zero() {
        let url = {
            let server = Server::new_async().await;
            server.url()
        }; // server dropped, port closed
        let mut ha = HaOffset::new(
            HomeAssistantAPI::with_endpoint(url, "tok".into()),
            "sensor.import".into(),
            "sensor.export".into(),
            false,
        );
        assert_eq!(ha.read().await.offset, 0.0);
    }

    #[tokio::test]
    async fn ha_one_sensor_failing_still_uses_the_other() {
        let mut server = Server::new_async().await;
        let _i = sensor(&mut server, "sensor.import", "400").await;
        let _e = server
            .mock("GET", "/api/states/sensor.export")
            .with_status(500)
            .create_async()
            .await;
        assert_eq!(offset_for(&server, false).read().await.offset, 400.0);
    }

    #[tokio::test]
    async fn ha_without_smoothing_follows_every_change_at_once() {
        let mut server = Server::new_async().await;
        let _e = sensor(&mut server, "sensor.export", "0").await;
        let i = sensor(&mut server, "sensor.import", "100").await;
        let mut ha = offset_for(&server, false);
        assert_eq!(ha.read().await.offset, 100.0);
        i.remove_async().await;
        let _i2 = sensor(&mut server, "sensor.import", "700").await;
        assert_eq!(ha.read().await.offset, 700.0);
    }

    #[tokio::test]
    async fn ha_smoothing_reports_zero_until_the_window_is_full_then_averages() {
        let mut server = Server::new_async().await;
        let _e = sensor(&mut server, "sensor.export", "0").await;
        let _i = sensor(&mut server, "sensor.import", "100").await;
        let mut ha = offset_for(&server, true);
        // Documented behaviour of the window: nothing is applied for the first 9 samples
        for n in 1..10 {
            assert_eq!(ha.read().await.offset, 0.0, "sample {n}");
        }
        assert_eq!(ha.read().await.offset, 100.0);
        assert_eq!(ha.read().await.offset, 100.0);
    }

    #[tokio::test]
    async fn ha_smoothing_averages_over_ten_samples() {
        let mut server = Server::new_async().await;
        let _e = sensor(&mut server, "sensor.export", "0").await;
        let i = sensor(&mut server, "sensor.import", "0").await;
        let mut ha = offset_for(&server, true);
        for _ in 0..10 {
            ha.read().await;
        }
        i.remove_async().await;
        let _i2 = sensor(&mut server, "sensor.import", "1000").await;
        // One new sample of 1000 W among nine zeros: 100 W
        let r = ha.read().await;
        assert_eq!(r.import, 1000.0); // raw value is reported unsmoothed
        assert!((r.offset - 100.0).abs() < 0.01, "{}", r.offset);
        for _ in 0..8 {
            ha.read().await;
        }
        assert!((ha.read().await.offset - 1000.0).abs() < 0.01);
    }

    #[tokio::test]
    async fn ha_smoothing_counts_failed_reads_as_zero_samples() {
        let mut server = Server::new_async().await;
        let _e = sensor(&mut server, "sensor.export", "0").await;
        let i = sensor(&mut server, "sensor.import", "100").await;
        let mut ha = offset_for(&server, true);
        for _ in 0..10 {
            ha.read().await;
        }
        i.remove_async().await; // now 501 from mockito
        let r = ha.read().await;
        assert!((r.offset - 90.0).abs() < 0.01, "{}", r.offset);
    }

    // ---- offset applied to the meter registers ----

    use crate::meter_data::PhaseMeasurements;

    fn three_phases(w: f32) -> Measurements {
        let p = PhaseMeasurements {
            voltage: 230.0,
            current: 1.0,
            active_power: w,
            apparent_power: w.abs() + 10.0,
            power_factor: 0.9,
        };
        Measurements {
            total_active_power: Some(w * 3.0),
            phases: [Some(p); 3],
            ..Default::default()
        }
    }

    fn total_and_phase_a(readings: &[Readings]) -> (f32, f32) {
        let total = readings
            .iter()
            .find_map(|r| match r {
                Readings::TotalRealPower(v) => Some(*v),
                _ => None,
            })
            .unwrap();
        let a = readings
            .iter()
            .find_map(|r| match r {
                Readings::PhaseAWatts(v) => Some(*v),
                _ => None,
            })
            .unwrap();
        (total, a)
    }

    #[test]
    fn ha_offset_changes_only_the_total_not_the_phases() {
        // Documents the current behaviour: per-phase values are the Shelly's own
        let m = three_phases(100.0);
        let (total, a) = total_and_phase_a(&m.to_readings(300.0 + 500.0));
        assert_eq!(total, 800.0);
        assert_eq!(a, 100.0);
    }

    #[test]
    fn ha_offset_is_added_after_the_shelly_value_is_inverted() {
        // worker order: invert Shelly, then add the HA offset
        let mut m = three_phases(100.0);
        m.invert();
        let shelly = m.total_active_power.unwrap();
        assert_eq!(shelly, -300.0);
        let (total, a) = total_and_phase_a(&m.to_readings(shelly + 50.0));
        assert_eq!(total, -250.0);
        assert_eq!(a, -100.0);
    }
}

