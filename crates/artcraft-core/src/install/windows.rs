//! Windows install backend (work in progress).
//!
//! The full installer/updater for Windows Craft Apps is not implemented yet.
//! These stubs keep the crate compiling on Windows so the GUI and CLI can be
//! built and shipped as a standalone binary; install/uninstall operations
//! return a clear "not yet implemented" error until the backend lands.

use std::path::{Path, PathBuf};

use crate::catalog::AppDef;
use crate::config::Config;
use crate::error::{Error, Result};
use crate::github::ReleasePlan;
use crate::paths::Paths;
use crate::progress::{self, ProgressFn};
use crate::state::InstalledApp;

pub fn install_archive(
    app: &AppDef,
    _plan: &ReleasePlan,
    _archive: &Path,
    paths: &Paths,
    _cfg: &Config,
    progress: ProgressFn<'_>,
) -> Result<InstalledApp> {
    progress::message(progress, format!("Installing {} on Windows is not implemented yet.", app.name));
    Err(Error::Install(format!(
        "{} cannot be installed on Windows yet (backend pending).",
        app.name
    )))
}

pub fn activate_existing(
    app: &AppDef,
    _version_dir: &Path,
    _version: &str,
    _paths: &Paths,
    progress: ProgressFn<'_>,
) -> Result<InstalledApp> {
    progress::message(progress, format!("Activating {} on Windows is not implemented yet.", app.name));
    Err(Error::Install(format!(
        "{} cannot be activated on Windows yet (backend pending).",
        app.name
    )))
}

pub fn find_main_binary(_bin_dir: &Path, _app: &AppDef) -> Option<String> {
    None
}

pub fn uninstall(_app: &AppDef, _paths: &Paths) -> Result<bool> {
    Err(Error::Install("Uninstall is not implemented on Windows yet.".into()))
}

// keep PathBuf import used
#[allow(dead_code)]
fn _ensure_paths(paths: &Paths) -> PathBuf {
    paths.install_root.clone()
}
