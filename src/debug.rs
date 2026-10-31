//! Tiny logging helpers.
//!
//! `FRONIUS_DEBUG=true` turns on very verbose logging of every Shelly read, every value published to the emulated
//! meter and every request the inverter makes. Lines are prefixed with the seconds since start-up so that timing
//! problems (slow Shelly reads, stale data, irregular inverter polling) are visible without extra tooling.
//! Warnings are always printed, debug lines only when the flag is set.

use std::sync::{Mutex, OnceLock};
use std::time::Instant;

static START: OnceLock<Instant> = OnceLock::new();
static ENABLED: OnceLock<bool> = OnceLock::new();

/// Call once at start-up (before any other thread logs) so the uptime clock starts at zero.
pub fn init() {
    START.get_or_init(Instant::now);
    ENABLED.get_or_init(|| env_flag("FRONIUS_DEBUG"));
}

pub fn enabled() -> bool {
    *ENABLED.get_or_init(|| env_flag("FRONIUS_DEBUG"))
}

pub fn uptime_secs() -> f64 {
    START.get_or_init(Instant::now).elapsed().as_secs_f64()
}

static LAST_ERROR: Mutex<Option<(Instant, String)>> = Mutex::new(None);

/// Remember the most recent problem reading the Shelly, so that the exit message of the watchdog can name it
pub fn record_error(message: impl Into<String>) {
    if let Ok(mut last) = LAST_ERROR.lock() {
        *last = Some((Instant::now(), message.into()));
    }
}

pub fn last_error_description() -> String {
    match LAST_ERROR.lock().ok().and_then(|last| last.clone()) {
        Some((when, message)) => format!("{message} ({}s ago)", when.elapsed().as_secs()),
        None => "none, the data fetcher did not report any error (is it stuck?)".to_string(),
    }
}

/// Accepts 1/true/yes/on (any case)
pub fn env_flag(name: &str) -> bool {
    std::env::var(name).map(|v| is_truthy(&v)).unwrap_or(false)
}

fn is_truthy(value: &str) -> bool {
    matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "1" | "true" | "yes" | "on"
    )
}

/// Printed only when `FRONIUS_DEBUG` is set
macro_rules! debug_log {
    ($($arg:tt)*) => {
        if $crate::debug::enabled() {
            println!("[{:9.3}s DEBUG] {}", $crate::debug::uptime_secs(), format_args!($($arg)*));
        }
    };
}

/// Always printed
macro_rules! warn_log {
    ($($arg:tt)*) => {
        eprintln!("[{:9.3}s WARN ] {}", $crate::debug::uptime_secs(), format_args!($($arg)*))
    };
}

pub(crate) use debug_log;
pub(crate) use warn_log;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truthy_values() {
        for v in ["1", "true", "TRUE", " yes ", "On"] {
            assert!(is_truthy(v), "{v}");
        }
        for v in ["", "0", "false", "no", "off", "debug"] {
            assert!(!is_truthy(v), "{v}");
        }
    }
}
