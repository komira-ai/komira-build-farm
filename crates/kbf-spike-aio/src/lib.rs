//! Throwaway spike harness: stub gRPC servers and Envoy bootstrap files. Not merged.

pub mod envoy;
pub mod stub;

use std::sync::OnceLock;
use std::time::Instant;

static START: OnceLock<Instant> = OnceLock::new();

/// Seconds since the first call.
pub fn now() -> f64 {
    START.get_or_init(Instant::now).elapsed().as_secs_f64()
}

/// One evidence line: `[t=  1.234s] label: text`.
#[macro_export]
macro_rules! ev {
    ($label:expr, $($arg:tt)*) => {
        println!("[t={:7.3}s] {}: {}", $crate::now(), $label, format!($($arg)*))
    };
}
