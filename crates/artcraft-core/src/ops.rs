//! High-level operations shared by the GUI, the CLI and the Omarchy widget.
//!
//! Every mutating operation takes the process-wide [`OperationLock`] so the
//! GUI, the CLI and the bar plugin cannot race each other. Read-only status
//! checks take the same lock for a consistent snapshot and fall back to cached
//! data when another process is busy.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::catalog::{self, AppDef};
use crate::config::Config;
use crate::download;
use crate::error::{Error, Result};
use crate::github::{self, ReleasePlan};
use crate::install;
use crate::lock::OperationLock;
use crate::paths::Paths;
use crate::progress::{self, Progress, ProgressFn};
use crate::state::{self, State};
use crate::time::now_unix;
use crate::trust;
use crate::version::ReleaseVersion;
use crate::vt::{Verdict, VtClient, VtResult};

/// One row in the hub / one entry of `artcraftctl status`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AppStatus {
    pub id: String,
    pub name: String,
    pub tagline: String,
    pub color: [u8; 3],
    pub installed_version: Option<String>,
    pub latest_version: Option<String>,
    pub latest_tag: Option<String>,
    pub latest_sha256: Option<String>,
    /// An update from an installed version is available.
    pub update_available: bool,
    /// The app is not installed at all.
    pub not_installed: bool,
    /// Updates are disabled because the app is pinned to a tag.
    #[serde(default)]
    pub pinned: bool,
    #[serde(default)]
    pub pinned_tag: Option<String>,
    /// The latest release is explicitly skipped.
    #[serde(default)]
    pub skipped: bool,
    /// Newest older version still on disk, usable for rollback.
    #[serde(default)]
    pub rollback_version: Option<String>,
    /// All versions still on disk, newest first.
    #[serde(default)]
    pub versions: Vec<String>,
    pub error: Option<String>,
    /// Full plan for display/installation; not part of the JSON status.
    #[serde(skip)]
    pub plan: Option<ReleasePlan>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct InstallOutcome {
    pub app_id: String,
    pub version: String,
    pub already_installed: bool,
    pub downloaded: bool,
    pub vt: Option<VtResult>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct UpgradeReport {
    pub updated: Vec<String>,
    pub failed: Vec<FailedApp>,
    pub skipped: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FailedApp {
    pub id: String,
    pub name: String,
    pub error: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DoctorItem {
    pub name: String,
    pub ok: bool,
    pub detail: String,
}

#[derive(Clone)]
struct CachedInput {
    plan: ReleasePlan,
    etag: Option<String>,
    last_modified: Option<String>,
    fresh: bool,
}

#[derive(Clone)]
struct Task {
    app: AppDef,
    cached: Option<CachedInput>,
}

struct CheckResult {
    id: String,
    plan: Option<ReleasePlan>,
    etag: Option<String>,
    last_modified: Option<String>,
    error: Option<String>,
    from_network: bool,
}

// ---------------------------------------------------------------------------
// Status
// ---------------------------------------------------------------------------

/// Checks every app. Uses cached metadata while it is fresh; `force` always
/// asks the network. If another ArtCraft process holds the lock, the cached
/// state is returned instead of blocking.
pub fn check_all(
    paths: &Paths,
    cfg: &Config,
    force: bool,
    progress: ProgressFn<'_>,
) -> Vec<AppStatus> {
    let _lock = match OperationLock::acquire(paths) {
        Ok(lock) => lock,
        Err(Error::Busy) => {
            progress::message(
                progress,
                "Another ArtCraft operation is running; showing cached status.",
            );
            return cached_statuses(paths);
        }
        Err(error) => {
            progress::message(progress, format!("Note: status lock unavailable ({error})."));
            return cached_statuses(paths);
        }
    };
    check_all_inner(paths, cfg, force, progress)
}

/// Status from the last saved state, without any network or disk scan writes.
fn cached_statuses(paths: &Paths) -> Vec<AppStatus> {
    let mut state = State::load(paths);
    state.reconcile(paths);
    catalog::SUITE
        .iter()
        .map(|app| {
            let plan = state.cached_release(app.id).map(|cached| cached.plan.clone());
            status_for(app, state.installed.get(app.id), plan, None, &state)
        })
        .collect()
}

/// Assumes the caller already holds the operation lock.
fn check_all_inner(
    paths: &Paths,
    cfg: &Config,
    force: bool,
    progress: ProgressFn<'_>,
) -> Vec<AppStatus> {
    let now = now_unix();
    let mut state = State::load(paths);
    state.reconcile(paths);
    let ttl = cfg.metadata_ttl_secs();

    let tasks: Vec<Task> = catalog::SUITE
        .iter()
        .map(|app| Task {
            app: *app,
            cached: state.cached_release(app.id).map(|cached| CachedInput {
                plan: cached.plan.clone(),
                etag: cached.etag.clone(),
                last_modified: cached.last_modified.clone(),
                fresh: now.saturating_sub(cached.fetched_at) <= ttl,
            }),
        })
        .collect();

    let results = run_checks(tasks, cfg, force, progress);

    let mut outcomes: BTreeMap<String, (Option<ReleasePlan>, Option<String>)> = BTreeMap::new();
    for result in results {
        if let Some(plan) = &result.plan {
            if result.from_network {
                state.store_plan_with_validators(
                    plan,
                    now,
                    result.etag.clone(),
                    result.last_modified.clone(),
                );
            }
            if let Some(app) = catalog::by_id(&result.id) {
                prune_download_cache(paths, app, &plan.asset_name);
            }
        }
        outcomes.insert(result.id, (result.plan, result.error));
    }

    let out = catalog::SUITE
        .iter()
        .map(|app| {
            let (plan, error) = match outcomes.remove(app.id) {
                Some((plan, error)) => (plan, error),
                None => (
                    state.cached_release(app.id).map(|cached| cached.plan.clone()),
                    None,
                ),
            };
            status_for(app, state.installed.get(app.id), plan, error, &state)
        })
        .collect();

    state.checked_at = Some(now);
    if let Err(error) = state.save(paths) {
        progress::message(progress, format!("Note: status not saved ({error})"));
    }
    out
}

fn run_checks(
    tasks: Vec<Task>,
    cfg: &Config,
    force: bool,
    progress: ProgressFn<'_>,
) -> Vec<CheckResult> {
    let workers = cfg.parallel_checks.clamp(1, 8) as usize;
    if workers <= 1 || tasks.len() <= 1 {
        return tasks
            .iter()
            .map(|task| check_one(task, cfg, force, progress))
            .collect();
    }

    let chunk_size = tasks.len().div_ceil(workers);
    let mut results = Vec::with_capacity(tasks.len());
    std::thread::scope(|scope| {
        let mut handles = Vec::new();
        for chunk in tasks.chunks(chunk_size) {
            let chunk = chunk.to_vec();
            handles.push(scope.spawn(move || {
                chunk
                    .iter()
                    .map(|task| check_one(task, cfg, force, progress))
                    .collect::<Vec<_>>()
            }));
        }
        for handle in handles {
            if let Ok(chunk) = handle.join() {
                results.extend(chunk);
            }
        }
    });
    results
}

fn check_one(
    task: &Task,
    cfg: &Config,
    force: bool,
    progress: ProgressFn<'_>,
) -> CheckResult {
    let app = &task.app;
    if !force {
        if let Some(cached) = task.cached.as_ref().filter(|cached| cached.fresh) {
            return CheckResult {
                id: app.id.to_string(),
                plan: Some(cached.plan.clone()),
                etag: cached.etag.clone(),
                last_modified: cached.last_modified.clone(),
                error: None,
                from_network: false,
            };
        }
    }

    progress::message(progress, format!("Checking {} …", app.name));
    let cached = task.cached.as_ref();
    match github::resolve_conditional(
        app,
        cfg,
        cached.map(|cached| &cached.plan),
        cached.and_then(|cached| cached.etag.as_deref()),
        cached.and_then(|cached| cached.last_modified.as_deref()),
    ) {
        Ok(outcome) => CheckResult {
            id: app.id.to_string(),
            plan: Some(outcome.plan),
            etag: outcome.etag,
            last_modified: outcome.last_modified,
            error: None,
            from_network: true,
        },
        Err(error) => CheckResult {
            id: app.id.to_string(),
            plan: cached.map(|cached| cached.plan.clone()),
            etag: cached.and_then(|cached| cached.etag.clone()),
            last_modified: cached.and_then(|cached| cached.last_modified.clone()),
            error: Some(error.to_string()),
            from_network: false,
        },
    }
}

fn status_for(
    app: &AppDef,
    installed: Option<&state::InstalledApp>,
    plan: Option<ReleasePlan>,
    error: Option<String>,
    state: &State,
) -> AppStatus {
    let pinned_tag = state.pinned_tag(app.id).map(str::to_string);
    let pinned = pinned_tag.is_some();
    let skipped = plan
        .as_ref()
        .is_some_and(|plan| state.is_skipped(app.id, &plan.tag));
    let update_available = match (installed, plan.as_ref()) {
        (Some(installed), Some(plan)) if !pinned && !skipped => {
            plan.version().is_newer_than(&installed.version)
        }
        _ => false,
    };
    let rollback_version = installed
        .and_then(|installed| state.rollback_target(app.id, &installed.version))
        .map(|entry| entry.version.clone());
    AppStatus {
        id: app.id.to_string(),
        name: app.name.to_string(),
        tagline: app.tagline.to_string(),
        color: app.color,
        installed_version: installed.map(|i| i.version.clone()),
        latest_version: plan.as_ref().map(|p| p.version.clone()),
        latest_tag: plan.as_ref().map(|p| p.tag.clone()),
        latest_sha256: plan.as_ref().map(|p| p.sha256.clone()),
        update_available,
        not_installed: installed.is_none(),
        pinned,
        pinned_tag,
        skipped,
        rollback_version,
        versions: state.versions(app.id),
        error,
        plan,
    }
}

// ---------------------------------------------------------------------------
// Install / upgrade
// ---------------------------------------------------------------------------

/// Installs or reinstalls one app. With `force = false` an existing latest
/// version is left alone.
pub fn install(
    app_id: &str,
    paths: &Paths,
    cfg: &Config,
    force: bool,
    progress: ProgressFn<'_>,
) -> Result<InstallOutcome> {
    let _lock = OperationLock::acquire(paths)?;
    install_locked(app_id, paths, cfg, force, None, progress)
}

/// Installs one explicit tag and pins the app to it.
pub fn install_version(
    app_id: &str,
    tag: &str,
    paths: &Paths,
    cfg: &Config,
    progress: ProgressFn<'_>,
) -> Result<InstallOutcome> {
    let _lock = OperationLock::acquire(paths)?;
    install_locked(app_id, paths, cfg, true, Some(tag), progress)
}

fn install_locked(
    app_id: &str,
    paths: &Paths,
    cfg: &Config,
    force: bool,
    version: Option<&str>,
    progress: ProgressFn<'_>,
) -> Result<InstallOutcome> {
    let app = catalog::by_id(app_id)
        .ok_or_else(|| Error::Other(format!("Unknown app: {app_id}")))?;

    if cfg.vt_key().is_none() {
        return Err(Error::VirusTotalNotConfigured);
    }

    let mut state = State::load(paths);
    state.reconcile(paths);

    if version.is_none() && !force {
        if let Some(installed) = state.installed.get(app.id) {
            if let Some(cached) = state.cached_plan(app.id, cfg.metadata_ttl_secs(), now_unix()) {
                if installed.version == cached.version {
                    return Ok(InstallOutcome {
                        app_id: app.id.to_string(),
                        version: cached.version.clone(),
                        already_installed: true,
                        downloaded: false,
                        vt: None,
                    });
                }
            }
        }
    }

    let now = now_unix();
    let (plan, etag, last_modified, fetched) =
        resolve_plan(app, cfg, &state, force, version, progress)?;

    if version.is_none() && !force {
        if let Some(installed) = state.installed.get(app.id) {
            if installed.version == plan.version {
                return Ok(InstallOutcome {
                    app_id: app.id.to_string(),
                    version: plan.version,
                    already_installed: true,
                    downloaded: false,
                    vt: None,
                });
            }
        }
    }

    let outcome = install_with_plan_inner(app, &plan, paths, cfg, progress)?;
    state.installed.insert(app.id.to_string(), outcome.1.clone());
    state.record_history(app.id, &outcome.1);

    if version.is_some() {
        // Explicit version installs are pinned so an automatic upgrade does
        // not immediately undo the user's choice.
        state.pin(app.id, &plan.tag);
        state.unskip(app.id);
    } else if fetched {
        state.store_plan_with_validators(&plan, now, etag, last_modified);
    }
    state.save(paths)?;
    Ok(outcome.0)
}

fn resolve_plan(
    app: &AppDef,
    cfg: &Config,
    state: &State,
    force: bool,
    version: Option<&str>,
    progress: ProgressFn<'_>,
) -> Result<(ReleasePlan, Option<String>, Option<String>, bool)> {
    if let Some(tag) = version {
        progress::message(
            progress,
            format!("Resolving {} {} …", app.name, tag),
        );
        let plan = github::resolve_tag(app, tag, cfg)?;
        return Ok((plan, None, None, false));
    }

    let now = now_unix();
    if !force {
        if let Some(plan) = state.cached_plan(app.id, cfg.metadata_ttl_secs(), now) {
            let cached = state.cached_release(app.id);
            return Ok((
                plan.clone(),
                cached.and_then(|c| c.etag.clone()),
                cached.and_then(|c| c.last_modified.clone()),
                false,
            ));
        }
    }

    progress::message(
        progress,
        format!("Resolving latest version of {} …", app.name),
    );
    let cached = state.cached_release(app.id);
    let outcome = github::resolve_conditional(
        app,
        cfg,
        cached.map(|c| &c.plan),
        cached.and_then(|c| c.etag.as_deref()),
        cached.and_then(|c| c.last_modified.as_deref()),
    )?;
    Ok((outcome.plan, outcome.etag, outcome.last_modified, true))
}

/// Installs a plan that has already been resolved (used by bulk upgrades).
/// Returns both the public outcome and the state record.
fn install_with_plan_inner(
    app: &AppDef,
    plan: &ReleasePlan,
    paths: &Paths,
    cfg: &Config,
    progress: ProgressFn<'_>,
) -> Result<(InstallOutcome, state::InstalledApp)> {
    progress::message(
        progress,
        format!("{} {} is being checked …", app.name, plan.version),
    );

    // 1. VirusTotal, before anything is downloaded.
    let mut vt = VtClient::new(paths, cfg)?;
    let report = |text: &str| progress(Progress::Message(text.to_string()));
    let verdict = vt.check(&plan.sha256, &plan.asset_url, &report)?;
    if verdict.verdict == Verdict::Blocked {
        return Err(Error::VirusTotalBlocked(verdict.detail.clone()));
    }
    if verdict.verdict == Verdict::Unknown {
        progress::message(
            progress,
            format!(
                "Warning: VirusTotal is unclear ({}), continuing because allow_unknown is set.",
                verdict.detail
            ),
        );
    } else {
        progress::message(
            progress,
            format!(
                "VirusTotal: clean (0 of {} engines).",
                verdict.stats.total()
            ),
        );
    }

    // 2. Signed checksum manifest, before spending bandwidth on the artifact.
    let trust_agent = crate::net::agent(Duration::from_secs(60));
    trust::verify_manifest_signature(&trust_agent, app, plan, cfg, progress)?;

    // 3. Download into the cache, hashing while writing.
    paths.ensure()?;
    let archive = paths.download_dir().join(&plan.asset_name);
    let mut downloaded = false;
    if archive.is_file() && download::has_checksum(&archive, &plan.sha256) {
        progress::message(progress, "Archive is already in the cache and verified.");
    } else {
        if archive.exists() {
            let _ = std::fs::remove_file(&archive);
        }
        progress::message(
            progress,
            format!(
                "Downloading {} ({} MB) …",
                plan.asset_name,
                plan.asset_size / 1024 / 1024
            ),
        );
        let agent = crate::net::agent(Duration::from_secs(600));
        download::fetch_with_retries(
            &agent,
            &plan.asset_url,
            &archive,
            &plan.sha256,
            cfg.network_attempts(),
            &|event| progress(event),
        )?;
        downloaded = true;
        progress::message(progress, "SHA-256 matches SHA256SUMS.txt.");
    }

    // 4. Artifact signature and provenance attestations.
    trust::verify_artifact_signature(&trust_agent, plan, &archive, cfg, progress)?;
    trust::verify_attestations(app.repo, &archive, cfg, progress)?;

    // 5. Extract transactionally and register.
    let installed = install::install_archive(app, plan, &archive, paths, cfg, progress)?;
    // Keep the download cache bounded: only the just-installed archive survives.
    prune_download_cache(paths, app, &plan.asset_name);
    progress::message(progress, format!("{} {} is installed.", app.name, plan.version));

    Ok((
        InstallOutcome {
            app_id: app.id.to_string(),
            version: plan.version.clone(),
            already_installed: false,
            downloaded,
            vt: Some(verdict),
        },
        installed,
    ))
}

/// Installs every available update, skipping failures.
pub fn upgrade_all(paths: &Paths, cfg: &Config, progress: ProgressFn<'_>) -> UpgradeReport {
    let mut report = UpgradeReport::default();
    let _lock = match OperationLock::acquire(paths) {
        Ok(lock) => lock,
        Err(error) => {
            report.failed.push(FailedApp {
                id: "*".into(),
                name: "All apps".into(),
                error: error.to_string(),
            });
            return report;
        }
    };

    let statuses = check_all_inner(paths, cfg, true, progress);
    let mut state = State::load(paths);
    state.reconcile(paths);

    for status in statuses {
        let Some(app) = catalog::by_id(&status.id) else {
            continue;
        };
        if let Some(error) = &status.error {
            report.failed.push(FailedApp {
                id: status.id.clone(),
                name: status.name.clone(),
                error: error.clone(),
            });
            continue;
        }
        if status.pinned || status.skipped {
            if status.installed_version.is_some() {
                report.skipped.push(status.id.clone());
            }
            continue;
        }
        if !status.update_available {
            if status.installed_version.is_some() {
                report.skipped.push(status.id.clone());
            }
            continue;
        }
        let Some(plan) = status.plan else {
            continue;
        };
        match install_with_plan_inner(app, &plan, paths, cfg, progress) {
            Ok((_, installed)) => {
                state.installed.insert(app.id.to_string(), installed.clone());
                state.record_history(app.id, &installed);
                state.store_plan(&plan, now_unix());
                report.updated.push(status.id.clone());
            }
            Err(error) => report.failed.push(FailedApp {
                id: status.id.clone(),
                name: status.name.clone(),
                error: error.to_string(),
            }),
        }
    }

    if let Err(error) = state.save(paths) {
        progress::message(progress, format!("Note: status not saved ({error})"));
    }
    report
}

// ---------------------------------------------------------------------------
// Pinning, skipping and rollback
// ---------------------------------------------------------------------------

/// Pins an app to its installed tag, an explicit tag, or the latest known tag.
pub fn pin(app_id: &str, tag: Option<&str>, paths: &Paths) -> Result<String> {
    let _lock = OperationLock::acquire(paths)?;
    let app = catalog::by_id(app_id)
        .ok_or_else(|| Error::Other(format!("Unknown app: {app_id}")))?;
    let mut state = State::load(paths);
    state.reconcile(paths);
    let tag = tag
        .map(str::to_string)
        .or_else(|| state.installed.get(app.id).map(|installed| installed.tag.clone()))
        .or_else(|| {
            state
                .cached_release(app.id)
                .map(|cached| cached.plan.tag.clone())
        })
        .ok_or_else(|| Error::Other(format!("No version available to pin for {}", app.name)))?;
    state.pin(app.id, &tag);
    state.save(paths)?;
    Ok(tag)
}

pub fn unpin(app_id: &str, paths: &Paths) -> Result<bool> {
    let _lock = OperationLock::acquire(paths)?;
    let app = catalog::by_id(app_id)
        .ok_or_else(|| Error::Other(format!("Unknown app: {app_id}")))?;
    let mut state = State::load(paths);
    state.reconcile(paths);
    let existed = state.is_pinned(app.id);
    state.unpin(app.id);
    state.save(paths)?;
    Ok(existed)
}

/// Skips the latest known release (or an explicit tag) for future updates.
pub fn skip(app_id: &str, tag: Option<&str>, paths: &Paths) -> Result<String> {
    let _lock = OperationLock::acquire(paths)?;
    let app = catalog::by_id(app_id)
        .ok_or_else(|| Error::Other(format!("Unknown app: {app_id}")))?;
    let mut state = State::load(paths);
    state.reconcile(paths);
    let tag = tag
        .map(str::to_string)
        .or_else(|| {
            state
                .cached_release(app.id)
                .map(|cached| cached.plan.tag.clone())
        })
        .or_else(|| state.installed.get(app.id).map(|installed| installed.tag.clone()))
        .ok_or_else(|| Error::Other(format!("No version available to skip for {}", app.name)))?;
    state.skip(app.id, &tag);
    state.save(paths)?;
    Ok(tag)
}

pub fn unskip(app_id: &str, paths: &Paths) -> Result<bool> {
    let _lock = OperationLock::acquire(paths)?;
    let app = catalog::by_id(app_id)
        .ok_or_else(|| Error::Other(format!("Unknown app: {app_id}")))?;
    let mut state = State::load(paths);
    state.reconcile(paths);
    let existed = state.skipped.contains_key(app.id);
    state.unskip(app.id);
    state.save(paths)?;
    Ok(existed)
}

/// Activates an older version that is still on disk and pins it.
pub fn rollback(
    app_id: &str,
    target: Option<&str>,
    paths: &Paths,
    progress: ProgressFn<'_>,
) -> Result<InstallOutcome> {
    let _lock = OperationLock::acquire(paths)?;
    let app = catalog::by_id(app_id)
        .ok_or_else(|| Error::Other(format!("Unknown app: {app_id}")))?;
    let mut state = State::load(paths);
    state.reconcile(paths);
    let active = state
        .installed
        .get(app.id)
        .cloned()
        .ok_or_else(|| Error::NoRollback(format!("{} is not installed", app.name)))?;

    let entry = match target {
        Some(target) => state
            .find_version(app.id, target)
            .cloned()
            .ok_or_else(|| Error::NoRollback(format!("{} {target}", app.name)))?,
        None => state
            .rollback_target(app.id, &active.version)
            .cloned()
            .ok_or_else(|| Error::NoRollback(app.name.to_string()))?,
    };
    if entry.version == active.version {
        return Ok(InstallOutcome {
            app_id: app.id.to_string(),
            version: entry.version,
            already_installed: true,
            downloaded: false,
            vt: None,
        });
    }

    progress::message(
        progress,
        format!("Rolling {} back to {} …", app.name, entry.version),
    );
    let mut installed =
        install::activate_existing(app, &entry.dir, &entry.version, paths, progress)?;
    installed.tag = entry.tag.clone();
    installed.sha256 = entry.sha256.clone();
    installed.installed_at = now_unix();

    state.installed.insert(app.id.to_string(), installed.clone());
    state.record_history(app.id, &installed);
    // Keep the rolled-back version until the user explicitly unpins it.
    state.pin(app.id, &installed.tag);
    state.unskip(app.id);
    state.save(paths)?;
    progress::message(
        progress,
        format!("{} {} is active.", app.name, installed.version),
    );

    Ok(InstallOutcome {
        app_id: app.id.to_string(),
        version: installed.version,
        already_installed: false,
        downloaded: false,
        vt: None,
    })
}

// ---------------------------------------------------------------------------
// Misc operations
// ---------------------------------------------------------------------------

/// Removes an app and updates the state file.
pub fn uninstall(app_id: &str, paths: &Paths) -> Result<bool> {
    let _lock = OperationLock::acquire(paths)?;
    let app = catalog::by_id(app_id)
        .ok_or_else(|| Error::Other(format!("Unknown app: {app_id}")))?;
    let removed = install::uninstall(app, paths)?;
    // Drop any cached archives of this app as well.
    prune_download_cache(paths, app, "");
    let mut state = State::load(paths);
    state.reconcile(paths);
    state.installed.remove(app.id);
    state.history.remove(app.id);
    state.pins.remove(app.id);
    state.skipped.remove(app.id);
    state.save(paths)?;
    Ok(removed)
}

/// Sanity checks for the CLI's `doctor` command.
pub fn doctor(paths: &Paths, cfg: &Config) -> Vec<DoctorItem> {
    let mut items = Vec::new();

    let mut writable = true;
    if let Err(error) = paths.ensure() {
        writable = false;
        items.push(DoctorItem {
            name: "Directories".into(),
            ok: false,
            detail: error.to_string(),
        });
    }
    if writable {
        let probe = paths.cache_dir.join(".doctor-probe");
        let ok = std::fs::write(&probe, b"ok").is_ok();
        let _ = std::fs::remove_file(&probe);
        items.push(DoctorItem {
            name: "Write access".into(),
            ok,
            detail: paths.cache_dir.display().to_string(),
        });
    }

    items.push(DoctorItem {
        name: "VirusTotal API key".into(),
        ok: cfg.vt_key().is_some(),
        detail: if cfg.vt_key().is_some() {
            "configured".into()
        } else {
            format!("missing – add it to {}", paths.config_file().display())
        },
    });

    items.push(DoctorItem {
        name: "GitHub token (optional)".into(),
        ok: cfg.github_token().is_some(),
        detail: if cfg.github_token().is_some() {
            "configured (5,000 requests/hour)".into()
        } else {
            "not configured (60 requests/hour, feed fallback active)".into()
        },
    });

    let path_has_bin = std::env::var_os("PATH")
        .map(|path| std::env::split_paths(&path).any(|p| p == paths.bin_dir))
        .unwrap_or(false);
    items.push(DoctorItem {
        name: "~/.local/bin in PATH".into(),
        ok: path_has_bin,
        detail: if path_has_bin {
            "yes".into()
        } else {
            format!("no – add {}", paths.bin_dir.display())
        },
    });

    items.push(DoctorItem {
        name: "artcraftctl".into(),
        ok: find_in_path("artcraftctl").is_some(),
        detail: find_in_path("artcraftctl")
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "not in PATH (needed for the Omarchy plugin)".into()),
    });

    items.push(DoctorItem {
        name: "Omarchy plugin".into(),
        ok: paths.omarchy_plugin_dir().join("manifest.json").is_file(),
        detail: paths.omarchy_plugin_dir().display().to_string(),
    });

    let theme = crate::theme::Theme::load(paths);
    items.push(DoctorItem {
        name: "Omarchy theme".into(),
        ok: cfg.follow_omarchy_theme && theme.name.is_some(),
        detail: theme.name.unwrap_or_else(|| "built-in palette".into()),
    });

    items.push(DoctorItem {
        name: "GitHub attestations (gh)".into(),
        ok: find_in_path("gh").is_some(),
        detail: if find_in_path("gh").is_some() {
            if cfg.require_attestations {
                "installed (required)".into()
            } else {
                "installed (optional)".into()
            }
        } else if cfg.require_attestations {
            "missing, but attestations are required".into()
        } else {
            "missing (optional)".into()
        },
    });

    items.push(DoctorItem {
        name: "GPG".into(),
        ok: find_in_path("gpg").is_some(),
        detail: if find_in_path("gpg").is_some() {
            if cfg.gpg_keyring.is_empty() {
                "installed (default keyring)".into()
            } else {
                format!("installed (keyring: {})", cfg.gpg_keyring)
            }
        } else if cfg.require_gpg_signature || cfg.require_signed_manifest {
            "missing, but signatures are required".into()
        } else {
            "missing (optional)".into()
        },
    });

    for tool in ["update-desktop-database", "gtk-update-icon-cache"] {
        let found = find_in_path(tool);
        items.push(DoctorItem {
            name: tool.into(),
            ok: found.is_some(),
            detail: found
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| "optional, not found".into()),
        });
    }

    items
}

fn find_in_path(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(name))
        .find(|candidate| candidate.is_file())
}

/// Re-runs the VirusTotal check for an app, ignoring any cached verdict.
pub fn vt_recheck(
    app_id: &str,
    paths: &Paths,
    cfg: &Config,
    progress: ProgressFn<'_>,
) -> Result<VtResult> {
    let _lock = OperationLock::acquire(paths)?;
    let app = catalog::by_id(app_id)
        .ok_or_else(|| Error::Other(format!("Unknown app: {app_id}")))?;
    let mut state = State::load(paths);
    state.reconcile(paths);
    let now = now_unix();
    let plan = state
        .cached_plan(app.id, cfg.metadata_ttl_secs(), now)
        .cloned()
        .or_else(|| github::resolve(app, cfg).ok())
        .ok_or_else(|| Error::Other(format!("No release plan for {}", app.name)))?;
    let mut vt = VtClient::new(paths, cfg)?;
    vt.invalidate(&plan.sha256);
    let report = |text: &str| progress(Progress::Message(text.to_string()));
    vt.check(&plan.sha256, &plan.asset_url, &report)
}

/// Launches an installed app by running its binary.
pub fn open_app(app_id: &str, paths: &Paths) -> Result<()> {
    let app = catalog::by_id(app_id)
        .ok_or_else(|| Error::Other(format!("Unknown app: {app_id}")))?;
    let dir = installed_dir(paths, app_id)
        .ok_or_else(|| Error::Other(format!("{} is not installed.", app.name)))?;
    // Find the main binary inside the version dir.
    #[cfg(target_os = "windows")]
    let bin_dir = {
        if dir.join("bin").is_dir() {
            dir.join("bin")
        } else {
            // Portable zips ship the exe at the top level (or one subdir).
            let mut found = dir.clone();
            if let Ok(entries) = std::fs::read_dir(&dir) {
                for entry in entries.flatten() {
                    let p = entry.path();
                    if p.is_dir() && std::fs::read_dir(&p).map(|mut e| e.any(|x| x.map(|y| y.path().extension().map(|e| e == "exe").unwrap_or(false)).unwrap_or(false))).unwrap_or(false) {
                        found = p;
                        break;
                    }
                }
            }
            found
        }
    };
    #[cfg(not(target_os = "windows"))]
    let bin_dir = dir.join("bin");
    let binary = install::find_main_binary(&bin_dir, app)
        .map(|name| bin_dir.join(name))
        .unwrap_or_else(|| paths.bin_dir.join(app.id));
    if !binary.exists() {
        return Err(Error::Other(format!("Binary not found: {}", binary.display())));
    }
    std::process::Command::new(&binary)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map_err(|e| Error::Other(format!("Could not launch {}: {e}", app.name)))?;
    Ok(())
}

/// Convenience for the GUI: the version dir of an installed app.
pub fn installed_dir(paths: &Paths, id: &str) -> Option<PathBuf> {
    let state = State::load(paths);
    state.installed.get(id).map(|i| i.dir.clone()).or_else(|| {
        let mut state = state;
        state.reconcile(paths);
        state.installed.get(id).map(|i| i.dir.clone())
    })
}

/// Short label such as `0.13.0 → 0.14.0`, used by the CLI and the widget.
pub fn version_label(installed: Option<&str>, latest: Option<&str>) -> String {
    match (installed, latest) {
        (Some(i), Some(l)) => {
            let iv = ReleaseVersion::parse(i);
            let lv = ReleaseVersion::parse(l);
            if lv.is_newer_than(i) {
                format!("{iv} → {lv}")
            } else {
                format!("{iv}")
            }
        }
        (Some(i), None) => i.to_string(),
        (None, Some(l)) => format!("not installed → {l}"),
        (None, None) => "–".to_string(),
    }
}

/// Checks whether a path is inside the hub's install root.
pub fn is_managed(paths: &Paths, path: &Path) -> bool {
    paths.is_managed_path(path)
}

/// Removes cached download archives of `app` that are no longer the current
/// version, so `~/.cache/artcraft-hub/downloads/` cannot grow without bound.
pub fn prune_download_cache(paths: &Paths, app: &AppDef, keep: &str) {
    let dir = paths.download_dir();
    let Ok(entries) = fs::read_dir(&dir) else {
        return;
    };
    let mut stems: Vec<&str> = vec![app.id];
    stems.extend(app.stems.iter().copied());
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if name == keep || !name.ends_with(".tar.gz") {
            continue;
        }
        if !stems.iter().any(|stem| name.starts_with(&format!("{stem}-"))) {
            continue;
        }
        let _ = fs::remove_file(&path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_labels_are_readable() {
        assert_eq!(version_label(Some("0.13.0"), Some("0.14.0")), "0.13.0 → 0.14.0");
        assert_eq!(version_label(Some("0.14.0"), Some("0.13.0")), "0.14.0");
        assert_eq!(version_label(None, Some("1.0.0")), "not installed → 1.0.0");
    }
}
