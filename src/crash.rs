//! Named points where a test build dies on request (#1717).
//!
//! A build with `--features crash-points` aborts at the point `NUTHATCH_CRASH_AT` names, which is a
//! kill -9 at a known instant: no destructor runs and nothing is flushed that was not already. Every
//! other build compiles each point to nothing.

/// Abort the process if `NUTHATCH_CRASH_AT` is `name`.
#[cfg(feature = "crash-points")]
pub fn point(name: &str) {
    if std::env::var("NUTHATCH_CRASH_AT").is_ok_and(|want| want == name) {
        eprintln!("crash point {name}: aborting");
        std::process::abort();
    }
}

#[cfg(not(feature = "crash-points"))]
#[inline(always)]
pub fn point(_name: &str) {}
