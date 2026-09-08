//! `init` and what runs beside it. This step holds one helper: the removal
//! of the state file releases before 0.14 kept the tab list in. The
//! bootstrap, the flags and the user commands arrive with step 6.

use std::io;
use std::path::{Path, PathBuf};

/// Remove `state.json` under `dir` when it is there: the path removed, or
/// `None` when there was nothing to remove. Only a failure other than the
/// file's absence is an error.
pub fn remove_legacy_state_file(dir: &Path) -> io::Result<Option<PathBuf>> {
    let path = dir.join("state.json");
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(Some(path)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}
