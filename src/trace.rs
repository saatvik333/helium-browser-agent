//! Opt-in timing trace for the daemon log: set `HBA_TRACE=1` when starting the daemon.

use std::sync::LazyLock;
use std::time::Instant;

pub static ENABLED: LazyLock<bool> = LazyLock::new(|| std::env::var_os("HBA_TRACE").is_some_and(|v| v != "0"));

pub static START: LazyLock<Instant> = LazyLock::new(Instant::now);

/// Writes `format!(...)` with milliseconds since daemon start to stderr (the daemon log).
#[macro_export]
macro_rules! trace {
    ($($arg:tt)*) => {
        if *$crate::trace::ENABLED {
            eprintln!("{:>9.1} {}", $crate::trace::START.elapsed().as_secs_f64() * 1000.0, format_args!($($arg)*));
        }
    };
}
