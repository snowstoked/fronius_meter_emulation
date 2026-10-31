use data_fetcher::DataFetcher;
use smart_meter_emulator::SmartMeterEmulator;
use std::env;
use tokio::net::TcpListener;
use tokio_modbus::server::tcp::{Server, accept_tcp_connection};
mod data_fetcher;
mod debug;
mod home_assistant;
mod meter_data;
mod rolling_average;
mod shelly_3em_client;
mod shelly_rest_client;
mod smart_meter_emulator;
#[cfg(test)]
mod test_support;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    debug::init();
    tracing_subscriber::fmt::init();
    if debug::enabled() {
        println!("Debug logging enabled (FRONIUS_DEBUG)");
    }

    let socket_addr: std::net::SocketAddr = env::var("FRONIUS_MODBUS_BIND")
        .unwrap_or("0.0.0.0:1502".to_string())
        .parse()
        .unwrap();
    println!("Starting Fronius modbus bridge on {socket_addr}");

    let (emulated_meter, meter_update_handle) = SmartMeterEmulator::new();
    let _data_fetcher = DataFetcher::new(meter_update_handle);

    //Start fake meter
    println!("Starting up server on {socket_addr}");
    let listener = TcpListener::bind(socket_addr).await?;
    server_context(listener, emulated_meter)
        .await
        .expect("Should never exit fake meter");

    Ok(())
}

async fn server_context(
    listener: TcpListener,
    emulated_meter: SmartMeterEmulator,
) -> anyhow::Result<()> {
    let server = Server::new(listener);
    let new_service = |_socket_addr| Ok(Some(emulated_meter.clone()));
    let on_connected = |stream, socket_addr| async move {
        accept_tcp_connection(stream, socket_addr, new_service)
    };
    let on_process_error = |err| {
        eprintln!("{err}");
    };
    server.serve(&on_connected, on_process_error).await?;
    Ok(())
}

/// Whole system: fake Shelly (Modbus) + fake Home Assistant -> fetcher -> emulated meter -> a Modbus client that
/// behaves like the inverter.
#[cfg(test)]
mod system_tests {
    use super::*;
    use crate::test_support::{ALL_ENV, ENV_LOCK, FakeShelly, clear_env, set_env};
    use std::time::{Duration, Instant};
    use tokio_modbus::{client::tcp, prelude::*};

    // SunSpec model 213 registers as the inverter reads them
    const TOTAL_POWER: u16 = 40097;
    const POWER_A: u16 = 40099;
    const POWER_B: u16 = 40101;
    const POWER_C: u16 = 40103;
    const VOLTAGE_A: u16 = 40081;
    const FREQUENCY: u16 = 40095;

    struct System {
        shelly: FakeShelly,
        inverter: tokio_modbus::client::Context,
        addr: std::net::SocketAddr,
        _guard: tokio::sync::MutexGuard<'static, ()>,
    }

    fn shelly_3em() -> FakeShelly {
        let shelly = FakeShelly::new(31000);
        shelly.set_total_power(500.0);
        shelly.set_phase(0, 231.0, 1.0, 200.0, 230.0, 0.9);
        shelly.set_phase(1, 232.0, 1.0, 150.0, 200.0, 0.8);
        shelly.set_phase(2, 233.0, 1.0, 150.0, 190.0, 0.7);
        shelly.set_frequency(50.01);
        shelly.set_energy(1000.0, 200.0);
        shelly
    }

    async fn start(env: &[(&str, &str)], unit: u8) -> System {
        let guard = ENV_LOCK.lock().await;
        clear_env(ALL_ENV);
        let shelly = shelly_3em();
        let shelly_addr = shelly.clone().spawn().await;
        set_env(&[
            ("SHELLY_MODBUS", &shelly_addr.to_string()),
            ("SHELLY_POLL_INTERVAL_MS", "100"),
        ]);
        set_env(env);

        let (emulated_meter, handle) = SmartMeterEmulator::new();
        let _fetcher = DataFetcher::new(handle);
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(server_context(listener, emulated_meter));
        let inverter = tcp::connect_slave(addr, Slave(unit)).await.unwrap();
        // The fetcher reads its settings when it first runs; give it the chance before the next test changes them
        tokio::time::sleep(Duration::from_millis(50)).await;
        System {
            shelly,
            inverter,
            addr,
            _guard: guard,
        }
    }

    fn f32_at(regs: &[u16], first: u16, register: u16) -> f32 {
        let i = (register - first) as usize;
        // SunSpec: high word first
        f32::from_bits((regs[i] as u32) << 16 | regs[i + 1] as u32)
    }

    impl System {
        async fn block(&mut self) -> Vec<u16> {
            self.inverter
                .read_holding_registers(40071, 58)
                .await
                .unwrap()
                .unwrap()
        }

        /// Poll like an inverter until `check` accepts the 40071..40129 block (or 5 s have passed)
        async fn wait_for(&mut self, check: impl Fn(&[u16]) -> bool) -> Vec<u16> {
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                let block = self.block().await;
                if check(&block) {
                    return block;
                }
                assert!(
                    Instant::now() < deadline,
                    "registers never reached the expected state; total {}W",
                    f32_at(&block, 40071, TOTAL_POWER)
                );
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        }
    }

    fn total(block: &[u16]) -> f32 {
        f32_at(block, 40071, TOTAL_POWER)
    }

    #[tokio::test]
    async fn shelly_values_reach_the_inverter_without_home_assistant() {
        let mut sys = start(&[], 1).await;
        // wait until the slow reads (phases) have arrived too
        let b = sys
            .wait_for(|b| f32_at(b, 40071, VOLTAGE_A) > 1.0 && f32_at(b, 40071, FREQUENCY) > 1.0)
            .await;
        assert_eq!(total(&b), 500.0);
        assert_eq!(f32_at(&b, 40071, POWER_A), 200.0);
        assert_eq!(f32_at(&b, 40071, VOLTAGE_A), 231.0);
        assert!((f32_at(&b, 40071, FREQUENCY) - 50.01).abs() < 1e-4);
    }

    #[tokio::test]
    async fn changes_on_the_shelly_follow_within_a_few_cycles() {
        let mut sys = start(&[], 1).await;
        sys.wait_for(|b| total(b) == 500.0).await;
        sys.shelly.set_total_power(-2500.0);
        sys.wait_for(|b| total(b) == -2500.0).await;
    }

    #[tokio::test]
    async fn home_assistant_offset_is_added_to_the_total_and_split_over_the_phases() {
        let mut ha = mockito::Server::new_async().await;
        let _i = ha
            .mock("GET", "/api/states/sensor.extra_in")
            .match_header("Authorization", "Bearer secret")
            .with_body(r#"{"entity_id":"a","state":"300","last_changed":"x","last_reported":"x","last_updated":"x"}"#)
            .expect_at_least(1)
            .create_async()
            .await;
        let _e = ha
            .mock("GET", "/api/states/sensor.extra_out")
            .with_body(r#"{"entity_id":"b","state":"100","last_changed":"x","last_reported":"x","last_updated":"x"}"#)
            .expect_at_least(1)
            .create_async()
            .await;
        let url = ha.url();
        let mut sys = start(
            &[
                ("HA_URL", url.as_str()),
                ("HA_TOKEN", "secret"),
                ("HA_EXTRA_IMPORT", "sensor.extra_in"),
                ("HA_EXTRA_EXPORT", "sensor.extra_out"),
            ],
            1,
        )
        .await;
        let b = sys.wait_for(|b| f32_at(b, 40071, VOLTAGE_A) > 1.0).await;
        // 500 W from the Shelly + (300 - 100) W from Home Assistant
        assert_eq!(total(&b), 700.0);
        // Each of the three phases carries a third of the 200 W
        assert!((f32_at(&b, 40071, POWER_A) - (200.0 + 200.0 / 3.0)).abs() < 0.01);
        assert!((f32_at(&b, 40071, POWER_B) - (150.0 + 200.0 / 3.0)).abs() < 0.01);
        assert!((f32_at(&b, 40071, POWER_C) - (150.0 + 200.0 / 3.0)).abs() < 0.01);
        // And it follows the Shelly
        sys.shelly.set_total_power(-1000.0);
        sys.wait_for(|b| total(b) == -800.0).await;
    }

    #[tokio::test]
    async fn home_assistant_offset_can_be_limited_to_the_total() {
        let mut ha = mockito::Server::new_async().await;
        let body = |s: &str| {
            format!(r#"{{"entity_id":"a","state":"{s}","last_changed":"x","last_reported":"x","last_updated":"x"}}"#)
        };
        let _i = ha.mock("GET", "/api/states/in").with_body(body("300")).create_async().await;
        let _e = ha.mock("GET", "/api/states/out").with_body(body("100")).create_async().await;
        let url = ha.url();
        let mut sys = start(
            &[
                ("HA_URL", url.as_str()),
                ("HA_TOKEN", "t"),
                ("HA_EXTRA_IMPORT", "in"),
                ("HA_EXTRA_EXPORT", "out"),
                ("HA_OFFSET_PER_PHASE", "false"),
            ],
            1,
        )
        .await;
        let b = sys.wait_for(|b| f32_at(b, 40071, VOLTAGE_A) > 1.0).await;
        assert_eq!(total(&b), 700.0);
        assert_eq!(f32_at(&b, 40071, POWER_A), 200.0);
    }

    #[tokio::test]
    async fn home_assistant_offset_with_inverted_shelly() {
        let mut ha = mockito::Server::new_async().await;
        let body = |s: &str| {
            format!(r#"{{"entity_id":"a","state":"{s}","last_changed":"x","last_reported":"x","last_updated":"x"}}"#)
        };
        let _i = ha.mock("GET", "/api/states/in").with_body(body("50")).create_async().await;
        let _e = ha.mock("GET", "/api/states/out").with_body(body("0")).create_async().await;
        let url = ha.url();
        let mut sys = start(
            &[
                ("HA_URL", url.as_str()),
                ("HA_TOKEN", "t"),
                ("HA_EXTRA_IMPORT", "in"),
                ("HA_EXTRA_EXPORT", "out"),
                ("SHELLY_INVERT", "true"),
            ],
            1,
        )
        .await;
        // Shelly +500 is flipped to -500, then the offset of +50 is added
        let b = sys.wait_for(|b| f32_at(b, 40071, VOLTAGE_A) > 1.0).await;
        assert_eq!(total(&b), -450.0);
        assert!((f32_at(&b, 40071, POWER_A) - (-200.0 + 50.0 / 3.0)).abs() < 0.01);
    }

    #[tokio::test]
    async fn broken_home_assistant_does_not_disturb_the_meter() {
        let mut ha = mockito::Server::new_async().await;
        let _any = ha.mock("GET", mockito::Matcher::Any).with_status(500).create_async().await;
        let url = ha.url();
        let mut sys = start(
            &[
                ("HA_URL", url.as_str()),
                ("HA_TOKEN", "t"),
                ("HA_EXTRA_IMPORT", "in"),
                ("HA_EXTRA_EXPORT", "out"),
            ],
            1,
        )
        .await;
        sys.wait_for(|b| total(b) == 500.0).await;
    }

    #[tokio::test]
    async fn smoothed_offset_applies_from_the_first_sample() {
        let mut ha = mockito::Server::new_async().await;
        let body = |s: &str| {
            format!(r#"{{"entity_id":"a","state":"{s}","last_changed":"x","last_reported":"x","last_updated":"x"}}"#)
        };
        let _i = ha.mock("GET", "/api/states/in").with_body(body("1000")).create_async().await;
        let _e = ha.mock("GET", "/api/states/out").with_body(body("0")).create_async().await;
        let url = ha.url();
        let mut sys = start(
            &[
                ("HA_URL", url.as_str()),
                ("HA_TOKEN", "t"),
                ("HA_EXTRA_IMPORT", "in"),
                ("HA_EXTRA_EXPORT", "out"),
                ("HA_SMOOTH", "true"),
            ],
            1,
        )
        .await;
        // No warm-up period: the first sample is already averaged over what there is, and Home Assistant is steady
        sys.wait_for(|b| total(b) == 1500.0).await;
    }

    #[tokio::test]
    async fn hanging_home_assistant_does_not_stall_the_meter() {
        // accepts connections and never answers
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let _held = tokio::spawn(async move {
            let mut held = Vec::new();
            while let Ok((socket, _)) = listener.accept().await {
                held.push(socket);
            }
        });
        let mut sys = start(
            &[
                ("HA_URL", url.as_str()),
                ("HA_TOKEN", "t"),
                ("HA_EXTRA_IMPORT", "in"),
                ("HA_EXTRA_EXPORT", "out"),
                ("HA_TIMEOUT_MS", "300"),
            ],
            1,
        )
        .await;
        sys.wait_for(|b| total(b) == 500.0).await;
        // After the first failed attempt Home Assistant is left alone for a while, so changes arrive quickly
        let changed = Instant::now();
        sys.shelly.set_total_power(-250.0);
        sys.wait_for(|b| total(b) == -250.0).await;
        assert!(changed.elapsed() < Duration::from_secs(2), "{:?}", changed.elapsed());
    }

    #[tokio::test]
    async fn home_assistant_going_away_keeps_the_last_offset_for_a_while() {
        let mut ha = mockito::Server::new_async().await;
        let body = |s: &str| {
            format!(r#"{{"entity_id":"a","state":"{s}","last_changed":"x","last_reported":"x","last_updated":"x"}}"#)
        };
        let i = ha.mock("GET", "/api/states/in").with_body(body("200")).create_async().await;
        let _e = ha.mock("GET", "/api/states/out").with_body(body("0")).create_async().await;
        let url = ha.url();
        let mut sys = start(
            &[
                ("HA_URL", url.as_str()),
                ("HA_TOKEN", "t"),
                ("HA_EXTRA_IMPORT", "in"),
                ("HA_EXTRA_EXPORT", "out"),
                ("HA_RETRY_S", "0"),
            ],
            1,
        )
        .await;
        sys.wait_for(|b| total(b) == 700.0).await;
        i.remove_async().await; // mockito answers 501 from now on
        tokio::time::sleep(Duration::from_millis(500)).await;
        // still 500 + 200, not 500
        assert_eq!(total(&sys.block().await), 700.0);
    }

    #[tokio::test]
    async fn home_assistant_offset_falls_back_to_zero_when_the_value_is_too_old() {
        let mut ha = mockito::Server::new_async().await;
        let body = |s: &str| {
            format!(r#"{{"entity_id":"a","state":"{s}","last_changed":"x","last_reported":"x","last_updated":"x"}}"#)
        };
        let i = ha.mock("GET", "/api/states/in").with_body(body("200")).create_async().await;
        let _e = ha.mock("GET", "/api/states/out").with_body(body("0")).create_async().await;
        let url = ha.url();
        let mut sys = start(
            &[
                ("HA_URL", url.as_str()),
                ("HA_TOKEN", "t"),
                ("HA_EXTRA_IMPORT", "in"),
                ("HA_EXTRA_EXPORT", "out"),
                ("HA_MAX_STALE_S", "0"),
                ("HA_RETRY_S", "0"),
            ],
            1,
        )
        .await;
        sys.wait_for(|b| total(b) == 700.0).await;
        i.remove_async().await;
        sys.wait_for(|b| total(b) == 500.0).await;
    }

    #[tokio::test]
    async fn each_unit_id_sees_its_own_serial() {
        let mut sys = start(&[], 2).await;
        let serial = |regs: Vec<u16>| -> String {
            regs.into_iter().filter(|c| *c != 0).map(|c| c as u8 as char).collect()
        };
        let id = sys.inverter.read_holding_registers(40052, 16).await.unwrap().unwrap();
        assert_eq!(serial(id), "00000002");
        let mut other = tcp::connect_slave(sys.addr, Slave(7)).await.unwrap();
        let id = other.read_holding_registers(40052, 16).await.unwrap().unwrap();
        assert_eq!(serial(id), "00000007");
    }

    #[tokio::test]
    async fn sunspec_identity_and_discovery_reads_work() {
        let mut sys = start(&[], 1).await;
        let marker = sys.inverter.read_holding_registers(40000, 2).await.unwrap().unwrap();
        assert_eq!(marker, vec![0x5375, 0x6e53]); // "SunS"
        let model = sys.inverter.read_holding_registers(40069, 2).await.unwrap().unwrap();
        assert_eq!(model, vec![213, 124]);
        let end = sys.inverter.read_holding_registers(40195, 2).await.unwrap().unwrap();
        assert_eq!(end, vec![0xFFFF, 0]);
        // Input registers answer too (Fronius uses both)
        assert!(sys.inverter.read_input_registers(40071, 2).await.unwrap().is_ok());
    }

    #[tokio::test]
    async fn writes_and_unknown_registers_are_refused() {
        let mut sys = start(&[], 1).await;
        let write = sys.inverter.write_single_register(40071, 1).await.unwrap();
        assert!(write.is_err());
        let unknown = sys.inverter.read_holding_registers(10, 2).await.unwrap();
        assert!(unknown.is_err());
    }

    #[tokio::test]
    async fn two_inverter_connections_are_served_at_once() {
        let mut sys = start(&[], 1).await;
        let mut second = tcp::connect_slave(sys.addr, Slave(1)).await.unwrap();
        sys.wait_for(|b| total(b) == 500.0).await;
        let b = second.read_holding_registers(40071, 58).await.unwrap().unwrap();
        assert_eq!(total(&b), 500.0);
    }
}
