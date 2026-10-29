use std::path::PathBuf;

/// RAII guard that removes a temp file on drop unless `disarm`ed.
/// Used by every "write to tmp, then rename into place" path to keep
/// failed writes from leaking `.tmp-*` orphans.
pub struct TmpFileGuard {
    path: PathBuf,
    armed: bool,
}

impl TmpFileGuard {
    pub fn new(path: PathBuf) -> Self {
        Self { path, armed: true }
    }
    pub fn disarm(mut self) {
        self.armed = false;
    }
}

impl Drop for TmpFileGuard {
    fn drop(&mut self) {
        // `NotFound` is a write that failed before it created the file.
        if self.armed
            && let Err(e) = std::fs::remove_file(&self.path)
            && e.kind() != std::io::ErrorKind::NotFound
        {
            tracing::warn!(
                path = %self.path.display(),
                error = %e,
                "failed to remove temp file; it is left behind"
            );
        }
    }
}
