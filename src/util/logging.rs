use once_cell::sync::OnceCell;
use std::time::Duration;

static RUN_ID: OnceCell<String> = OnceCell::new();
const DIAGNOSTICS_INTERVAL_ENV: &str = "KRONID_INTERNAL_LEN_LOG_SECS";
const DEFAULT_DIAGNOSTICS_INTERVAL_SECS: u64 = 60 * 60;

pub fn set_run_id(run_id: impl Into<String>) {
    let _ = RUN_ID.set(run_id.into());
}

pub fn run_id() -> Option<&'static str> {
    RUN_ID.get().map(String::as_str)
}

pub fn diagnostics_interval() -> Duration {
    let secs = std::env::var(DIAGNOSTICS_INTERVAL_ENV)
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|secs| *secs > 0)
        .unwrap_or(DEFAULT_DIAGNOSTICS_INTERVAL_SECS);
    Duration::from_secs(secs)
}

#[macro_export]
macro_rules! log_with_run_id {
    ($level:expr, $($arg:tt)+) => {{
        if log::log_enabled!($level) {
            match $crate::util::logging::run_id() {
                Some(id) => log::log!($level, "[{}] {}", id, format_args!($($arg)+)),
                None => log::log!($level, "[-] {}", format_args!($($arg)+)),
            }
        }
    }};
}

#[macro_export]
macro_rules! error {
    ($($arg:tt)+) => {
        $crate::log_with_run_id!(log::Level::Error, $($arg)+)
    };
}

#[macro_export]
macro_rules! warn {
    ($($arg:tt)+) => {
        $crate::log_with_run_id!(log::Level::Warn, $($arg)+)
    };
}

#[macro_export]
macro_rules! info {
    ($($arg:tt)+) => {
        $crate::log_with_run_id!(log::Level::Info, $($arg)+)
    };
}

#[macro_export]
macro_rules! debug {
    ($($arg:tt)+) => {
        $crate::log_with_run_id!(log::Level::Debug, $($arg)+)
    };
}

#[macro_export]
macro_rules! trace {
    ($($arg:tt)+) => {
        $crate::log_with_run_id!(log::Level::Trace, $($arg)+)
    };
}

pub use crate::{debug, error, info, trace, warn};
