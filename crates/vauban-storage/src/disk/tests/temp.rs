use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

/// Distinguishes two directories asked for in the same nanosecond by the same process.
static SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// A path under [`std::env::temp_dir`], removed with its contents on drop.
#[derive(Debug)]
pub(crate) struct TempDir(PathBuf);

impl TempDir {
    /// A unique path, not created on disk: for the tests that ask `open` to create it.
    pub(crate) fn reserved(label: &str) -> Self {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|since| since.as_nanos())
            .unwrap_or(0);
        let sequence = SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let name = format!(
            "vauban-{label}-{pid}-{nanos}-{sequence}",
            pid = std::process::id()
        );
        Self(std::env::temp_dir().join(name))
    }

    /// A unique path, created on disk and empty.
    pub(crate) fn created(label: &str) -> Self {
        let dir = Self::reserved(label);
        std::fs::create_dir_all(dir.path()).expect("create a temporary directory");
        dir
    }

    /// The path itself.
    pub(crate) fn path(&self) -> &Path {
        &self.0
    }

    /// The path of an entry inside the directory.
    pub(crate) fn child(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        // A test that failed before creating the directory leaves nothing to remove.
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
