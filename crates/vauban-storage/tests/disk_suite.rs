//! The generic contract suite of the `Storage` trait, run on `DiskStorage`.
//!
//! Each scenario gets a fresh instance in a unique temporary directory under
//! [`std::env::temp_dir`], created by the closure the [`storage_contract_suite`] macro
//! evaluates once per test. The [`DiskStorage`] behind the returned box is dropped when the
//! scenario ends, which releases the instance guard; the directory itself is left on disk so
//! a failing run can be inspected, and the operating system sweeps `temp_dir()` between
//! runs.
//!
//! The `testsuite` feature of this crate enables both `memory_suite` and this one.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use vauban_storage::{DiskOptions, DiskStorage, storage_contract_suite};

/// Distinguishes two instances asked for in the same nanosecond by the same process.
static SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// The next unique path under `temp_dir()`, created on disk and empty.
fn fresh_dir() -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|since| since.as_nanos())
        .unwrap_or(0);
    let sequence = SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!(
        "vauban-disk-suite-{pid}-{nanos}-{sequence}",
        pid = std::process::id()
    ));
    std::fs::create_dir_all(&path).expect("create a temporary directory for the disk suite");
    path
}

storage_contract_suite!(
    DiskStorage::open(&fresh_dir(), DiskOptions::default())
        .expect("open an instance for the disk suite")
);
