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

/// A Home Assistant value that can no longer be read is kept for this long, then it counts as 0 W
const DEFAULT_HA_MAX_STALE: Duration = Duration::from_secs(30);
/// After a failed request a sensor is not asked again for this long, so that an unreachable Home Assistant doesn't
/// slow down every cycle of the power reading
const DEFAULT_HA_RETRY: Duration = Duration::from_secs(5);

fn env_secs(name: &str, default: Duration) -> Duration {
    env::var(name)
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .map(Duration::from_secs)
        .unwrap_or(default)
}

/// `false`, `0`, `no` and `off` switch it off, anything else (including unset) leaves it on
fn env_bool_default_true(name: &str) -> bool {
    flag_default_true(env::var(name).ok().as_deref())
}

fn flag_default_true(value: Option<&str>) -> bool {
    !matches!(
        value
            .unwrap_or_default()
            .trim()
            .to_ascii_lowercase()
            .as_str(),
        "false" | "0" | "no" | "off"
    )
}

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
        )
        .with_timing(
            env_secs("HA_MAX_STALE_S", DEFAULT_HA_MAX_STALE),
            env_secs("HA_RETRY_S", DEFAULT_HA_RETRY),
        );
        let offset_per_phase = env_bool_default_true("HA_OFFSET_PER_PHASE");

        if !ha.api.is_configured() {
            println!("Home assistant integration disabled")
        } else {
            println!(
                "Home Assistant offset: {}",
                if offset_per_phase {
                    "split over the phases"
                } else {
                    "total power only (HA_OFFSET_PER_PHASE=false)"
                }
            );
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
                ..
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
            let readings = if offset_per_phase {
                state.with_offset(ha_offset).to_readings(summed_power)
            } else {
                state.to_readings(summed_power)
            };
            output
                .send(Readings::Batch(readings))
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
    /// At least one sensor could not be read in this cycle (its last value was used, or 0 W once that is too old)
    pub degraded: bool,
}

impl HaReading {
    const NONE: HaReading = HaReading {
        import: 0.0,
        export: 0.0,
        offset: 0.0,
        degraded: false,
    };
}

/// One Home Assistant sensor and what is known about its health
struct HaSensor {
    name: String,
    /// Last value that was read successfully, and when
    last_good: Option<(f32, Instant)>,
    failing: bool,
    /// While failing: don't ask again before this
    retry_at: Instant,
    /// The "treated as 0 W" warning has been given for this failure
    zero_logged: bool,
}

impl HaSensor {
    fn new(name: String) -> Self {
        Self {
            name,
            last_good: None,
            failing: false,
            retry_at: Instant::now(),
            zero_logged: false,
        }
    }

    fn used(&self) -> bool {
        !self.name.trim().is_empty()
    }

    fn due(&self, now: Instant) -> bool {
        self.used() && (!self.failing || now >= self.retry_at)
    }

    /// Only finite numbers are values: `unavailable`, `unknown`, empty and garbage are failures (and so are `nan`
    /// and `inf`, which Rust would happily parse)
    fn parse(state: &str) -> Result<f32, String> {
        match state.trim().parse::<f32>() {
            Ok(v) if v.is_finite() => Ok(v),
            _ => Err(format!("state is `{state}`, not a number")),
        }
    }

    fn record(&mut self, result: Result<f32, String>, now: Instant, retry: Duration) {
        match result {
            Ok(value) => {
                if self.failing {
                    warn_log!("Home Assistant sensor `{}` is readable again", self.name);
                }
                self.failing = false;
                self.zero_logged = false;
                self.last_good = Some((value, now));
            }
            Err(e) => {
                if !self.failing {
                    warn_log!(
                        "Home Assistant sensor `{}` can't be read: {e}. Keeping the last value for a while, retrying every {}s",
                        self.name,
                        retry.as_secs()
                    );
                }
                self.failing = true;
                self.retry_at = now + retry;
            }
        }
    }

    /// The value to use now: the last good one unless it is older than `max_stale`
    fn value(&mut self, now: Instant, max_stale: Duration) -> f32 {
        match self.last_good {
            Some((value, at)) if now.duration_since(at) <= max_stale => value,
            _ => {
                if self.failing && !self.zero_logged {
                    warn_log!(
                        "Home Assistant sensor `{}` has been unreadable for more than {}s, counting it as 0 W",
                        self.name,
                        max_stale.as_secs()
                    );
                    self.zero_logged = true;
                }
                0.0
            }
        }
    }
}

/// Extra consumption/production that the Shelly doesn't see, taken from two Home Assistant sensors.
///
/// A sensor that can't be read (no answer within the timeout, an error, `unavailable`, not a number) keeps its last
/// value for `max_stale`, then counts as 0 W. It isn't asked again for `retry_after`, so a dead Home Assistant costs
/// one timeout per `retry_after` rather than one per cycle. Failures are logged once, not every cycle, and a failed
/// read is never fed into the smoothing as a 0.
pub struct HaOffset {
    api: HomeAssistantAPI,
    import: HaSensor,
    export: HaSensor,
    smooth: bool,
    filter: RollingAverage,
    max_stale: Duration,
    retry_after: Duration,
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
            import: HaSensor::new(import_sensor),
            export: HaSensor::new(export_sensor),
            smooth,
            filter: RollingAverage::default(),
            max_stale: DEFAULT_HA_MAX_STALE,
            retry_after: DEFAULT_HA_RETRY,
        }
    }

    pub fn with_timing(mut self, max_stale: Duration, retry_after: Duration) -> Self {
        self.max_stale = max_stale;
        self.retry_after = retry_after;
        self
    }

    pub async fn read(&mut self) -> HaReading {
        if !self.api.is_configured() {
            return HaReading::NONE;
        }
        let now = Instant::now();
        // Both sensors at the same time: a slow Home Assistant costs one timeout, not two
        let (import_result, export_result) = tokio::join!(
            Self::fetch(&self.api, &self.import, now),
            Self::fetch(&self.api, &self.export, now)
        );
        let retry = self.retry_after;
        if let Some(r) = import_result {
            self.import.record(r, now, retry);
        }
        if let Some(r) = export_result {
            self.export.record(r, now, retry);
        }

        let degraded = self.import.failing || self.export.failing;
        let import = if self.import.used() {
            self.import.value(now, self.max_stale)
        } else {
            0.0
        };
        let export = if self.export.used() {
            self.export.value(now, self.max_stale)
        } else {
            0.0
        };
        let raw = import - export;
        let offset = if self.smooth {
            let both_gone = degraded && self.no_usable_value(now);
            if both_gone {
                // Nothing to average any more; start over once Home Assistant is back
                self.filter.reset();
                0.0
            } else {
                self.filter.add(raw)
            }
        } else {
            raw
        };
        HaReading {
            import,
            export,
            offset,
            degraded,
        }
    }

    /// True when every sensor in use is failing and has no value left within `max_stale`
    fn no_usable_value(&self, now: Instant) -> bool {
        [&self.import, &self.export]
            .iter()
            .filter(|s| s.used())
            .all(|s| {
                s.failing
                    && s.last_good
                        .is_none_or(|(_, at)| now.duration_since(at) > self.max_stale)
            })
    }

    /// `None`: not asked this time (unused sensor, or still waiting after a failure)
    async fn fetch(
        api: &HomeAssistantAPI,
        sensor: &HaSensor,
        now: Instant,
    ) -> Option<Result<f32, String>> {
        if !sensor.due(now) {
            return None;
        }
        Some(match api.read_sensor_value(&sensor.name).await {
            Ok(res) => HaSensor::parse(&res.state),
            Err(e) => Err(format!("{e:#}")),
        })
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

    #[test]
    fn default_true_flag() {
        assert!(flag_default_true(None));
        assert!(flag_default_true(Some("")));
        assert!(flag_default_true(Some("true")));
        assert!(flag_default_true(Some("anything")));
        for off in ["false", "FALSE", " 0 ", "no", "Off"] {
            assert!(!flag_default_true(Some(off)), "{off}");
        }
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
        // Tests that want to see a retry or a stale value say so; by default nothing expires
        .with_timing(Duration::from_secs(3600), Duration::ZERO)
    }

    #[tokio::test]
    async fn ha_not_configured_gives_zero_and_no_requests() {
        let mut ha = HaOffset::new(
            HomeAssistantAPI::with_endpoint(String::new(), String::new()),
            "sensor.import".into(),
            "sensor.export".into(),
            false,
        );
        assert_eq!(ha.read().await, HaReading::NONE);
        // Only one of the two set is still "not configured"
        let mut ha = HaOffset::new(
            HomeAssistantAPI::with_endpoint("http://x".into(), String::new()),
            "a".into(),
            "b".into(),
            true,
        );
        assert_eq!(ha.read().await, HaReading::NONE);
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
        assert!(!r.degraded);
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
    async fn ha_sensor_without_a_name_is_not_requested() {
        let mut server = Server::new_async().await;
        let i = server
            .mock("GET", "/api/states/sensor.import")
            .with_body(sensor_body("sensor.import", "250"))
            .expect(1)
            .create_async()
            .await;
        let none = server
            .mock("GET", mockito::Matcher::Regex("^/api/states/$".into()))
            .expect(0)
            .create_async()
            .await;
        let mut ha = HaOffset::new(
            HomeAssistantAPI::with_endpoint(server.url(), "tok".into()),
            "sensor.import".into(),
            String::new(),
            false,
        );
        let r = ha.read().await;
        assert_eq!((r.import, r.export, r.offset), (250.0, 0.0, 250.0));
        assert!(!r.degraded, "an unused sensor is not a failure");
        i.assert_async().await;
        none.assert_async().await;
    }

    #[tokio::test]
    async fn ha_unavailable_or_non_numeric_state_is_a_failure_with_zero_when_nothing_was_ever_read()
    {
        let mut server = Server::new_async().await;
        let _i = sensor(&mut server, "sensor.import", "unavailable").await;
        let _e = sensor(&mut server, "sensor.export", "250").await;
        let r = offset_for(&server, false).read().await;
        assert_eq!(r.import, 0.0);
        assert_eq!(r.offset, -250.0);
        assert!(r.degraded);
        // "unknown", empty, garbage, decimal comma, and the floats Rust would accept but that aren't power values
        for state in ["unknown", "", "abc", "1,5", "NaN", "inf", "-inf"] {
            let mut s = Server::new_async().await;
            let _i = sensor(&mut s, "sensor.import", state).await;
            let _e = sensor(&mut s, "sensor.export", "0").await;
            let r = offset_for(&s, false).read().await;
            assert_eq!(r.offset, 0.0, "{state:?}");
            assert!(r.degraded, "{state:?}");
        }
    }

    #[tokio::test]
    async fn ha_state_with_whitespace_still_parses() {
        let mut server = Server::new_async().await;
        let _i = sensor(&mut server, "sensor.import", " 12.5 ").await;
        let _e = sensor(&mut server, "sensor.export", "-3").await;
        let r = offset_for(&server, false).read().await;
        assert_eq!(r.offset, 15.5);
        assert!(!r.degraded);
    }

    #[tokio::test]
    async fn ha_http_errors_and_bad_json_are_failures() {
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
        assert!(r.degraded);
    }

    #[tokio::test]
    async fn ha_unreachable_server_is_a_failure() {
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
        let r = ha.read().await;
        assert_eq!(r.offset, 0.0);
        assert!(r.degraded);
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
        let r = offset_for(&server, false).read().await;
        assert_eq!(r.offset, 400.0);
        assert!(r.degraded);
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

    // ---- failures: hold, expire, back off, recover ----

    #[tokio::test]
    async fn ha_failed_read_keeps_the_last_value() {
        let mut server = Server::new_async().await;
        let _e = sensor(&mut server, "sensor.export", "0").await;
        let i = sensor(&mut server, "sensor.import", "400").await;
        let mut ha = offset_for(&server, false);
        assert_eq!(ha.read().await.offset, 400.0);
        i.remove_async().await; // mockito answers 501 from now on
        let r = ha.read().await;
        assert_eq!(r.offset, 400.0, "last value is kept, not replaced by 0");
        assert_eq!(r.import, 400.0);
        assert!(r.degraded);
    }

    #[tokio::test]
    async fn ha_kept_value_expires_into_zero() {
        let mut server = Server::new_async().await;
        let _e = sensor(&mut server, "sensor.export", "0").await;
        let i = sensor(&mut server, "sensor.import", "400").await;
        let mut ha =
            offset_for(&server, false).with_timing(Duration::from_millis(150), Duration::ZERO);
        assert_eq!(ha.read().await.offset, 400.0);
        i.remove_async().await;
        assert_eq!(ha.read().await.offset, 400.0);
        tokio::time::sleep(Duration::from_millis(200)).await;
        let r = ha.read().await;
        assert_eq!(r.offset, 0.0);
        assert!(r.degraded);
    }

    #[tokio::test]
    async fn ha_failed_sensor_is_not_asked_again_before_the_retry_time() {
        let mut server = Server::new_async().await;
        let _e = sensor(&mut server, "sensor.export", "0").await;
        let bad = server
            .mock("GET", "/api/states/sensor.import")
            .with_status(500)
            .expect(1)
            .create_async()
            .await;
        let mut ha = offset_for(&server, false)
            .with_timing(Duration::from_secs(60), Duration::from_secs(60));
        for _ in 0..5 {
            ha.read().await;
        }
        bad.assert_async().await; // one request, not five
    }

    #[tokio::test]
    async fn ha_failed_sensor_is_asked_again_after_the_retry_time_and_recovers() {
        let mut server = Server::new_async().await;
        let _e = sensor(&mut server, "sensor.export", "0").await;
        let bad = server
            .mock("GET", "/api/states/sensor.import")
            .with_status(500)
            .create_async()
            .await;
        let mut ha = offset_for(&server, false)
            .with_timing(Duration::from_secs(60), Duration::from_millis(100));
        assert!(ha.read().await.degraded);
        bad.remove_async().await;
        let _good = sensor(&mut server, "sensor.import", "250").await;
        // still waiting
        let r = ha.read().await;
        assert!(r.degraded);
        assert_eq!(r.offset, 0.0);
        tokio::time::sleep(Duration::from_millis(150)).await;
        let r = ha.read().await;
        assert!(!r.degraded);
        assert_eq!(r.offset, 250.0);
    }

    #[tokio::test]
    async fn ha_does_not_wait_longer_than_one_timeout_for_two_hanging_sensors() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let task = tokio::spawn(async move {
            let mut held = Vec::new();
            while let Ok((socket, _)) = listener.accept().await {
                held.push(socket);
            }
        });
        let mut ha = HaOffset::new(
            HomeAssistantAPI::with_endpoint(url, "tok".into())
                .with_timeout(Duration::from_millis(300)),
            "a".into(),
            "b".into(),
            false,
        )
        .with_timing(Duration::from_secs(30), Duration::from_secs(60));
        let started = Instant::now();
        let r = ha.read().await;
        let first = started.elapsed();
        assert!(r.degraded);
        assert!(
            first < Duration::from_millis(550),
            "both sensors wait together: {first:?}"
        );
        // and then it leaves Home Assistant alone for the retry time: the next cycle is instant
        let started = Instant::now();
        ha.read().await;
        assert!(
            started.elapsed() < Duration::from_millis(50),
            "{:?}",
            started.elapsed()
        );
        task.abort();
    }

    // ---- smoothing ----

    #[tokio::test]
    async fn ha_smoothing_applies_from_the_first_sample() {
        let mut server = Server::new_async().await;
        let _e = sensor(&mut server, "sensor.export", "0").await;
        let _i = sensor(&mut server, "sensor.import", "100").await;
        let mut ha = offset_for(&server, true);
        // No dead time while the window fills: the average of what there is
        for n in 1..=12 {
            assert_eq!(ha.read().await.offset, 100.0, "sample {n}");
        }
    }

    #[tokio::test]
    async fn ha_smoothing_ramps_while_the_window_fills() {
        let mut server = Server::new_async().await;
        let _e = sensor(&mut server, "sensor.export", "0").await;
        let i = sensor(&mut server, "sensor.import", "0").await;
        let mut ha = offset_for(&server, true);
        assert_eq!(ha.read().await.offset, 0.0);
        i.remove_async().await;
        let _i2 = sensor(&mut server, "sensor.import", "300").await;
        // (0 + 300) / 2, (0 + 300 + 300) / 3
        assert_eq!(ha.read().await.offset, 150.0);
        assert_eq!(ha.read().await.offset, 200.0);
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
    async fn ha_smoothing_does_not_average_in_failed_reads_as_zero() {
        let mut server = Server::new_async().await;
        let _e = sensor(&mut server, "sensor.export", "0").await;
        let i = sensor(&mut server, "sensor.import", "100").await;
        let mut ha = offset_for(&server, true);
        for _ in 0..10 {
            ha.read().await;
        }
        i.remove_async().await; // now 501 from mockito
        for _ in 0..5 {
            let r = ha.read().await;
            assert!((r.offset - 100.0).abs() < 0.01, "{}", r.offset);
            assert!(r.degraded);
        }
    }

    #[tokio::test]
    async fn ha_smoothing_starts_over_after_everything_expired() {
        let mut server = Server::new_async().await;
        let _e = sensor(&mut server, "sensor.export", "0").await;
        let i = sensor(&mut server, "sensor.import", "1000").await;
        let mut ha =
            offset_for(&server, true).with_timing(Duration::from_millis(100), Duration::ZERO);
        for _ in 0..10 {
            ha.read().await;
        }
        i.remove_async().await;
        let _e2 = server
            .mock("GET", "/api/states/sensor.export")
            .with_status(500)
            .create_async()
            .await;
        tokio::time::sleep(Duration::from_millis(150)).await;
        assert_eq!(ha.read().await.offset, 0.0);
        // Home Assistant comes back with a small value: no memory of the old 1000 W
        server.reset();
        let _i3 = sensor(&mut server, "sensor.import", "20").await;
        let _e3 = sensor(&mut server, "sensor.export", "0").await;
        let r = ha.read().await;
        assert_eq!(r.offset, 20.0);
        assert!(!r.degraded);
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

    fn find(readings: &[Readings], f: impl Fn(&Readings) -> Option<f32>) -> f32 {
        readings.iter().find_map(f).unwrap()
    }

    fn total_and_phases(readings: &[Readings]) -> (f32, [f32; 3]) {
        let total = find(readings, |r| match r {
            Readings::TotalRealPower(v) => Some(*v),
            _ => None,
        });
        let a = find(readings, |r| match r {
            Readings::PhaseAWatts(v) => Some(*v),
            _ => None,
        });
        let b = find(readings, |r| match r {
            Readings::PhaseBWatts(v) => Some(*v),
            _ => None,
        });
        let c = find(readings, |r| match r {
            Readings::PhaseCWatts(v) => Some(*v),
            _ => None,
        });
        (total, [a, b, c])
    }

    #[test]
    fn ha_offset_reaches_the_phases_and_they_still_add_up_to_the_total() {
        let m = three_phases(100.0);
        let shelly = m.total_active_power.unwrap();
        let offset = 600.0;
        let (total, phases) = total_and_phases(&m.with_offset(offset).to_readings(shelly + offset));
        assert_eq!(total, 900.0);
        assert_eq!(phases, [300.0, 300.0, 300.0]);
        assert_eq!(phases.iter().sum::<f32>(), total);
    }

    #[test]
    fn ha_offset_is_added_after_the_shelly_value_is_inverted() {
        // worker order: invert Shelly, then add the HA offset
        let mut m = three_phases(100.0);
        m.invert();
        let shelly = m.total_active_power.unwrap();
        assert_eq!(shelly, -300.0);
        let (total, phases) = total_and_phases(&m.with_offset(50.0).to_readings(shelly + 50.0));
        assert_eq!(total, -250.0);
        let sum: f32 = phases.iter().sum();
        assert!((sum - total).abs() < 0.01);
        assert!((phases[0] - (-100.0 + 50.0 / 3.0)).abs() < 0.01);
    }
}
