use once_cell::sync::OnceCell;

static RUN_ID: OnceCell<String> = OnceCell::new();

pub fn set_run_id(run_id: impl Into<String>) {
    let _ = RUN_ID.set(run_id.into());
}

pub fn run_id() -> Option<&'static str> {
    RUN_ID.get().map(String::as_str)
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
