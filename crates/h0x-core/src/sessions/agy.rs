//! agy: no session finder yet; the flag lists are filled from its `--help`.

use super::*;

pub(super) const BOOL_FLAGS: &[&str] = &[];
pub(super) const VALUE_FLAGS: &[&str] = &[];
pub(super) const FLAGS_BEFORE_RESUME: bool = true;

pub(super) fn root(_: Env) -> Option<PathBuf> {
    None
}

pub(super) fn list(_: &Path, _: &str) -> Vec<SessionInfo> {
    Vec::new()
}
