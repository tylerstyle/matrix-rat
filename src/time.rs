//! The clock types of the whole crate: one `Instant`, one `SystemTime`.
//!
//! On the terminal these are `std::time`'s own, so nothing changes. The browser has no
//! such clock (`std::time::Instant::now` panics there), so `web_time`, which reads
//! `performance.now()` and `Date.now()`, stands in. Use these, never `std::time::Instant`
//! or `std::time::SystemTime`; `Duration` is the same everywhere and stays in `std::time`.

#[cfg(not(target_family = "wasm"))]
pub use std::time::{Instant, SystemTime, UNIX_EPOCH};

#[cfg(target_family = "wasm")]
pub use web_time::{Instant, SystemTime, UNIX_EPOCH};
