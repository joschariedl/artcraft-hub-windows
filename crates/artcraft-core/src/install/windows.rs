//! Windows install backend.
//!
//! Installs a portable `*-windows-*-portable.zip` release into
//! `%LOCALAPPDATA%\ArtCraft\<id>\<version>`, creates a Start Menu shortcut and
//! records the install in the shared state file. Extraction and shortcut
//! creation are delegated to PowerShell, which is present on every Windows 11
//! machine, so the hub itself stays a single self-contained executable.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::catalog::AppDef;
use crate::config::Config;
use crate::error::{Error, Result};
use crate::github::ReleasePlan;
use crate::paths::Paths;
use crate::progress::{self, ProgressFn};
use crate::state::{self, InstalledApp};
use crate::time::now_unix;

pub fn install_archive(
    app: &AppDef,
    plan: &ReleasePlan,
    archive: &Path,
    paths: &Paths,
    _cfg: &Config,
    progress: ProgressFn<'_>,
) -> Result<InstalledApp> {
    paths.ensure()?;

    let previous = state::scan_installed(paths).remove(app.id);
    let staging = state::staging_dir(paths, app.id);
    if staging.exists() {
        let _ = fs::remove_dir_all(&staging);
    }
    let content = staging.join("content");
    fs::create_dir_all(&content)?;

    let result = (|| -> Result<InstalledApp> {
        progress::message(progress, format!("Extracting {} …", plan.asset_name));
        extract_archive(archive, &content)?;

        let bin_dir = find_bin_dir(&content).ok_or_else(|| {
            Error::Install("Archive contains no bin/ directory".into())
        })?;
        find_main_binary(&bin_dir, app).ok_or_else(|| {
            Error::Install(format!("no executable for {} found in bin/", app.name))
        })?;

        let version_dir = paths.app_version_dir(app.id, &plan.version);
        if let Some(parent) = version_dir.parent() {
            fs::create_dir_all(parent)?;
        }
        let backup = paths.app_dir(app.id).join(format!(".backup-{}", std::process::id()));
        if backup.exists() {
            let _ = fs::remove_dir_all(&backup);
        }
        let had_target = version_dir.exists();
        if had_target {
            fs::rename(&version_dir, &backup)?;
        }
        if let Err(error) = fs::rename(&content, &version_dir) {
            if had_target {
                let _ = fs::rename(&backup, &version_dir);
            }
            return Err(error.into());
        }

        let activated = (|| -> Result<InstalledApp> {
            let mut record = activate_version(app, &version_dir, &plan.version, paths, progress)?;
            record.tag = plan.tag.clone();
            record.sha256 = plan.sha256.clone();
            Ok(record)
        })();

        match activated {
            Ok(record) => {
                let _ = fs::remove_dir_all(&backup);
                // Keep only the newest few versions.
                prune_old_versions(app, paths, &version_dir);
                Ok(record)
            }
            Err(error) => {
                let _ = fs::remove_dir_all(&version_dir);
                if had_target {
                    let _ = fs::rename(&backup, &version_dir);
                }
                if let Some(previous) = previous.as_ref() {
                    if previous.dir != version_dir {
                        let _ = activate_existing(app, &previous.dir, &previous.version, paths, progress);
                    }
                }
                Err(error)
            }
        }
    })();

    let _ = fs::remove_dir_all(&staging);
    result
}

pub fn activate_existing(
    app: &AppDef,
    version_dir: &Path,
    version: &str,
    paths: &Paths,
    progress: ProgressFn<'_>,
) -> Result<InstalledApp> {
    if !version_dir.is_dir() {
        return Err(Error::Install(format!(
            "{} {} is not present on disk",
            app.name, version
        )));
    }
    let record = activate_version(app, version_dir, version, paths, progress)?;
    Ok(record)
}

pub fn find_main_binary(bin_dir: &Path, app: &AppDef) -> Option<String> {
    let mut names: Vec<String> = Vec::new();
    if let Ok(entries) = fs::read_dir(bin_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_file() {
                continue;
            }
            if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                if name.to_ascii_lowercase().ends_with(".exe") {
                    names.push(name.to_string());
                }
            }
        }
    }
    let mut stems: Vec<&str> = vec![app.id];
    stems.extend(app.stems.iter().copied());
    for stem in &stems {
        let wanted = format!("{stem}.exe");
        if names.iter().any(|n| n.eq_ignore_ascii_case(&wanted)) {
            return Some(wanted);
        }
    }
    names.into_iter().find(|n| {
        let lower = n.to_ascii_lowercase();
        !lower.contains("-cli") && !lower.contains("web")
    })
}

fn find_bin_dir(root: &Path) -> Option<PathBuf> {
    if root.join("bin").is_dir() {
        return Some(root.join("bin"));
    }
    if let Ok(entries) = fs::read_dir(root) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() && path.join("bin").is_dir() {
                return Some(path.join("bin"));
            }
        }
    }
    None
}

fn extract_archive(archive: &Path, dest: &Path) -> Result<()> {
    let script = format!(
        "Expand-Archive -LiteralPath '{}' -DestinationPath '{}' -Force",
        ps_quote(&archive.display().to_string()),
        ps_quote(&dest.display().to_string())
    );
    let status = Command::new("powershell")
        .args(["-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-Command", &script])
        .status()?;
    if !status.success() {
        return Err(Error::Install("Failed to extract archive with PowerShell".into()));
    }
    Ok(())
}

fn activate_version(
    app: &AppDef,
    version_dir: &Path,
    version: &str,
    paths: &Paths,
    progress: ProgressFn<'_>,
) -> Result<InstalledApp> {
    let bin_dir = version_dir.join("bin");
    if !bin_dir.is_dir() {
        return Err(Error::Install(format!(
            "{} {} has no bin/ directory",
            app.name, version
        )));
    }
    let main_binary = find_main_binary(&bin_dir, app).ok_or_else(|| {
        Error::Install(format!("no executable for {} found in bin/", app.name))
    })?;
    let binary = bin_dir.join(&main_binary);

    progress::message(progress, "Creating Start Menu shortcut …");
    let shortcut = paths.applications_dir.join(format!("{}.lnk", app.bundle_id()));
    if let Some(parent) = shortcut.parent() {
        fs::create_dir_all(parent)?;
    }
    create_shortcut(&shortcut, &binary, &bin_dir)?;

    Ok(InstalledApp {
        version: version.to_string(),
        tag: version.to_string(),
        sha256: String::new(),
        installed_at: now_unix(),
        dir: version_dir.to_path_buf(),
        desktop_file: shortcut,
    })
}

fn create_shortcut(shortcut: &Path, target: &Path, working_dir: &Path) -> Result<()> {
    let script = format!(
        "$ws = New-Object -ComObject WScript.Shell; $sc = $ws.CreateShortcut('{}'); $sc.TargetPath = '{}'; $sc.WorkingDirectory = '{}'; $sc.Save()",
        ps_quote(&shortcut.display().to_string()),
        ps_quote(&target.display().to_string()),
        ps_quote(&working_dir.display().to_string())
    );
    let status = Command::new("powershell")
        .args(["-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-Command", &script])
        .status()?;
    if !status.success() {
        return Err(Error::Install("Failed to create Start Menu shortcut".into()));
    }
    Ok(())
}

pub fn uninstall(app: &AppDef, paths: &Paths) -> Result<bool> {
    let mut removed = false;
    let app_dir = paths.app_dir(app.id);
    if app_dir.exists() {
        let _ = fs::remove_dir_all(&app_dir);
        removed = true;
    }
    let shortcut = paths.applications_dir.join(format!("{}.lnk", app.bundle_id()));
    if shortcut.exists() {
        let _ = fs::remove_file(&shortcut);
        removed = true;
    }
    Ok(removed)
}

fn prune_old_versions(app: &AppDef, paths: &Paths, keep_dir: &Path) {
    let app_dir = paths.app_dir(app.id);
    if let Ok(entries) = fs::read_dir(&app_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path == keep_dir || !path.is_dir() {
                continue;
            }
            let _ = fs::remove_dir_all(&path);
        }
    }
}

fn ps_quote(s: &str) -> String {
    s.replace('\'', "''")
}
