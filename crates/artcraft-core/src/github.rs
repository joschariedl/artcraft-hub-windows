//! Release discovery and checksum handling.
//!
//! Two sources are used: GitHub's API (authoritative, but unauthenticated
//! callers only get 60 requests/hour) and the Storyteller release feed, which
//! mirrors the same GitHub releases in one request. Download links and
//! `SHA256SUMS.txt` always come from GitHub itself; the feed is only a hint.
//!
//! Every URL and file name below is treated as untrusted input.

use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::catalog::{AppDef, release_arch};
use crate::config::Config;
use crate::error::{Error, Result};
use crate::net;
use crate::time;
use crate::version::ReleaseVersion;

pub const GITHUB_API: &str = "https://api.github.com";
pub const FEED_URL: &str = "https://api.storyteller.ai/v1/craft_apps/release_info";
pub const SUMS_NAME: &str = "SHA256SUMS.txt";

const MAX_JSON: u64 = 4 * 1024 * 1024;
const MAX_SUMS: u64 = 1024 * 1024;
const MAX_NOTES: usize = 64 * 1024;

/// One downloadable file of a release.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Asset {
    pub name: String,
    pub size: u64,
    /// `https://github.com/<repo>/releases/download/<tag>/<name>`.
    pub url: String,
    /// Lowercase hex SHA-256, when GitHub or the feed told us.
    pub sha256: Option<String>,
}

/// A published release of one app.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Release {
    pub tag: String,
    /// Tag without a leading `v`, e.g. `0.13.0`.
    pub version: String,
    pub name: String,
    pub published_at: Option<i64>,
    pub html_url: String,
    pub notes: String,
    pub prerelease: bool,
    pub assets: Vec<Asset>,
}

/// Everything needed to verify and install one app version.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ReleasePlan {
    pub app_id: String,
    pub tag: String,
    pub version: String,
    pub asset_name: String,
    pub asset_url: String,
    pub asset_size: u64,
    /// The checksum the artifact must have.
    pub sha256: String,
    /// Checksum of `SHA256SUMS.txt` itself, as published by GitHub.
    pub sums_sha256: Option<String>,
    /// The digest GitHub attached to the artifact, when it did.
    pub api_sha256: Option<String>,
    pub release_url: String,
    pub published_at: Option<i64>,
    pub notes: String,
    pub prerelease: bool,
}

impl ReleasePlan {
    pub fn version(&self) -> ReleaseVersion {
        ReleaseVersion::parse(&self.tag)
    }

    /// Download URL of the checksum list for this release.
    pub fn sums_url(&self, repo: &str) -> String {
        format!("{}{}/{}", download_prefix(repo), self.tag, SUMS_NAME)
    }
}

/// Result of a conditional release lookup.
#[derive(Clone, Debug)]
pub struct ResolveOutcome {
    pub plan: ReleasePlan,
    /// Validators to store for the next conditional request.
    pub etag: Option<String>,
    pub last_modified: Option<String>,
    /// The server confirmed that the cached metadata is still current.
    pub not_modified: bool,
}

/// A parsed `SHA256SUMS.txt`.
#[derive(Clone, Debug, Default)]
pub struct Sums {
    pub entries: Vec<(String, String)>,
    /// SHA-256 of the checksum file itself.
    pub file_sha256: String,
}

impl Sums {
    pub fn find(&self, name: &str) -> Option<&str> {
        self.entries
            .iter()
            .find(|(_, n)| n == name)
            .map(|(h, _)| h.as_str())
    }
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// The latest release of `app`, from GitHub with a feed fallback.
pub fn latest(app: &AppDef, cfg: &Config) -> Result<Release> {
    let agent = net::agent(Duration::from_secs(45));
    let attempts = cfg.network_attempts();
    let from_api = match fetch_github_meta(&agent, app, cfg, None, None, attempts) {
        Ok(GithubMeta::Release { release, .. }) => Ok(*release),
        // Without validators the server cannot answer 304, but treat it as a
        // failed lookup rather than panicking.
        Ok(GithubMeta::NotModified) => Err(Error::Other("unexpected HTTP 304".into())),
        Err(error) => Err(error),
    };
    match from_api {
        Ok(release) => Ok(release),
        Err(api_error) => match fetch_feed(&agent, app, attempts) {
            Ok(release) => Ok(release),
            Err(feed_error) => Err(match api_error {
                // Rate limiting is the actionable cause; the feed often lacks
                // individual apps (e.g. FigCraft), which would otherwise mask it.
                Error::RateLimited => Error::RateLimited,
                Error::Http(_) => feed_error,
                other => Error::Other(format!("{other} (Feed: {feed_error})")),
            }),
        },
    }
}

/// Resolves the latest release down to a verifiable download plan.
pub fn resolve(app: &AppDef, cfg: &Config) -> Result<ReleasePlan> {
    Ok(resolve_conditional(app, cfg, None, None, None)?.plan)
}

/// Resolves the latest release, reusing cached metadata when the server
/// confirms it is still current (HTTP 304 via `ETag` / `Last-Modified`).
pub fn resolve_conditional(
    app: &AppDef,
    cfg: &Config,
    cached_plan: Option<&ReleasePlan>,
    etag: Option<&str>,
    last_modified: Option<&str>,
) -> Result<ResolveOutcome> {
    let agent = net::agent(Duration::from_secs(45));
    let attempts = cfg.network_attempts();

    match fetch_github_meta(&agent, app, cfg, etag, last_modified, attempts) {
        Ok(GithubMeta::NotModified) => {
            let Some(cached) = cached_plan else {
                // We sent no validators (or the server ignored them); retry
                // unconditionally instead of failing.
                return resolve_conditional(app, cfg, None, None, None);
            };
            Ok(ResolveOutcome {
                plan: cached.clone(),
                etag: etag.map(str::to_string),
                last_modified: last_modified.map(str::to_string),
                not_modified: true,
            })
        }
        Ok(GithubMeta::Release {
            release,
            etag: new_etag,
            last_modified: new_last_modified,
        }) => {
            let plan = match cached_plan.filter(|plan| plan.tag == release.tag) {
                Some(cached) => cached.clone(),
                None => plan_from_release_with_agent(&agent, app, &release, cfg, attempts)?,
            };
            Ok(ResolveOutcome {
                plan,
                etag: new_etag,
                last_modified: new_last_modified,
                not_modified: false,
            })
        }
        Err(api_error) => match fetch_feed(&agent, app, attempts) {
            Ok(release) => {
                let plan = match cached_plan.filter(|plan| plan.tag == release.tag) {
                    Some(cached) => cached.clone(),
                    None => plan_from_release_with_agent(&agent, app, &release, cfg, attempts)?,
                };
                Ok(ResolveOutcome {
                    plan,
                    etag: None,
                    last_modified: None,
                    not_modified: false,
                })
            }
            Err(feed_error) => Err(match api_error {
                Error::RateLimited => Error::RateLimited,
                Error::Http(_) => feed_error,
                other => Error::Other(format!("{other} (Feed: {feed_error})")),
            }),
        },
    }
}

/// Resolves one explicitly requested tag (for version pinning and installs).
pub fn resolve_tag(app: &AppDef, tag: &str, cfg: &Config) -> Result<ReleasePlan> {
    if !valid_tag(tag) {
        return Err(Error::InvalidUrl(format!("invalid release tag {tag:?}")));
    }
    let agent = net::agent(Duration::from_secs(45));
    let attempts = cfg.network_attempts();
    let url = format!("{GITHUB_API}/repos/{}/releases/tags/{tag}", app.repo);
    let token = cfg.github_token().unwrap_or_default();
    let mut headers: Vec<(&str, &str)> = vec![
        ("Accept", "application/vnd.github+json"),
        ("X-GitHub-Api-Version", "2022-11-28"),
    ];
    if !token.is_empty() {
        headers.push(("Authorization", &token));
    }
    let reply = net::get_retry(&agent, &url, &headers, MAX_JSON, attempts)?;
    match reply.status {
        200 => {
            let release = parse_github_release(&reply.body_text(), app.repo)?;
            if release.tag != tag {
                return Err(Error::NoRelease(format!("{} {tag}", app.name)));
            }
            plan_from_release_with_agent(&agent, app, &release, cfg, attempts)
        }
        403 | 429 => {
            // The feed only mirrors the latest release, but that is still
            // better than failing for the most common explicit version.
            match fetch_feed(&agent, app, attempts) {
                Ok(release) if release.tag == tag => {
                    plan_from_release_with_agent(&agent, app, &release, cfg, attempts)
                }
                _ => Err(Error::RateLimited),
            }
        }
        404 => Err(Error::NoRelease(format!("{} {tag}", app.name))),
        status => Err(Error::Http(format!(
            "GitHub replied for {} {tag} with HTTP {status}",
            app.repo
        ))),
    }
}

/// Fetches `SHA256SUMS.txt` on top of an already known release.
pub fn plan_from_release(app: &AppDef, release: &Release, cfg: &Config) -> Result<ReleasePlan> {
    let agent = net::agent(Duration::from_secs(45));
    plan_from_release_with_agent(&agent, app, release, cfg, cfg.network_attempts())
}

fn plan_from_release_with_agent(
    agent: &ureq::Agent,
    app: &AppDef,
    release: &Release,
    cfg: &Config,
    attempts: u32,
) -> Result<ReleasePlan> {
    let sums = match fetch_sums_with_agent(agent, app.repo, &release.tag, attempts) {
        Ok(sums) => Some(sums),
        Err(Error::MissingChecksums(tag)) if !cfg.require_sha256sums => {
            let _ = tag;
            None
        }
        Err(error) => return Err(error),
    };
    plan_from_parts(app, release, sums.as_ref(), cfg)
}

/// Pure planning step; separated so tests can exercise the checksum rules.
pub fn plan_from_parts(
    app: &AppDef,
    release: &Release,
    sums: Option<&Sums>,
    cfg: &Config,
) -> Result<ReleasePlan> {
    if !valid_tag(&release.tag) {
        return Err(Error::InvalidUrl(format!(
            "invalid release tag {:?}",
            release.tag
        )));
    }
    let asset = select_asset(app, release)?;

    // The checksum list itself is verified against GitHub's asset digest, so
    // its contents cannot be swapped for a different list.
    if let Some(sums) = sums {
        if let Some(published) = release
            .assets
            .iter()
            .find(|a| a.name == SUMS_NAME)
            .and_then(|a| a.sha256.as_deref())
        {
            if !published.eq_ignore_ascii_case(&sums.file_sha256) {
                return Err(Error::ChecksumConflict(format!(
                    "{}: GitHub digest {} ≠ downloaded file {}",
                    SUMS_NAME, published, sums.file_sha256
                )));
            }
        }
    }

    let listed = sums.and_then(|s| s.find(&asset.name)).map(str::to_string);
    let expected = match (listed, asset.sha256.clone()) {
        (Some(listed), Some(api)) => {
            if !listed.eq_ignore_ascii_case(&api) {
                return Err(Error::ChecksumConflict(format!(
                    "{}: SHA256SUMS.txt {} ≠ GitHub-Digest {}",
                    asset.name, listed, api
                )));
            }
            listed
        }
        (Some(listed), None) => listed,
        (None, Some(api)) if !cfg.require_sha256sums => api,
        (None, _) if cfg.require_sha256sums => {
            return Err(Error::ChecksumNotListed(asset.name.clone()));
        }
        (None, _) => {
            return Err(Error::MissingChecksums(release.tag.clone()));
        }
    };

    if !is_hex64(&expected) {
        return Err(Error::ChecksumNotListed(asset.name.clone()));
    }

    Ok(ReleasePlan {
        app_id: app.id.to_string(),
        tag: release.tag.clone(),
        version: release.version.clone(),
        asset_name: asset.name.clone(),
        asset_url: asset.url.clone(),
        asset_size: asset.size,
        sha256: expected.to_ascii_lowercase(),
        sums_sha256: sums.map(|s| s.file_sha256.clone()),
        api_sha256: asset.sha256.clone(),
        release_url: release.html_url.clone(),
        published_at: release.published_at,
        notes: release.notes.clone(),
        prerelease: release.prerelease,
    })
}

/// Downloads `SHA256SUMS.txt` for `repo`/`tag` from GitHub.
pub fn fetch_sums(agent: &ureq::Agent, repo: &str, tag: &str) -> Result<Sums> {
    fetch_sums_with_agent(agent, repo, tag, 1)
}

fn fetch_sums_with_agent(
    agent: &ureq::Agent,
    repo: &str,
    tag: &str,
    attempts: u32,
) -> Result<Sums> {
    let body = fetch_sums_bytes(agent, repo, tag, attempts)?;
    let file_sha256 = sha256_hex(&body);
    let text = String::from_utf8_lossy(&body).to_string();
    let entries = parse_sha256sums(&text);
    if entries.is_empty() {
        return Err(Error::MissingChecksums(format!(
            "{tag} (file contains no valid lines)"
        )));
    }
    Ok(Sums {
        entries,
        file_sha256,
    })
}

/// Downloads the raw `SHA256SUMS.txt` bytes, used for signature checks.
pub fn fetch_sums_bytes(
    agent: &ureq::Agent,
    repo: &str,
    tag: &str,
    attempts: u32,
) -> Result<Vec<u8>> {
    if !valid_tag(tag) {
        return Err(Error::InvalidUrl(format!("invalid release tag {tag:?}")));
    }
    let url = format!("{}{}/{}", download_prefix(repo), tag, SUMS_NAME);
    let reply = net::get_retry(
        agent,
        &url,
        &[("Accept", "text/plain, */*")],
        MAX_SUMS,
        attempts,
    )?;
    if reply.status == 404 {
        return Err(Error::MissingChecksums(tag.to_string()));
    }
    if !reply.is_success() {
        return Err(Error::Http(format!(
            "SHA256SUMS.txt for {tag}: HTTP {}",
            reply.status
        )));
    }
    Ok(reply.body)
}

/// Parses `sha256sum` output: `<hex>  <name>` (or `<hex> *<name>`).
pub fn parse_sha256sums(text: &str) -> Vec<(String, String)> {
    text.lines()
        .filter_map(|line| {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                return None;
            }
            let (hash, rest) = line.split_once(char::is_whitespace)?;
            let name = rest.trim_start().trim_start_matches('*').trim();
            (is_hex64(hash) && valid_file_name(name))
                .then(|| (hash.to_ascii_lowercase(), name.to_string()))
        })
        .collect()
}

/// Picks the `linux-<arch>.tar.gz` asset for `app`.
pub fn select_asset<'a>(app: &AppDef, release: &'a Release) -> Result<&'a Asset> {
    #[cfg(target_os = "windows")]
    {
        let arch = match std::env::consts::ARCH {
            "x86_64" => "x64",
            "aarch64" => "arm64",
            "x86" => "x86",
            other => other,
        };
        let mut stems = vec![app.id];
        stems.extend(app.stems.iter().copied());
        let candidates = [
            format!("-windows-{arch}-portable.zip"),
            format!("-windows-{arch}.msi"),
        ];
        for stem in &stems {
            for cand in &candidates {
                let expected = format!("{stem}-{}{}", release.version, cand);
                if let Some(asset) = release
                    .assets
                    .iter()
                    .find(|a| a.name == expected && valid_download_url(&a.url, app.repo, &release.tag, &a.name))
                {
                    return Ok(asset);
                }
            }
        }
    }

    let arch = release_arch();
    let suffix = format!("-linux-{arch}.tar.gz");
    let mut stems = vec![app.id];
    stems.extend(app.stems.iter().copied());

    for stem in &stems {
        let expected = format!("{stem}-{}{suffix}", release.version);
        if let Some(asset) = release
            .assets
            .iter()
            .find(|a| a.name == expected && valid_download_url(&a.url, app.repo, &release.tag, &a.name))
        {
            return Ok(asset);
        }
    }

    // Older or oddly named builds: accept an unambiguous stem match, but never
    // a CLI/Web bundle.
    let mut found: Vec<&Asset> = release
        .assets
        .iter()
        .filter(|a| {
            a.name.ends_with(&suffix)
                && !a.name.contains("-cli-")
                && !a.name.contains("-web-")
                && stems.iter().any(|stem| a.name.starts_with(&format!("{stem}-")))
                && valid_download_url(&a.url, app.repo, &release.tag, &a.name)
        })
        .collect();
    found.sort_by(|a, b| a.name.cmp(&b.name));
    match found.len() {
        1 => Ok(found[0]),
        0 => Err(Error::MissingAsset(format!(
            "{} for {} (expected *{suffix})",
            app.id, release.tag
        ))),
        _ => Err(Error::MissingAsset(format!(
            "{} for {} is ambiguous: {}",
            app.id,
            release.tag,
            found
                .iter()
                .map(|a| a.name.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ))),
    }
}

/// Candidate raw GitHub URLs for the 1024 px app icon.
pub fn icon_urls(app: &AppDef, tag: &str) -> Vec<String> {
    let mut out = Vec::new();
    for reference in [tag, "HEAD"] {
        if !valid_tag(reference) && reference != "HEAD" {
            continue;
        }
        for path in app.icon_paths() {
            out.push(format!(
                "https://raw.githubusercontent.com/{}/{reference}/{path}",
                app.repo
            ));
        }
    }
    out
}

// ---------------------------------------------------------------------------
// GitHub API
// ---------------------------------------------------------------------------

enum GithubMeta {
    NotModified,
    Release {
        release: Box<Release>,
        etag: Option<String>,
        last_modified: Option<String>,
    },
}

fn fetch_github_meta(
    agent: &ureq::Agent,
    app: &AppDef,
    cfg: &Config,
    etag: Option<&str>,
    last_modified: Option<&str>,
    attempts: u32,
) -> Result<GithubMeta> {
    let url = format!("{GITHUB_API}/repos/{}/releases/latest", app.repo);
    let token = cfg.github_token().unwrap_or_default();
    let mut headers: Vec<(&str, &str)> = vec![
        ("Accept", "application/vnd.github+json"),
        ("X-GitHub-Api-Version", "2022-11-28"),
    ];
    if let Some(etag) = etag {
        headers.push(("If-None-Match", etag));
    }
    if let Some(last_modified) = last_modified {
        headers.push(("If-Modified-Since", last_modified));
    }
    if !token.is_empty() {
        headers.push(("Authorization", &token));
    }
    let reply = net::get_retry(agent, &url, &headers, MAX_JSON, attempts)?;
    match reply.status {
        304 => Ok(GithubMeta::NotModified),
        200 => Ok(GithubMeta::Release {
            release: Box::new(parse_github_release(&reply.body_text(), app.repo)?),
            etag: reply.etag,
            last_modified: reply.last_modified,
        }),
        403 | 429 => Err(Error::RateLimited),
        404 => Err(Error::NoRelease(app.name.to_string())),
        status => Err(Error::Http(format!(
            "GitHub replied for {} with HTTP {status}",
            app.repo
        ))),
    }
}

/// Parses `GET /repos/<repo>/releases/latest`.
pub fn parse_github_release(json: &str, repo: &str) -> Result<Release> {
    let value: Value = serde_json::from_str(json)?;
    release_from_value(&value, repo)
}

fn release_from_value(value: &Value, repo: &str) -> Result<Release> {
    if let Some(message) = value.get("message").and_then(Value::as_str) {
        return Err(Error::Other(format!(
            "GitHub: {}",
            truncate(message, 200)
        )));
    }
    let tag = value
        .get("tag_name")
        .and_then(Value::as_str)
        .filter(|t| valid_tag(t))
        .ok_or_else(|| Error::NoRelease(repo.to_string()))?
        .to_string();
    let version = ReleaseVersion::parse(&tag).raw;
    let prefix = download_prefix(repo);

    let assets = value
        .get("assets")
        .and_then(Value::as_array)
        .map(|list| {
            list.iter()
                .take(400)
                .filter_map(|asset| {
                    let name = asset.get("name").and_then(Value::as_str)?;
                    if !valid_file_name(name) {
                        return None;
                    }
                    let url = asset.get("browser_download_url").and_then(Value::as_str)?;
                    if url != format!("{prefix}{tag}/{name}") {
                        return None;
                    }
                    let sha256 = asset
                        .get("digest")
                        .and_then(Value::as_str)
                        .and_then(|d| d.strip_prefix("sha256:"))
                        .filter(|h| is_hex64(h))
                        .map(str::to_ascii_lowercase);
                    Some(Asset {
                        name: name.to_string(),
                        size: asset.get("size").and_then(Value::as_u64).unwrap_or(0),
                        url: url.to_string(),
                        sha256,
                    })
                })
                .collect()
        })
        .unwrap_or_default();

    let releases_url = format!("https://github.com/{repo}/releases");
    let html_url = value
        .get("html_url")
        .and_then(Value::as_str)
        .filter(|u| safe_release_url(repo, u))
        .unwrap_or(&releases_url)
        .to_string();

    Ok(Release {
        name: value
            .get("name")
            .and_then(Value::as_str)
            .map(|n| truncate(n, 120))
            .filter(|n| !n.is_empty())
            .unwrap_or_else(|| tag.clone()),
        tag,
        version,
        published_at: value
            .get("published_at")
            .and_then(Value::as_str)
            .and_then(time::parse_rfc3339),
        html_url,
        notes: value
            .get("body")
            .and_then(Value::as_str)
            .map(|b| truncate(b, MAX_NOTES))
            .unwrap_or_default(),
        prerelease: value
            .get("prerelease")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        assets,
    })
}

// ---------------------------------------------------------------------------
// Storyteller feed
// ---------------------------------------------------------------------------

fn fetch_feed(agent: &ureq::Agent, app: &AppDef, attempts: u32) -> Result<Release> {
    let reply = net::get_retry(
        agent,
        FEED_URL,
        &[("Accept", "application/json")],
        MAX_JSON,
        attempts,
    )?;
    if !reply.is_success() {
        return Err(Error::Http(format!(
            "Release feed replied with HTTP {}",
            reply.status
        )));
    }
    parse_feed(&reply.body_text(), app).ok_or_else(|| {
        Error::NoRelease(format!("{} (neither GitHub nor the feed)", app.name))
    })
}

/// Reads one app's entry from the Storyteller feed. Only releases whose
/// download links point at the app's own GitHub repository are accepted.
pub fn parse_feed(json: &str, app: &AppDef) -> Option<Release> {
    let value: Value = serde_json::from_str(json).ok()?;
    let release_info = value.get("release_info")?;
    let entry = release_info.get("apps")?.get(app.id)?;

    if let Some(repo) = entry.get("repo").and_then(Value::as_str) {
        if !repo.eq_ignore_ascii_case(app.repo) {
            return None;
        }
    }

    let raw = entry.get("latest_release")?;
    if raw.is_null() {
        return None;
    }
    let tag = raw
        .get("tag")
        .and_then(Value::as_str)
        .filter(|t| valid_tag(t))?
        .to_string();
    let version = ReleaseVersion::parse(&tag).raw;
    let prefix = download_prefix(app.repo);

    let assets = raw
        .get("assets")
        .and_then(Value::as_array)
        .map(|list| {
            list.iter()
                .take(400)
                .filter_map(|asset| {
                    let name = asset.get("name").and_then(Value::as_str)?;
                    let url = asset.get("download_url").and_then(Value::as_str)?;
                    if !valid_file_name(name) || url != format!("{prefix}{tag}/{name}") {
                        return None;
                    }
                    Some(Asset {
                        name: name.to_string(),
                        size: asset.get("size_bytes").and_then(Value::as_u64).unwrap_or(0),
                        url: url.to_string(),
                        sha256: asset
                            .get("sha256")
                            .and_then(Value::as_str)
                            .filter(|h| is_hex64(h))
                            .map(str::to_ascii_lowercase),
                    })
                })
                .collect()
        })
        .unwrap_or_default();

    let releases_url = format!("https://github.com/{}/releases", app.repo);
    let html_url = raw
        .get("html_url")
        .and_then(Value::as_str)
        .filter(|u| safe_release_url(app.repo, u))
        .unwrap_or(&releases_url)
        .to_string();

    Some(Release {
        name: raw
            .get("name")
            .and_then(Value::as_str)
            .map(|n| truncate(n, 120))
            .filter(|n| !n.is_empty())
            .unwrap_or_else(|| tag.clone()),
        tag,
        version,
        published_at: raw
            .get("published_at")
            .and_then(Value::as_str)
            .and_then(time::parse_rfc3339),
        html_url,
        notes: raw
            .get("notes_markdown")
            .and_then(Value::as_str)
            .map(|b| truncate(b, MAX_NOTES))
            .unwrap_or_default(),
        prerelease: raw
            .get("prerelease")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        assets,
    })
}

// ---------------------------------------------------------------------------
// Validation helpers
// ---------------------------------------------------------------------------

pub fn download_prefix(repo: &str) -> String {
    format!("https://github.com/{repo}/releases/download/")
}

/// True when `url` is exactly `<prefix><tag>/<name>` without traversal tricks.
pub fn valid_download_url(url: &str, repo: &str, tag: &str, name: &str) -> bool {
    if !valid_tag(tag) || !valid_file_name(name) {
        return false;
    }
    url == format!("{}{tag}/{name}", download_prefix(repo))
        && !url.contains(['?', '#', '\\'])
}

/// Release page URL must live under the app's repository.
pub fn safe_release_url(repo: &str, url: &str) -> bool {
    let releases = format!("https://github.com/{repo}/releases");
    if url == releases {
        return true;
    }
    let Some(rest) = url.strip_prefix(&releases) else {
        return false;
    };
    let Some(path) = rest.strip_prefix('/') else {
        return false;
    };
    !path.is_empty()
        && url.len() <= 500
        && !url.contains(['?', '#', '\\'])
        && path.split('/').all(|seg| !seg.is_empty() && seg != "." && seg != "..")
}

/// A tag that can be used as a single URL path segment.
pub fn valid_tag(tag: &str) -> bool {
    !tag.is_empty()
        && tag.len() <= 64
        && tag
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-' | b'+'))
        && tag != "."
        && tag != ".."
}

/// A bare file name we would write to disk.
pub fn valid_file_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 200
        && name != "."
        && name != ".."
        && !name.starts_with('.')
        && !name
            .chars()
            .any(|c| c.is_control() || matches!(c, '/' | '\\' | ':' | '<' | '>' | '"' | '|' | '?' | '*'))
}

pub fn is_hex64(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|b| b.is_ascii_hexdigit())
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(bytes);
    hex::encode(digest)
}

pub fn truncate(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_string();
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &text[..end])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog;

    const HASH: &str = "8a5edab282632443219e051e4ade2d1d5bbc671c781051bf1437897cbdfea0f1";

    fn app(id: &str) -> &'static AppDef {
        catalog::by_id(id).unwrap()
    }

    #[test]
    fn parses_sums_lines() {
        let text = "\
20c6c56ec1ffd6bbcffee6cdee58cf6b5cadd2f4a04a4360dd7cc64d53ad97e5  gridcraft-0.13.0-linux-x86_64.tar.gz
# comment
not-a-hash  file.tar.gz
deadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeef *other.tar.gz
";
        let sums = parse_sha256sums(text);
        assert_eq!(sums.len(), 2);
        assert_eq!(sums[0].1, "gridcraft-0.13.0-linux-x86_64.tar.gz");
        assert_eq!(sums[1].1, "other.tar.gz");
    }

    #[test]
    fn rejects_traversing_and_foreign_urls() {
        let repo = "storytold/gridcraft";
        let tag = "v0.13.0";
        let name = "gridcraft-0.13.0-linux-x86_64.tar.gz";
        assert!(valid_download_url(
            &format!("{}{tag}/{name}", download_prefix(repo)),
            repo,
            tag,
            name
        ));
        assert!(!valid_download_url(
            "https://github.com/storytold/gridcraft/releases/download/v0.13.0/../../../evil/releases/download/v1/x.tar.gz",
            repo,
            tag,
            name
        ));
        assert!(!valid_download_url(
            "https://evil.example/gridcraft.tar.gz",
            repo,
            tag,
            name
        ));
        assert!(!safe_release_url(repo, "https://github.com/other/x/releases"));
        assert!(safe_release_url(
            repo,
            "https://github.com/storytold/gridcraft/releases/tag/v0.13.0"
        ));
    }

    #[test]
    fn reads_github_release_and_drops_foreign_assets() {
        let json = format!(
            r#"{{
              "tag_name": "v0.13.0",
              "name": "GridCraft 0.13.0",
              "published_at": "2026-10-09T10:00:00Z",
              "html_url": "https://github.com/storytold/gridcraft/releases/tag/v0.13.0",
              "prerelease": false,
              "body": "Notes",
              "assets": [
                {{"name": "gridcraft-0.13.0-linux-x86_64.tar.gz", "size": 123,
                  "browser_download_url": "https://github.com/storytold/gridcraft/releases/download/v0.13.0/gridcraft-0.13.0-linux-x86_64.tar.gz",
                  "digest": "sha256:{HASH}"}},
                {{"name": "evil.tar.gz", "size": 1,
                  "browser_download_url": "https://evil.example/evil.tar.gz", "digest": null}}
              ]
            }}"#
        );
        let release = parse_github_release(&json, "storytold/gridcraft").unwrap();
        assert_eq!(release.version, "0.13.0");
        assert_eq!(release.published_at, Some(1_791_540_000));
        assert_eq!(release.assets.len(), 1);
        assert_eq!(release.assets[0].sha256.as_deref(), Some(HASH));
        assert_eq!(release.assets[0].size, 123);
    }

    #[test]
    fn picks_the_linux_tarball_and_ignores_cli_web_and_other_arches() {
        let release = Release {
            tag: "v0.13.0".into(),
            version: "0.13.0".into(),
            assets: vec![
                Asset {
                    name: "gridcraft-0.13.0-linux-aarch64.tar.gz".into(),
                    url: "https://github.com/storytold/gridcraft/releases/download/v0.13.0/gridcraft-0.13.0-linux-aarch64.tar.gz".into(),
                    ..Default::default()
                },
                Asset {
                    name: "gridcraft-cli-0.13.0-linux-x86_64.tar.gz".into(),
                    url: "https://github.com/storytold/gridcraft/releases/download/v0.13.0/gridcraft-cli-0.13.0-linux-x86_64.tar.gz".into(),
                    ..Default::default()
                },
                Asset {
                    name: "gridcraft-0.13.0-linux-x86_64.tar.gz".into(),
                    url: "https://github.com/storytold/gridcraft/releases/download/v0.13.0/gridcraft-0.13.0-linux-x86_64.tar.gz".into(),
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        let asset = select_asset(app("gridcraft"), &release).unwrap();
        assert_eq!(asset.name, "gridcraft-0.13.0-linux-x86_64.tar.gz");
    }

    #[test]
    fn pdfcraft_accepts_the_old_printcraft_stem() {
        let name = format!(
            "printcraft-0.2.1-linux-{}.tar.gz",
            crate::catalog::release_arch()
        );
        let release = Release {
            tag: "v0.2.1".into(),
            version: "0.2.1".into(),
            assets: vec![Asset {
                name: name.clone(),
                url: format!(
                    "https://github.com/storytold/pdfcraft/releases/download/v0.2.1/{name}"
                ),
                ..Default::default()
            }],
            ..Default::default()
        };
        assert_eq!(select_asset(app("pdfcraft"), &release).unwrap().name, name);
    }

    #[test]
    fn plan_requires_agreeing_checksums() {
        let name = format!(
            "gridcraft-0.13.0-linux-{}.tar.gz",
            crate::catalog::release_arch()
        );
        let release = Release {
            tag: "v0.13.0".into(),
            version: "0.13.0".into(),
            html_url: "https://github.com/storytold/gridcraft/releases/tag/v0.13.0".into(),
            assets: vec![
                Asset {
                    name: name.clone(),
                    url: format!(
                        "https://github.com/storytold/gridcraft/releases/download/v0.13.0/{name}"
                    ),
                    sha256: Some(HASH.into()),
                    ..Default::default()
                },
                Asset {
                    name: SUMS_NAME.into(),
                    url: format!(
                        "https://github.com/storytold/gridcraft/releases/download/v0.13.0/{SUMS_NAME}"
                    ),
                    sha256: Some("a".repeat(64)),
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        let sums = Sums {
            entries: vec![(HASH.to_string(), name.clone())],
            file_sha256: "a".repeat(64),
        };
        let cfg = Config::default();
        let plan = plan_from_parts(app("gridcraft"), &release, Some(&sums), &cfg).unwrap();
        assert_eq!(plan.sha256, HASH);
        assert_eq!(plan.sums_sha256.as_deref(), Some("a".repeat(64).as_str()));

        // A different checksum in the list is a hard failure.
        let mut bad = sums.clone();
        bad.entries[0].0 = "b".repeat(64);
        assert!(matches!(
            plan_from_parts(app("gridcraft"), &release, Some(&bad), &cfg),
            Err(Error::ChecksumConflict(_))
        ));

        // A modified checksum file is a hard failure, too.
        let mut tampered = sums;
        tampered.file_sha256 = "c".repeat(64);
        assert!(matches!(
            plan_from_parts(app("gridcraft"), &release, Some(&tampered), &cfg),
            Err(Error::ChecksumConflict(_))
        ));
    }

    #[test]
    fn reads_the_storyteller_feed_but_only_for_the_right_repo() {
        let json = format!(
            r#"{{"success": true, "release_info": {{"apps": {{
              "gridcraft": {{
                "repo": "storytold/gridcraft",
                "app_last_fetched": "2026-10-09T13:00:00Z",
                "latest_release": {{
                  "tag": "v0.13.0", "name": "GridCraft 0.13.0",
                  "published_at": "2026-10-09T10:00:00Z",
                  "html_url": "https://github.com/storytold/gridcraft/releases/tag/v0.13.0",
                  "prerelease": false, "notes_markdown": "Notes",
                  "assets": [
                    {{"name": "gridcraft-0.13.0-linux-x86_64.tar.gz", "size_bytes": 123,
                      "download_url": "https://github.com/storytold/gridcraft/releases/download/v0.13.0/gridcraft-0.13.0-linux-x86_64.tar.gz",
                      "sha256": "{HASH}"}},
                    {{"name": "evil.tar.gz", "size_bytes": 1,
                      "download_url": "https://evil.example/evil.tar.gz", "sha256": null}}
                  ]
                }}
              }}
            }}}}}}"#
        );
        let release = parse_feed(&json, app("gridcraft")).unwrap();
        assert_eq!(release.tag, "v0.13.0");
        assert_eq!(release.assets.len(), 1);
        assert_eq!(release.assets[0].sha256.as_deref(), Some(HASH));

        let wrong = json.replace(
            "\"repo\": \"storytold/gridcraft\"",
            "\"repo\": \"attacker/gridcraft\"",
        );
        assert!(parse_feed(&wrong, app("gridcraft")).is_none());
        assert!(parse_feed("{}", app("gridcraft")).is_none());
    }

    #[test]
    fn tags_must_be_url_safe() {
        assert!(valid_tag("v0.13.0"));
        assert!(!valid_tag("../../etc"));
        assert!(!valid_tag("v1.0/evil"));
        assert!(!valid_tag(""));
    }
}
