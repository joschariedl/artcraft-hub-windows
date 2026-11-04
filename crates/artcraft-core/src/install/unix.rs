//! Installing a verified tarball into the user's home directory.
//!
//! Layout:
//!
//! ```text
//! ~/.local/opt/artcraft/<id>/<version>/      app files
//! ~/.local/bin/<binary>                      symlinks
//! ~/.local/share/applications/<bundle>.desktop
//! ~/.local/share/icons/hicolor/<size>/apps/<bundle>.png
//! ```
//!
//! Nothing needs root and nothing outside these directories is touched.

use std::fs;
use std::io::Read;
use std::os::unix::fs::{symlink, PermissionsExt};
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};

use crate::catalog::AppDef;
use crate::config::Config;
use crate::download;
use crate::error::{Error, Result};
use crate::github::{self, ReleasePlan};
use crate::paths::Paths;
use crate::progress::{self, ProgressFn};
use crate::state::{self, InstalledApp};

const MAX_DESKTOP: u64 = 256 * 1024;

/// Extracts `archive` into `dest`, stripping the archive's single top-level
/// directory. Rejects absolute paths, `..`, symlinks, hardlinks and devices.
pub fn extract_archive(archive: &Path, dest: &Path) -> Result<()> {
    let file = fs::File::open(archive)?;
    let decoder = flate2::read::GzDecoder::new(file);
    let mut tar = tar::Archive::new(decoder);
    tar.set_preserve_permissions(true);
    tar.set_overwrite(true);

    let mut top: Option<String> = None;
    let mut entries = 0usize;

    for entry in tar.entries()? {
        let mut entry = entry?;
        let path = entry.path()?.to_path_buf();
        let rel = checked_relative(&path, &mut top)?;
        entries += 1;
        if entries > 20_000 {
            return Err(Error::UnsafeArchive(
                "Archive contains more than 20,000 entries".into(),
            ));
        }
        if rel.as_os_str().is_empty() {
            continue;
        }
        let target = dest.join(&rel);
        if !target.starts_with(dest) {
            return Err(Error::UnsafeArchive(format!(
                "Path escapes the target directory: {}",
                path.display()
            )));
        }
        match entry.header().entry_type() {
            tar::EntryType::Directory => {
                fs::create_dir_all(&target)?;
            }
            tar::EntryType::Regular | tar::EntryType::Continuous => {
                if let Some(parent) = target.parent() {
                    fs::create_dir_all(parent)?;
                }
                entry.unpack(&target)?;
            }
            other => {
                return Err(Error::UnsafeArchive(format!(
                    "disallowed entry type {:?} for {}",
                    other,
                    path.display()
                )));
            }
        }
    }

    if entries == 0 {
        return Err(Error::UnsafeArchive("Archive is empty".into()));
    }
    Ok(())
}

/// Validates one archive path and returns it relative to the single top-level
/// directory. The first call sets `top`.
pub fn checked_relative(path: &Path, top: &mut Option<String>) -> Result<PathBuf> {
    if path.as_os_str().is_empty() {
        return Err(Error::UnsafeArchive("empty path in archive".into()));
    }
    let text = path.to_string_lossy();
    if text.chars().any(|c| c.is_control() || c == '\\') {
        return Err(Error::UnsafeArchive(format!(
            "disallowed characters in path: {text:?}"
        )));
    }

    let mut normal: Vec<&str> = Vec::new();
    for component in path.components() {
        match component {
            Component::Normal(name) => {
                let name = name.to_str().ok_or_else(|| {
                    Error::UnsafeArchive("Path is not valid UTF-8".into())
                })?;
                normal.push(name);
            }
            Component::CurDir
            | Component::ParentDir
            | Component::RootDir
            | Component::Prefix(_) => {
                return Err(Error::UnsafeArchive(format!(
                    "unsafe path in archive: {text}"
                )));
            }
        }
    }

    let Some((first, rest)) = normal.split_first() else {
        return Err(Error::UnsafeArchive("empty path in archive".into()));
    };
    match top {
        Some(expected) if expected != first => {
            return Err(Error::UnsafeArchive(format!(
                "Archive contains multiple root directories ({expected}, {first})"
            )));
        }
        None => *top = Some((*first).to_string()),
        _ => {}
    }
    Ok(rest.iter().collect())
}

/// Installs `plan` from a verified archive. The archive must already have the
/// expected SHA-256; it is checked again by [`download::fetch`] before this.
///
/// The install is transactional: an existing target directory is moved aside,
/// the new tree is activated, and any failure restores the previous version so
/// a half-written release never becomes the active one.
pub fn install_archive(
    app: &AppDef,
    plan: &ReleasePlan,
    archive: &Path,
    paths: &Paths,
    cfg: &Config,
    progress: ProgressFn<'_>,
) -> Result<InstalledApp> {
    paths.ensure()?;

    let previous = state::scan_installed(paths).remove(app.id);
    let staging = state::staging_dir(paths, app.id);
    if staging.exists() {
        fs::remove_dir_all(&staging)?;
    }
    let content = staging.join("content");
    fs::create_dir_all(&content)?;

    let result = (|| -> Result<InstalledApp> {
        progress::message(progress, format!("Extracting {} …", plan.asset_name));
        extract_archive(archive, &content)?;

        let bin_dir = content.join("bin");
        if !bin_dir.is_dir() {
            return Err(Error::Install(
                "Archive contains no bin/ directory".into(),
            ));
        }
        find_main_binary(&bin_dir, app).ok_or_else(|| {
            Error::Install(format!(
                "no executable for {} found in bin/",
                app.name
            ))
        })?;

        let version_dir = paths.app_version_dir(app.id, &plan.version);
        if let Some(parent) = version_dir.parent() {
            fs::create_dir_all(parent)?;
        }
        let backup = paths
            .app_dir(app.id)
            .join(format!(".backup-{}", std::process::id()));
        if backup.exists() {
            fs::remove_dir_all(&backup)?;
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
            if cfg.install_1024_icons {
                if let Err(error) = install_highres_icon(app, &plan.tag, paths, progress) {
                    progress::message(
                        progress,
                        format!("Note: 1024 px icon not loaded ({error})"),
                    );
                }
            }
            Ok(record)
        })();

        match activated {
            Ok(record) => {
                let _ = fs::remove_dir_all(&backup);
                prune_old_versions(app, paths, &version_dir, cfg.keeps_versions());
                refresh_desktop_databases(paths);
                Ok(record)
            }
            Err(error) => {
                // Undo the activation and put the old tree back.
                let _ = fs::remove_dir_all(&version_dir);
                if had_target {
                    let _ = fs::rename(&backup, &version_dir);
                }
                if let Some(previous) = previous.as_ref() {
                    if previous.dir != version_dir {
                        let _ = activate_existing(
                            app,
                            &previous.dir,
                            &previous.version,
                            paths,
                            progress,
                        );
                    }
                }
                Err(error)
            }
        }
    })();

    let _ = fs::remove_dir_all(&staging);
    result
}

/// Activates an already-extracted version: desktop file, binary links, icons
/// and shared metadata. Used by installs and by rollback.
pub fn activate_version(
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
    make_executable(&binary)?;

    progress::message(progress, "Creating .desktop file …");
    write_desktop_file_for(app, version, version_dir, &binary, paths)?;

    progress::message(progress, "Linking programs …");
    link_binaries(app, version_dir, paths, progress);

    progress::message(progress, "Installing icons …");
    copy_icons(app, version_dir, paths)?;

    copy_extras(app, version_dir, paths);

    Ok(InstalledApp {
        version: version.to_string(),
        tag: version.to_string(),
        sha256: String::new(),
        installed_at: crate::time::now_unix(),
        dir: version_dir.to_path_buf(),
        desktop_file: paths.desktop_file(&app.bundle_id()),
    })
}

/// Switches the active version to a directory that is already on disk.
pub fn activate_existing(
    app: &AppDef,
    version_dir: &Path,
    version: &str,
    paths: &Paths,
    progress: ProgressFn<'_>,
) -> Result<InstalledApp> {
    if !paths.is_managed_path(version_dir) || !version_dir.is_dir() {
        return Err(Error::Install(format!(
            "{} {} is not present on disk",
            app.name, version
        )));
    }
    let record = activate_version(app, version_dir, version, paths, progress)?;
    refresh_desktop_databases(paths);
    Ok(record)
}

/// Finds the GUI binary: `<id>`, a legacy stem, or any non-CLI executable.
pub fn find_main_binary(bin_dir: &Path, app: &AppDef) -> Option<String> {
    let mut names: Vec<String> = Vec::new();
    let entries = fs::read_dir(bin_dir).ok()?;
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        names.push(name.to_string());
    }
    let mut stems: Vec<&str> = vec![app.id];
    stems.extend(app.stems.iter().copied());
    for stem in &stems {
        if names.iter().any(|n| n == stem) {
            return Some((*stem).to_string());
        }
    }
    names
        .into_iter()
        .find(|name| !name.contains("cli") && !name.contains("web"))
}

fn make_executable(path: &Path) -> Result<()> {
    let mut perms = fs::metadata(path)?.permissions();
    perms.set_mode(perms.mode() | 0o755);
    fs::set_permissions(path, perms)?;
    Ok(())
}

/// Writes `~/.local/share/applications/<bundle>.desktop`, rewriting `Exec` and
/// `TryExec` to absolute paths.
pub fn write_desktop_file(
    app: &AppDef,
    plan: &ReleasePlan,
    version_dir: &Path,
    binary: &Path,
    paths: &Paths,
) -> Result<PathBuf> {
    write_desktop_file_for(app, &plan.version, version_dir, binary, paths)
}

pub fn write_desktop_file_for(
    app: &AppDef,
    version: &str,
    version_dir: &Path,
    binary: &Path,
    paths: &Paths,
) -> Result<PathBuf> {
    let bundle = app.bundle_id();
    let source = find_source_desktop(version_dir, &bundle);
    let text = build_desktop_file(app, version, binary, &bundle, source.as_deref());
    let target = paths.desktop_file(&bundle);
    state::write_atomic(&target, text.as_bytes(), 0o644)?;
    Ok(target)
}

fn find_source_desktop(version_dir: &Path, bundle: &str) -> Option<String> {
    let dir = version_dir.join("share/applications");
    let preferred = dir.join(format!("{bundle}.desktop"));
    let path = if preferred.is_file() {
        preferred
    } else {
        fs::read_dir(&dir)
            .ok()?
            .flatten()
            .map(|e| e.path())
            .find(|p| p.extension().and_then(|e| e.to_str()) == Some("desktop"))?
    };
    let meta = fs::metadata(&path).ok()?;
    if meta.len() > MAX_DESKTOP {
        return None;
    }
    fs::read_to_string(path).ok()
}

/// Pure desktop-file builder, kept separate for tests.
pub fn build_desktop_file(
    app: &AppDef,
    version: &str,
    binary: &Path,
    bundle: &str,
    source: Option<&str>,
) -> String {
    let exec = quote_exec(binary);
    let mut out = String::new();
    let mut has_type = false;
    let mut has_terminal = false;
    let mut has_icon = false;
    let mut has_startup_class = false;

    for line in source.unwrap_or_default().lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("X-ArtCraft-") {
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix("Exec=") {
            let args: Vec<&str> = rest.split_whitespace().skip(1).collect();
            if args.is_empty() {
                out.push_str(&format!("Exec={exec}\n"));
            } else {
                out.push_str(&format!("Exec={exec} {}\n", args.join(" ")));
            }
            continue;
        }
        if trimmed.starts_with("TryExec=") {
            // TryExec is a path, not a shell command: it must not be quoted,
            // otherwise GLib looks for a file whose name literally contains the
            // quotes and treats the entry as unavailable.
            out.push_str(&format!("TryExec={}\n", binary.display()));
            continue;
        }
        if trimmed.starts_with("Icon=") {
            out.push_str(&format!("Icon={bundle}\n"));
            has_icon = true;
            continue;
        }
        if trimmed.starts_with("StartupWMClass=") {
            out.push_str(&format!("StartupWMClass={bundle}\n"));
            has_startup_class = true;
            continue;
        }
        if trimmed.starts_with("Type=") {
            has_type = true;
        }
        if trimmed.starts_with("Terminal=") {
            has_terminal = true;
        }
        out.push_str(line.trim_end());
        out.push('\n');
    }

    if out.trim().is_empty() {
        out.push_str("[Desktop Entry]\n");
    }
    if !out.lines().any(|l| l.trim() == "[Desktop Entry]") {
        out.insert_str(0, "[Desktop Entry]\n");
    }
    if !has_type {
        out.push_str("Type=Application\n");
    }
    if !out.lines().any(|l| l.trim().starts_with("Name=")) {
        out.push_str(&format!("Name={}\n", app.name));
    }
    if !out.lines().any(|l| l.trim().starts_with("Comment=")) {
        out.push_str(&format!("Comment={}\n", app.tagline));
    }
    if !has_icon {
        out.push_str(&format!("Icon={bundle}\n"));
    }
    if !has_terminal {
        out.push_str("Terminal=false\n");
    }
    if !out
        .lines()
        .any(|l| l.trim().starts_with("Categories="))
    {
        out.push_str("Categories=Graphics;\n");
    }
    if !has_startup_class {
        out.push_str(&format!("StartupWMClass={bundle}\n"));
    }
    if !out.contains(&format!("Exec={exec}")) {
        out.push_str(&format!("Exec={exec}\n"));
    }
    out.push_str(&format!("X-ArtCraft-Version={version}\n"));
    out.push_str("X-ArtCraft-Managed=true\n");
    if !out.ends_with('\n') {
        out.push('\n');
    }
    out
}

/// Quotes a path for a desktop `Exec=` key.
pub fn quote_exec(path: &Path) -> String {
    let text = path.to_string_lossy();
    let escaped = text
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('`', "\\`")
        .replace('$', "\\$")
        .replace('%', "%%");
    format!("\"{escaped}\"")
}

fn link_binaries(app: &AppDef, version_dir: &Path, paths: &Paths, progress: ProgressFn<'_>) {
    let dir = version_dir.join("bin");
    let Ok(entries) = fs::read_dir(&dir) else {
        return;
    };
    let mut stems: Vec<&str> = vec![app.id];
    stems.extend(app.stems.iter().copied());

    for entry in entries.flatten() {
        let source = entry.path();
        if !source.is_file() {
            continue;
        }
        let Some(name) = source.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        let belongs = stems.iter().any(|stem| {
            name == *stem || name.starts_with(&format!("{stem}-")) || name.ends_with("-cli")
        });
        if !belongs {
            continue;
        }
        let target = paths.bin_dir.join(name);
        if let Ok(meta) = fs::symlink_metadata(&target) {
            if meta.file_type().is_symlink() {
                if let Ok(existing) = fs::read_link(&target) {
                    if paths.is_managed_path(&existing) {
                        let _ = fs::remove_file(&target);
                    } else {
                        progress::message(
                            progress,
                            format!("Note: {name} already exists and is left unchanged"),
                        );
                        continue;
                    }
                }
            } else {
                progress::message(
                    progress,
                    format!("Note: {name} already exists and is left unchanged"),
                );
                continue;
            }
        }
        if let Err(error) = symlink(&source, &target) {
            progress::message(progress, format!("Note: {name} not linked ({error})"));
        }
    }
}

fn copy_icons(app: &AppDef, version_dir: &Path, paths: &Paths) -> Result<()> {
    let bundle = app.bundle_id();
    let source = version_dir.join("share/icons/hicolor");
    let Ok(sizes) = fs::read_dir(&source) else {
        return Ok(());
    };
    for size in sizes.flatten() {
        let size_path = size.path();
        if !size_path.is_dir() {
            continue;
        }
        let Some(size_name) = size_path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        let apps = size_path.join("apps");
        let Ok(files) = fs::read_dir(&apps) else {
            continue;
        };
        for file in files.flatten() {
            let path = file.path();
            let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            let ext = path.extension().and_then(|e| e.to_str()).unwrap_or_default();
            if !name.starts_with(&bundle) {
                continue;
            }
            // SVGs are only copied when they are small, clean vectors. The
            // upstream repos ship 300 KB - 1 MB traced illustrations that
            // render poorly in menus; those are skipped in favour of the PNGs.
            if ext == "svg" {
                let Ok(meta) = fs::metadata(&path) else { continue; };
                if meta.len() > 50 * 1024 {
                    continue;
                }
            } else if ext != "png" {
                continue;
            }
            let target_dir = paths.icons_dir.join("hicolor").join(size_name).join("apps");
            fs::create_dir_all(&target_dir)?;
            let target = target_dir.join(name);
            fs::copy(&path, &target)?;
            fs::set_permissions(&target, fs::Permissions::from_mode(0o644))?;
        }
    }
    Ok(())
}

fn copy_extras(app: &AppDef, version_dir: &Path, paths: &Paths) {
    let bundle = app.bundle_id();
    let pairs = [
        (
            version_dir.join("share/mime/packages"),
            paths.mime_dir.join("packages"),
        ),
        (version_dir.join("share/metainfo"), paths.metainfo_dir.clone()),
    ];
    for (src, dst) in pairs {
        let Ok(entries) = fs::read_dir(&src) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            if !path.is_file() || !(name.contains(&bundle) || name.contains(app.id)) {
                continue;
            }
            if fs::create_dir_all(&dst).is_err() {
                continue;
            }
            let _ = fs::copy(&path, dst.join(name));
        }
    }
}

/// Downloads the 1024 px PNG from the app's repository when the tarball only
/// ships up to 512 px.
fn install_highres_icon(
    app: &AppDef,
    tag: &str,
    paths: &Paths,
    progress: ProgressFn<'_>,
) -> Result<()> {
    let bundle = app.bundle_id();
    let target_dir = paths
        .icons_dir
        .join("hicolor/1024x1024/apps");
    fs::create_dir_all(&target_dir)?;
    let target = target_dir.join(format!("{bundle}.png"));
    if target.is_file() {
        return Ok(());
    }

    let agent = crate::net::agent(std::time::Duration::from_secs(30));
    for url in github::icon_urls(app, tag) {
        match download::fetch_bytes(&agent, &url, 4 * 1024 * 1024) {
            Ok(bytes) if bytes.starts_with(b"\x89PNG\r\n\x1a\n") => {
                fs::write(&target, &bytes)?;
                fs::set_permissions(&target, fs::Permissions::from_mode(0o644))?;
                progress::message(progress, "High-resolution icon installed.");
                return Ok(());
            }
            _ => continue,
        }
    }
    Err(Error::Install("no 1024 px PNG found in the repository".into()))
}

/// Keeps the active version plus the newest `keep_count - 1` rollback targets.
/// Dot-directories (staging/backup leftovers) are always removed.
fn prune_old_versions(app: &AppDef, paths: &Paths, active: &Path, keep_count: usize) {
    let root = paths.app_dir(app.id);
    let Ok(entries) = fs::read_dir(&root) else {
        return;
    };
    let mut versions: Vec<(crate::version::ReleaseVersion, PathBuf)> = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path == *active || !path.is_dir() {
            continue;
        }
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if name.starts_with('.') {
            let _ = fs::remove_dir_all(&path);
            continue;
        }
        if paths.is_managed_path(&path) {
            versions.push((crate::version::ReleaseVersion::parse(name), path));
        }
    }
    versions.sort_by(|a, b| b.0.cmp_version(&a.0));

    let keep_others = keep_count.saturating_sub(1);
    for (index, (_, path)) in versions.into_iter().enumerate() {
        if index >= keep_others {
            let _ = fs::remove_dir_all(path);
        }
    }
}

fn refresh_desktop_databases(paths: &Paths) {
    let _ = Command::new("update-desktop-database")
        .arg("-q")
        .arg(&paths.applications_dir)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    let hicolor = paths.icons_dir.join("hicolor");
    if hicolor.join("index.theme").is_file() {
        let _ = Command::new("gtk-update-icon-cache")
            .arg("-f")
            .arg("-t")
            .arg(&hicolor)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
    if paths.mime_dir.join("packages").is_dir() {
        let _ = Command::new("update-mime-database")
            .arg(&paths.mime_dir)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

/// Removes an app completely and returns whether anything was removed.
pub fn uninstall(app: &AppDef, paths: &Paths) -> Result<bool> {
    let bundle = app.bundle_id();
    let mut removed = false;

    let app_dir = paths.app_dir(app.id);
    if paths.is_managed_path(&app_dir) && app_dir.exists() {
        fs::remove_dir_all(&app_dir)?;
        removed = true;
    }

    if let Ok(entries) = fs::read_dir(&paths.bin_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(target) = fs::read_link(&path) else {
                continue;
            };
            if target.starts_with(&app_dir) || target.starts_with(paths.install_root.join(app.id)) {
                let _ = fs::remove_file(&path);
                removed = true;
            }
        }
    }

    let desktop = paths.desktop_file(&bundle);
    if desktop.exists() {
        fs::remove_file(desktop)?;
        removed = true;
    }

    for base in [paths.icons_dir.join("hicolor"), paths.mime_dir.clone(), paths.metainfo_dir.clone()] {
        remove_matching(&base, &bundle, app.id);
    }

    refresh_desktop_databases(paths);
    Ok(removed)
}

fn remove_matching(base: &Path, bundle: &str, id: &str) {
    let Ok(entries) = fs::read_dir(base) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            remove_matching(&path, bundle, id);
        } else if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
            if name.contains(bundle) || name.contains(id) {
                let _ = fs::remove_file(path);
            }
        }
    }
}

/// Reads at most `limit` bytes from a reader, for diagnostics.
pub fn read_prefix(mut reader: impl Read, limit: usize) -> Vec<u8> {
    let mut buf = vec![0u8; limit];
    let mut read = 0;
    while read < limit {
        match reader.read(&mut buf[read..]) {
            Ok(0) | Err(_) => break,
            Ok(n) => read += n,
        }
    }
    buf.truncate(read);
    buf
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog;

    #[test]
    fn rejects_unsafe_archive_paths() {
        let mut top = None;
        assert!(checked_relative(Path::new("../evil"), &mut top).is_err());
        assert!(checked_relative(Path::new("/etc/passwd"), &mut top).is_err());
        assert!(checked_relative(Path::new("./x"), &mut top).is_err());
        assert!(checked_relative(Path::new("gridcraft-0.13.0/bin/gridcraft"), &mut top).is_ok());
        assert_eq!(top.as_deref(), Some("gridcraft-0.13.0"));
        assert!(checked_relative(Path::new("other/bin/x"), &mut top).is_err());
        assert_eq!(
            checked_relative(Path::new("gridcraft-0.13.0/share/x"), &mut top).unwrap(),
            PathBuf::from("share/x")
        );
    }

    #[test]
    fn desktop_file_uses_absolute_paths_and_keeps_arguments() {
        let app = catalog::by_id("gridcraft").unwrap();
        let source = "\
[Desktop Entry]
Type=Application
Name=GridCraft
Exec=gridcraft %F
TryExec=gridcraft
Icon=ai.storyteller.gridcraft
X-ArtCraft-Version=old
";
        let text = build_desktop_file(
            app,
            "0.13.0",
            Path::new("/home/me/.local/opt/artcraft/gridcraft/0.13.0/bin/gridcraft"),
            "ai.storyteller.gridcraft",
            Some(source),
        );
        assert!(text.contains(
            "Exec=\"/home/me/.local/opt/artcraft/gridcraft/0.13.0/bin/gridcraft\" %F"
        ));
        assert!(text.contains(
            "TryExec=/home/me/.local/opt/artcraft/gridcraft/0.13.0/bin/gridcraft"
        ));
        assert!(text.contains("Icon=ai.storyteller.gridcraft"));
        assert!(text.contains("X-ArtCraft-Version=0.13.0"));
        assert!(!text.contains("X-ArtCraft-Version=old"));
        assert_eq!(text.matches("X-ArtCraft-Managed=true").count(), 1);
    }

    #[test]
    fn desktop_file_is_synthesised_when_missing() {
        let app = catalog::by_id("soundcraft").unwrap();
        let text = build_desktop_file(
            app,
            "2.0.0",
            Path::new("/tmp/soundcraft"),
            "ai.storyteller.soundcraft",
            None,
        );
        assert!(text.starts_with("[Desktop Entry]\n"));
        assert!(text.contains("Name=SoundCraft"));
        assert!(text.contains("Exec=\"/tmp/soundcraft\""));
        assert!(text.contains("Terminal=false"));
    }

    #[test]
    fn quotes_percent_and_special_characters() {
        let quoted = quote_exec(Path::new("/home/a b/100%/x$y"));
        assert_eq!(quoted, "\"/home/a b/100%%/x\\$y\"");
    }
}
