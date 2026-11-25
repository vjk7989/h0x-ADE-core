//! A scratch HOME for finder tests: never the user's own.

use std::path::{Path, PathBuf};
use std::time::{Duration, UNIX_EPOCH};

/// A temporary folder standing in for HOME; removed afterwards.
pub(super) struct Scratch(PathBuf);

impl Scratch {
    pub(super) fn new() -> Scratch {
        let root = std::env::temp_dir().join(format!("h0x-core-finder-{}", crate::platform::random_hex()));
        std::fs::create_dir_all(&root).unwrap();
        Scratch(root)
    }

    pub(super) fn path(&self) -> &Path {
        &self.0
    }

    /// Writes a file under the scratch folder, last modified at `ms`.
    pub(super) fn put(&self, rel: &str, bytes: &[u8], ms: u64) -> PathBuf {
        let path = self.0.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, bytes).unwrap();
        let file = std::fs::File::options().write(true).open(&path).unwrap();
        file.set_modified(UNIX_EPOCH + Duration::from_millis(ms)).unwrap();
        path
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
