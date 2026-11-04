//! Persistent state: what is installed and what the last release check found.
//!
//! The filesystem is the source of truth for installations; `state.json` only
//! adds metadata (checksum, tag, install time), remembers older versions for
//! rollback, stores version pins/skips and caches release lookups so the
//! Omarchy widget can answer instantly.

use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
#[cfg(unix)]
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::catalog::SUITE;
use crate::error::Result;
use crate::github::ReleasePlan;
use crate::paths::{Paths, sanitize_component};
use crate::time::now_unix;
use crate::version::ReleaseVersion;

/// One installed app version.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct InstalledApp {
    pub version: String,
    pub tag: String,
    pub sha256: String,
    pub installed_at: i64,
    pub dir: PathBuf,
    pub desktop_file: PathBuf,
}

/// A version that is still present on disk and can be activated again.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct VersionEntry {
    pub version: String,
    pub tag: String,
    pub sha256: String,
    pub installed_at: i64,
    pub dir: PathBuf,
}

impl VersionEntry {
    pub fn from_installed(app: &InstalledApp) -> Self {
        Self {
            version: app.version.clone(),
            tag: app.tag.clone(),
            sha256: app.sha256.clone(),
            installed_at: app.installed_at,
            dir: app.dir.clone(),
        }
    }

    pub fn to_installed(&self, paths: &Paths, id: &str) -> InstalledApp {
        InstalledApp {
            version: self.version.clone(),
            tag: self.tag.clone(),
            sha256: self.sha256.clone(),
            installed_at: self.installed_at,
            dir: self.dir.clone(),
            desktop_file: paths.desktop_file(&format!("ai.storyteller.{id}")),
        }
    }
}

/// A release lookup cached for `metadata_cache_minutes`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CachedRelease {
    pub fetched_at: i64,
    pub plan: ReleasePlan,
    /// Validators for conditional GitHub API requests.
    #[serde(default)]
    pub etag: Option<String>,
    #[serde(default)]
    pub last_modified: Option<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct State {
    #[serde(default)]
    pub installed: BTreeMap<String, InstalledApp>,
    #[serde(default)]
    pub releases: BTreeMap<String, CachedRelease>,
    #[serde(default)]
    pub checked_at: Option<i64>,
    /// Every version still on disk per app, newest first.
    #[serde(default)]
    pub history: BTreeMap<String, Vec<VersionEntry>>,
    /// App id -> pinned tag. Pinned apps are never updated automatically.
    #[serde(default)]
    pub pins: BTreeMap<String, String>,
    /// App id -> tag that the user asked to skip.
    #[serde(default)]
    pub skipped: BTreeMap<String, String>,
}

impl State {
    /// Loads the state file, falling back to an empty state when it is missing
    /// or unreadable. A corrupt cache must never block the app.
    pub fn load(paths: &Paths) -> Self {
        let path = paths.state_file();
        match fs::read_to_string(&path) {
            Ok(text) => serde_json::from_str(&text).unwrap_or_default(),
            Err(_) => Self::default(),
        }
    }

    pub fn save(&self, paths: &Paths) -> Result<()> {
        fs::create_dir_all(&paths.state_dir)?;
        let json = serde_json::to_vec_pretty(self)?;
        write_atomic(&paths.state_file(), &json, 0o644)?;
        Ok(())
    }

    /// Replaces the installed map with what actually exists on disk, keeping
    /// metadata for versions that are still present and rebuilding history.
    pub fn reconcile(&mut self, paths: &Paths) {
        let mut merged_installed = BTreeMap::new();
        let mut history = BTreeMap::new();

        for app in SUITE.iter() {
            let scanned = scan_versions(paths, app.id);
            if scanned.is_empty() {
                continue;
            }

            let mut metadata: BTreeMap<String, (String, String, i64)> = BTreeMap::new();
            if let Some(old_history) = self.history.get(app.id) {
                for entry in old_history {
                    metadata.insert(
                        entry.version.clone(),
                        (entry.tag.clone(), entry.sha256.clone(), entry.installed_at),
                    );
                }
            }
            if let Some(old) = self.installed.get(app.id) {
                metadata.entry(old.version.clone()).or_insert_with(|| {
                    (old.tag.clone(), old.sha256.clone(), old.installed_at)
                });
            }

            let entries: Vec<VersionEntry> = scanned
                .into_iter()
                .map(|found| match metadata.get(&found.version) {
                    Some((tag, sha256, installed_at)) => VersionEntry {
                        tag: tag.clone(),
                        sha256: sha256.clone(),
                        installed_at: *installed_at,
                        ..found
                    },
                    None => found,
                })
                .collect();

            if let Some(active) = entries.first() {
                merged_installed.insert(app.id.to_string(), active.to_installed(paths, app.id));
            }
            history.insert(app.id.to_string(), entries);
        }

        self.installed = merged_installed;
        self.history = history;
        // Drop cached data whose app disappeared from the catalogue.
        let known = |id: &String| SUITE.iter().any(|a| a.id == id);
        self.releases.retain(|id, _| known(id));
        self.pins.retain(|id, _| known(id));
        self.skipped.retain(|id, _| known(id));
    }

    pub fn cached_plan(&self, id: &str, ttl_secs: i64, now: i64) -> Option<&ReleasePlan> {
        let entry = self.releases.get(id)?;
        if now.saturating_sub(entry.fetched_at) <= ttl_secs {
            Some(&entry.plan)
        } else {
            None
        }
    }

    pub fn cached_release(&self, id: &str) -> Option<&CachedRelease> {
        self.releases.get(id)
    }

    pub fn store_plan(&mut self, plan: &ReleasePlan, now: i64) {
        self.store_plan_with_validators(plan, now, None, None);
    }

    pub fn store_plan_with_validators(
        &mut self,
        plan: &ReleasePlan,
        now: i64,
        etag: Option<String>,
        last_modified: Option<String>,
    ) {
        self.releases.insert(
            plan.app_id.clone(),
            CachedRelease {
                fetched_at: now,
                plan: plan.clone(),
                etag,
                last_modified,
            },
        );
    }

    pub fn installed_version(&self, id: &str) -> Option<&str> {
        self.installed.get(id).map(|a| a.version.as_str())
    }

    /// Records a version that now exists on disk, replacing older metadata for
    /// the same version and keeping the list sorted newest first.
    pub fn record_history(&mut self, id: &str, installed: &InstalledApp) {
        let entry = VersionEntry::from_installed(installed);
        let list = self.history.entry(id.to_string()).or_default();
        list.retain(|existing| existing.version != entry.version);
        list.push(entry);
        sort_versions(list);
    }

    pub fn versions(&self, id: &str) -> Vec<String> {
        self.history
            .get(id)
            .map(|list| list.iter().map(|e| e.version.clone()).collect())
            .unwrap_or_default()
    }

    /// The newest installed version that is older than `active`.
    pub fn rollback_target(&self, id: &str, active: &str) -> Option<&VersionEntry> {
        let active = ReleaseVersion::parse(active);
        self.history.get(id)?.iter().find(|entry| {
            ReleaseVersion::parse(&entry.version)
                .cmp_version(&active)
                .is_lt()
        })
    }

    pub fn find_version(&self, id: &str, version: &str) -> Option<&VersionEntry> {
        self.history
            .get(id)?
            .iter()
            .find(|entry| entry.version == version || entry.tag == version)
    }

    pub fn pinned_tag(&self, id: &str) -> Option<&str> {
        self.pins.get(id).map(String::as_str)
    }

    pub fn is_pinned(&self, id: &str) -> bool {
        self.pins.contains_key(id)
    }

    pub fn pin(&mut self, id: &str, tag: &str) {
        self.pins.insert(id.to_string(), tag.to_string());
    }

    pub fn unpin(&mut self, id: &str) {
        self.pins.remove(id);
    }

    pub fn is_skipped(&self, id: &str, tag: &str) -> bool {
        self.skipped.get(id).is_some_and(|skipped| skipped == tag)
    }

    pub fn skip(&mut self, id: &str, tag: &str) {
        self.skipped.insert(id.to_string(), tag.to_string());
    }

    pub fn unskip(&mut self, id: &str) {
        self.skipped.remove(id);
    }
}

/// Reads `<root>/<id>/<version>` and returns every version that contains a
/// `bin/` directory, newest first.
pub fn scan_versions(paths: &Paths, id: &str) -> Vec<VersionEntry> {
    let root = paths.app_dir(id);
    let Ok(entries) = fs::read_dir(&root) else {
        return Vec::new();
    };
    let mut found: Vec<(ReleaseVersion, PathBuf)> = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if name.starts_with('.') {
            continue;
        }
        #[cfg(target_os = "windows")]
        let valid = path.join("bin").is_dir() || has_windows_executable(&path);
        #[cfg(not(target_os = "windows"))]
        let valid = path.join("bin").is_dir();
        if !valid {
            continue;
        }
        found.push((ReleaseVersion::parse(name), path));
    }
    found.sort_by(|a, b| b.0.cmp_version(&a.0));
    found
        .into_iter()
        .map(|(version, dir)| VersionEntry {
            version: version.raw.clone(),
            tag: version.raw,
            sha256: String::new(),
            installed_at: 0,
            dir,
        })
        .collect()
}

fn sort_versions(list: &mut [VersionEntry]) {
    list.sort_by(|a, b| {
        ReleaseVersion::parse(&b.version).cmp_version(&ReleaseVersion::parse(&a.version))
    });
}

/// Reads `~/.local/opt/artcraft/<id>/<version>` and picks the newest version
/// that actually contains a `bin/` directory.
pub fn scan_installed(paths: &Paths) -> BTreeMap<String, InstalledApp> {
    let mut out = BTreeMap::new();
    for app in SUITE.iter() {
        let versions = scan_versions(paths, app.id);
        if let Some(active) = versions.first() {
            out.insert(app.id.to_string(), active.to_installed(paths, app.id));
        }
    }
    out
}

/// The version directory used while installing: `<root>/<id>/.staging-<pid>`.
pub fn staging_dir(paths: &Paths, id: &str) -> PathBuf {
    paths
        .app_dir(id)
        .join(format!(".staging-{}", std::process::id()))
}

/// Directory name for a version, safe against traversal.
pub fn version_dir_name(version: &str) -> String {
    sanitize_component(version)
}

pub fn write_atomic(path: &Path, bytes: &[u8], mode: u32) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension(format!(
        "{}.tmp",
        path.extension().and_then(|e| e.to_str()).unwrap_or("json")
    ));
    {
        let mut file = fs::OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(&tmp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
    }
    #[cfg(unix)] fs::set_permissions(&tmp, fs::Permissions::from_mode(mode))?;
    fs::rename(&tmp, path)?;
    Ok(())
}

/// Builds the `InstalledApp` record for a freshly installed version.
pub fn installed_record(paths: &Paths, plan: &ReleasePlan, dir: &Path) -> InstalledApp {
    InstalledApp {
        version: plan.version.clone(),
        tag: plan.tag.clone(),
        sha256: plan.sha256.clone(),
        installed_at: now_unix(),
        dir: dir.to_path_buf(),
        desktop_file: paths.desktop_file(&format!("ai.storyteller.{}", plan.app_id)),
    }
}


#[cfg(target_os = "windows")]
fn has_windows_executable(dir: &std::path::Path) -> bool {
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_file() {
                if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                    if name.to_ascii_lowercase().ends_with(".exe") {
                        return true;
                    }
                }
            } else if path.is_dir() {
                if has_windows_executable(&path) {
                    return true;
                }
            }
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_paths() -> (tempfile::TempDir, Paths) {
        let dir = tempfile::tempdir().unwrap();
        let mut paths = Paths::from_env();
        let home = dir.path();
        paths.config_dir = home.join("config");
        paths.state_dir = home.join("state");
        paths.cache_dir = home.join("cache");
        paths.data_dir = home.join("data");
        paths.install_root = home.join("opt");
        paths.bin_dir = home.join("bin");
        paths.applications_dir = home.join("data/applications");
        paths.desktop_dir = paths.applications_dir.clone();
        paths.icons_dir = home.join("data/icons");
        paths.mime_dir = home.join("data/mime");
        paths.metainfo_dir = home.join("data/metainfo");
        paths.ensure().unwrap();
        (dir, paths)
    }

    fn plan(tag: &str) -> ReleasePlan {
        let version = ReleaseVersion::parse(tag).raw;
        ReleasePlan {
            app_id: "gridcraft".into(),
            tag: tag.into(),
            version,
            asset_name: "x.tar.gz".into(),
            asset_url: "https://github.com/storytold/gridcraft/releases/download/v1.0.0/x.tar.gz".into(),
            asset_size: 1,
            sha256: "a".repeat(64),
            sums_sha256: None,
            api_sha256: None,
            release_url: "https://github.com/storytold/gridcraft/releases".into(),
            published_at: None,
            notes: String::new(),
            prerelease: false,
        }
    }

    #[test]
    fn scans_the_newest_version_on_disk() {
        let (_dir, paths) = temp_paths();
        for version in ["0.9.0", "0.13.0", "0.10.1"] {
            let dir = paths.app_version_dir("gridcraft", version);
            fs::create_dir_all(dir.join("bin")).unwrap();
            fs::write(dir.join("bin/gridcraft"), b"bin").unwrap();
        }
        fs::create_dir_all(paths.app_dir("gridcraft").join(".staging-1/bin")).unwrap();
        let found = scan_installed(&paths);
        assert_eq!(found["gridcraft"].version, "0.13.0");
        let history: Vec<_> = scan_versions(&paths, "gridcraft")
            .into_iter()
            .map(|entry| entry.version)
            .collect();
        assert_eq!(history, vec!["0.13.0", "0.10.1", "0.9.0"]);
    }

    #[test]
    fn cached_plans_expire() {
        let mut state = State::default();
        state.store_plan(&plan("v1.0.0"), 100);
        assert!(state.cached_plan("gridcraft", 60, 150).is_some());
        assert!(state.cached_plan("gridcraft", 60, 200).is_none());
    }

    #[test]
    fn reconcile_keeps_history_and_metadata() {
        let (_dir, paths) = temp_paths();
        for version in ["1.0.0", "1.1.0"] {
            let dir = paths.app_version_dir("gridcraft", version);
            fs::create_dir_all(dir.join("bin")).unwrap();
        }
        let installed = scan_installed(&paths);
        assert_eq!(installed["gridcraft"].version, "1.1.0");

        let mut state = State::default();
        state.history.insert(
            "gridcraft".into(),
            vec![VersionEntry {
                version: "1.0.0".into(),
                tag: "v1.0.0".into(),
                sha256: "b".repeat(64),
                installed_at: 42,
                dir: paths.app_version_dir("gridcraft", "1.0.0"),
            }],
        );
        state.reconcile(&paths);

        let history = &state.history["gridcraft"];
        assert_eq!(history.len(), 2);
        assert_eq!(history[0].version, "1.1.0");
        assert_eq!(history[1].tag, "v1.0.0");
        assert_eq!(history[1].installed_at, 42);
        assert_eq!(state.installed["gridcraft"].version, "1.1.0");
    }

    #[test]
    fn pins_skips_and_rollback_targets() {
        let (_dir, paths) = temp_paths();
        let mut state = State::default();
        state.history.insert(
            "gridcraft".into(),
            vec![
                VersionEntry {
                    version: "2.0.0".into(),
                    tag: "v2.0.0".into(),
                    sha256: String::new(),
                    installed_at: 0,
                    dir: paths.app_version_dir("gridcraft", "2.0.0"),
                },
                VersionEntry {
                    version: "1.0.0".into(),
                    tag: "v1.0.0".into(),
                    sha256: String::new(),
                    installed_at: 0,
                    dir: paths.app_version_dir("gridcraft", "1.0.0"),
                },
            ],
        );
        assert_eq!(
            state.rollback_target("gridcraft", "2.0.0").unwrap().version,
            "1.0.0"
        );
        assert!(state.rollback_target("gridcraft", "1.0.0").is_none());

        state.pin("gridcraft", "v1.0.0");
        assert!(state.is_pinned("gridcraft"));
        assert_eq!(state.pinned_tag("gridcraft"), Some("v1.0.0"));
        state.unpin("gridcraft");
        assert!(!state.is_pinned("gridcraft"));

        state.skip("gridcraft", "v2.0.0");
        assert!(state.is_skipped("gridcraft", "v2.0.0"));
        assert!(!state.is_skipped("gridcraft", "v1.0.0"));
        state.unskip("gridcraft");
        assert!(!state.is_skipped("gridcraft", "v2.0.0"));
    }
}
