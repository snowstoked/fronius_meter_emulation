//! Helpers shared by the tests: a fake Shelly that speaks Modbus TCP, and a lock for tests that need
//! process-wide environment variables.

use std::{
    collections::HashMap,
    future,
    net::SocketAddr,
    pin::Pin,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use tokio::net::TcpListener;
use tokio_modbus::{
    ExceptionCode,
    prelude::*,
    server::tcp::{Server, accept_tcp_connection},
};

/// Tests that set environment variables (the fetcher reads its settings from there) hold this for their duration
pub static ENV_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

pub fn set_env(vars: &[(&str, &str)]) {
    for (k, v) in vars {
        unsafe { std::env::set_var(k, v) };
    }
}

pub fn clear_env(names: &[&str]) {
    for k in names {
        unsafe { std::env::remove_var(k) };
    }
}

/// Every variable the program reads, so that a test starts from a clean slate
pub const ALL_ENV: &[&str] = &[
    "HA_URL",
    "HA_TOKEN",
    "HA_EXTRA_IMPORT",
    "HA_EXTRA_EXPORT",
    "HA_SMOOTH",
    "HA_TIMEOUT_MS",
    "HA_MAX_STALE_S",
    "HA_RETRY_S",
    "HA_OFFSET_PER_PHASE",
    "SLOW_METER_READ_INTERVAL_S",
    "SHELLY_POLL_INTERVAL_MS",
    "SHELLY_INVERT",
    "SHELLY_REST",
    "SHELLY_MODBUS",
    "SHELLY_ID",
    "SHELLY_EM_ID",
    "SHELLY_IP",
    "SHELLY_PORT",
    "FRONIUS_SERIAL",
    "FRONIUS_MODBUS_ADDRESS",
    "FRONIUS_DEVICE_ADDRESS",
    "FRONIUS_MODBUS_BIND",
    "FRONIUS_DEBUG",
];

pub fn f32_regs(value: f32) -> [u16; 2] {
    let bits = value.to_bits();
    // Shelly: low word first
    [(bits & 0xFFFF) as u16, (bits >> 16) as u16]
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Behaviour {
    Normal,
    /// Answer IllegalDataAddress for anything outside the served range
    Refuse,
    /// Accept the request but never answer
    Hang,
}

/// A Shelly Pro 3EM as far as the emulator uses it. The component block is served at `base`.
#[derive(Clone)]
pub struct FakeShelly {
    pub base: u16,
    pub registers: Arc<Mutex<HashMap<u16, u16>>>,
    pub behaviour: Arc<Mutex<Behaviour>>,
    pub reads: Arc<AtomicUsize>,
    pub connections: Arc<AtomicUsize>,
}

impl FakeShelly {
    pub fn new(base: u16) -> Self {
        Self {
            base,
            registers: Arc::new(Mutex::new(HashMap::new())),
            behaviour: Arc::new(Mutex::new(Behaviour::Normal)),
            reads: Arc::new(AtomicUsize::new(0)),
            connections: Arc::new(AtomicUsize::new(0)),
        }
    }

    pub fn set_float(&self, rel: u16, value: f32) {
        let [lo, hi] = f32_regs(value);
        let mut regs = self.registers.lock().unwrap();
        regs.insert(self.base + rel, lo);
        regs.insert(self.base + rel + 1, hi);
    }

    pub fn set_total_power(&self, watts: f32) {
        self.set_float(13, watts);
    }

    /// phase 0..3: voltage, current, active power, apparent power, power factor
    pub fn set_phase(&self, phase: u16, v: f32, a: f32, w: f32, va: f32, pf: f32) {
        let rel = 20 + 20 * phase;
        for (i, value) in [v, a, w, va, pf].into_iter().enumerate() {
            self.set_float(rel + 2 * i as u16, value);
        }
    }

    pub fn set_frequency(&self, hz: f32) {
        self.set_float(20 + 13, hz);
    }

    pub fn set_energy(&self, import_wh: f32, export_wh: f32) {
        self.set_float(162, import_wh);
        self.set_float(164, export_wh);
    }

    pub fn set_behaviour(&self, behaviour: Behaviour) {
        *self.behaviour.lock().unwrap() = behaviour;
    }

    /// Starts listening on a free port
    pub async fn spawn(self) -> SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        self.spawn_on(listener);
        addr
    }

    pub fn spawn_on(self, listener: TcpListener) {
        let shelly = self;
        tokio::spawn(async move {
            let server = Server::new(listener);
            let new_service = |_addr| Ok(Some(shelly.clone()));
            let counter = shelly.connections.clone();
            let on_connected = |stream, socket_addr| {
                counter.fetch_add(1, Ordering::SeqCst);
                async move { accept_tcp_connection(stream, socket_addr, new_service) }
            };
            let _ = server.serve(&on_connected, |_err| {}).await;
        });
    }
}

impl tokio_modbus::server::Service for FakeShelly {
    type Request = SlaveRequest<'static>;
    type Response = Response;
    type Exception = ExceptionCode;
    type Future =
        Pin<Box<dyn future::Future<Output = Result<Self::Response, Self::Exception>> + Send>>;

    fn call(&self, req: Self::Request) -> Self::Future {
        let this = self.clone();
        Box::pin(async move {
            this.reads.fetch_add(1, Ordering::SeqCst);
            let Request::ReadInputRegisters(addr, count) = req.request else {
                return Err(ExceptionCode::IllegalFunction);
            };
            if *this.behaviour.lock().unwrap() == Behaviour::Hang {
                tokio::time::sleep(Duration::from_secs(3600)).await;
            }
            let regs = this.registers.lock().unwrap();
            let refuse = *this.behaviour.lock().unwrap() == Behaviour::Refuse;
            let values: Option<Vec<u16>> = (addr..addr + count)
                .map(|a| regs.get(&a).copied())
                .collect();
            match values {
                Some(v) if !refuse => Ok(Response::ReadInputRegisters(v)),
                _ => Err(ExceptionCode::IllegalDataAddress),
            }
        })
    }
}
