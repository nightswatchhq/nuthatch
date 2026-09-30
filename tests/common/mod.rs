//! Shared harness for the offline end-to-end integration tests (0.4.0 hardening).
//!
//! Everything here is deterministic and network-free: the chain is a scripted [`tape::TapeSource`]
//! the test drives by hand, the fixtures are fixed, and every wait is a *bounded poll* on observable
//! state (never a fixed sleep that drives the pipeline). See `tape.rs` for the source itself.
//!
//! `#![allow(dead_code)]`: each `tests/*.rs` is a separate crate that uses only a slice of this
//! module, so helpers unused by a given binary are legitimately dead there.
#![allow(dead_code)]

pub mod tape;

/// Whether a file's Parquet footer and schema read: what "the segment binds" means, checked without an
/// engine, for a test that must know its corrupt fixture is the corruption it claims.
pub fn footer_reads(path: &std::path::Path) -> bool {
    std::fs::File::open(path).is_ok_and(|f| {
        parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder::try_new(f).is_ok()
    })
}
