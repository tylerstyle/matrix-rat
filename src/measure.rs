//! Helpers for the ignored tests that measure memory and time (`measure_*`). They are
//! meant to run alone and in release, one test at a time, so that nothing else
//! allocates while they count:
//!
//! ```text
//! MATRIX_RAT_MEASURE=1 cargo test --release measure:: -- --ignored --nocapture --test-threads=1
//! ```
//!
//! What they print are sizes and times, never anything of an account.

use std::time::{Duration, Instant};

/// Bytes the allocator has handed out and not taken back, over all its arenas: glibc's
/// own count, chunk headers and rounding included, plus what it mapped for large
/// blocks. Differences between two calls are what was allocated in between, as far as
/// the process is concerned. Zero where the allocator cannot tell (not glibc).
pub fn heap_in_use() -> usize {
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    {
        // SAFETY: `mallinfo2` only reads the allocator's counters; it takes no
        // arguments and hands back a plain struct.
        let info = unsafe { libc::mallinfo2() };
        info.uordblks + info.hblkhd
    }
    #[cfg(not(all(target_os = "linux", target_env = "gnu")))]
    {
        0
    }
}

/// Whether the measurements are to run. They are ignored tests, and a run of all
/// ignored tests (the server-backed ones, with a homeserver) would take them along in
/// a debug build, where they take long and say little: they only run when asked for
/// with `MATRIX_RAT_MEASURE=1`.
pub fn wanted() -> bool {
    let wanted = std::env::var_os("MATRIX_RAT_MEASURE").is_some();
    if !wanted {
        println!("skipped: set MATRIX_RAT_MEASURE=1 to measure");
    }
    wanted
}

/// `after - before` in bytes, signed: memory can also have been given back.
pub fn grew(before: usize, after: usize) -> i64 {
    after as i64 - before as i64
}

/// Runs `f` `runs` times and gives the shortest, the median and the longest run.
pub fn time_runs(runs: usize, mut f: impl FnMut()) -> Times {
    let mut all: Vec<Duration> = (0..runs.max(1))
        .map(|_| {
            let start = Instant::now();
            f();
            start.elapsed()
        })
        .collect();
    all.sort();
    Times(all)
}

/// The durations of several runs, shortest first.
pub struct Times(pub Vec<Duration>);

impl Times {
    pub fn min(&self) -> Duration {
        self.0[0]
    }

    pub fn median(&self) -> Duration {
        self.0[self.0.len() / 2]
    }

    pub fn max(&self) -> Duration {
        self.0[self.0.len() - 1]
    }
}

impl std::fmt::Display for Times {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let ms = |d: Duration| d.as_secs_f64() * 1000.0;
        write!(
            f,
            "min {:.3} ms, median {:.3} ms, max {:.3} ms ({} runs)",
            ms(self.min()),
            ms(self.median()),
            ms(self.max()),
            self.0.len()
        )
    }
}

/// Bytes in MB (10^6), for printing.
pub fn mb(bytes: f64) -> f64 {
    bytes / 1_000_000.0
}

mod tests {
    #[test]
    fn times_are_sorted_and_summarised() {
        let mut n = 0u64;
        let times = super::time_runs(5, || n += 1);
        assert_eq!(n, 5);
        assert_eq!(times.0.len(), 5);
        assert!(times.min() <= times.median() && times.median() <= times.max());
        assert!(times.to_string().contains("(5 runs)"));
        assert_eq!(super::grew(10, 4), -6);
    }

    /// The allocator's count follows what is allocated (where it can be read). Ignored
    /// with the measurements: other tests running at the same time move the count.
    #[test]
    #[ignore = "a measurement: run alone, see the module's documentation"]
    fn measure_heap_count_sees_a_large_block() {
        let before = super::heap_in_use();
        let block = vec![1u8; 4 << 20];
        let after = super::heap_in_use();
        std::hint::black_box(&block);
        if cfg!(all(target_os = "linux", target_env = "gnu")) {
            assert!(super::grew(before, after) >= 4 << 20, "{before} -> {after}");
        }
    }
}
