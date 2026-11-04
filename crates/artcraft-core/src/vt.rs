//! VirusTotal verification.
//!
//! The public API cannot accept 40–130 MB uploads, so the hub never uploads the
//! artifact. Instead it
//!
//! 1. looks up the SHA-256 in VirusTotal's file database,
//! 2. otherwise asks VirusTotal to fetch the GitHub download URL and analyse it,
//! 3. and finally compares the SHA-256 VirusTotal saw with the checksum we
//!    verified locally. Only an exact match counts as clean.
//!
//! Fail-closed: anything unclear is an error unless the user explicitly set
//! `vt.allow_unknown = true`.

use std::collections::BTreeMap;
use std::fs;
use std::thread::sleep;
use std::time::{Duration, Instant};

use base64::Engine as _;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::config::{Config, VtConfig};
use crate::error::{Error, Result};
use crate::github::is_hex64;
use crate::net;
use crate::paths::Paths;
use crate::state::write_atomic;
use crate::time::now_unix;

const MAX_JSON: u64 = 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Verdict {
    Clean,
    Blocked,
    Unknown,
}

impl Verdict {
    pub fn label(self) -> &'static str {
        match self {
            Verdict::Clean => "clean",
            Verdict::Blocked => "blocked",
            Verdict::Unknown => "unclear",
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Stats {
    #[serde(default)]
    pub malicious: u64,
    #[serde(default)]
    pub suspicious: u64,
    #[serde(default)]
    pub harmless: u64,
    #[serde(default)]
    pub undetected: u64,
    #[serde(default)]
    pub timeout: u64,
}

impl Stats {
    pub fn total(self) -> u64 {
        self.malicious + self.suspicious + self.harmless + self.undetected + self.timeout
    }

    pub fn bad(self) -> u64 {
        self.malicious + self.suspicious
    }

    fn from_json(value: &Value) -> Self {
        let get = |name: &str| value.get(name).and_then(Value::as_u64).unwrap_or(0);
        Self {
            malicious: get("malicious"),
            suspicious: get("suspicious"),
            harmless: get("harmless"),
            undetected: get("undetected"),
            timeout: get("timeout"),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct VtResult {
    pub sha256: String,
    pub verdict: Verdict,
    pub stats: Stats,
    pub checked_at: i64,
    pub source: String,
    pub detail: String,
    /// The download URL scanned when `source == "url-scan"`. Entries written by
    /// older versions do not have it and fall back to the release plan's URL.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
}

impl VtResult {
    pub fn permalink(&self) -> String {
        self.permalink_with(None)
    }

    /// Returns the best VirusTotal link for this result. URL-scan verdicts use
    /// the URL analysis page because VirusTotal often has no file object for
    /// them; file reports keep the file page. `fallback_url` supplies the
    /// scanned URL for old cache entries that predate the `url` field.
    pub fn permalink_with(&self, fallback_url: Option<&str>) -> String {
        let scanned = if self.source == "url-scan" {
            self.url.as_deref().or(fallback_url)
        } else {
            None
        };
        match scanned {
            Some(url) => url_report_permalink(url),
            None => file_report_permalink(&self.sha256),
        }
    }

    fn is_fresh(&self, now: i64, days: u64) -> bool {
        now.saturating_sub(self.checked_at) < (days as i64) * 86_400
    }
}

/// Link to the file analysis, keyed by SHA-256.
pub fn file_report_permalink(sha256: &str) -> String {
    format!("https://www.virustotal.com/gui/file/{sha256}")
}

/// Link to the URL analysis, keyed by the base64url encoding of the URL.
pub fn url_report_permalink(url: &str) -> String {
    let id = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(url.as_bytes());
    format!("https://www.virustotal.com/gui/url/{id}")
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct VtCache {
    #[serde(default)]
    pub entries: BTreeMap<String, VtResult>,
}

impl VtCache {
    pub fn load(paths: &Paths) -> Self {
        fs::read_to_string(paths.vt_cache_file())
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default()
    }

    pub fn save(&self, paths: &Paths) -> Result<()> {
        let json = serde_json::to_vec_pretty(self)?;
        fs::create_dir_all(&paths.state_dir)?;
        write_atomic(&paths.vt_cache_file(), &json, 0o600)?;
        Ok(())
    }

    fn prune(&mut self, now: i64, days: u64) {
        self.entries.retain(|_, r| r.is_fresh(now, days));
    }
}

pub struct VtClient {
    agent: ureq::Agent,
    key: String,
    cfg: VtConfig,
    cache: VtCache,
    paths: Paths,
    last_request: Option<Instant>,
}

impl VtClient {
    /// Drops a cached verdict so the next check re-queries VirusTotal.
    pub fn invalidate(&mut self, sha256: &str) {
        let sha = sha256.to_ascii_lowercase();
        if self.cache.entries.remove(&sha).is_some() {
            let _ = self.cache.save(&self.paths);
        }
    }

    pub fn new(paths: &Paths, cfg: &Config) -> Result<Self> {
        let key = cfg.vt_key().ok_or(Error::VirusTotalNotConfigured)?;
        let mut cache = VtCache::load(paths);
        cache.prune(now_unix(), cfg.vt.cache_days);
        Ok(Self {
            agent: net::agent(Duration::from_secs(45)),
            key,
            cfg: cfg.vt.clone(),
            cache,
            paths: paths.clone(),
            last_request: None,
        })
    }

    /// Verifies one artifact. `download_url` must be the GitHub URL the file
    /// will actually be downloaded from.
    pub fn check(
        &mut self,
        sha256: &str,
        download_url: &str,
        report: &dyn Fn(&str),
    ) -> Result<VtResult> {
        if !is_hex64(sha256) {
            return Err(Error::VirusTotal(format!(
                "invalid SHA-256 value: {sha256}"
            )));
        }
        let sha256 = sha256.to_ascii_lowercase();

        if let Some(cached) = self.cache.entries.get(&sha256) {
            if cached.is_fresh(now_unix(), self.cfg.cache_days) {
                let mut cached = cached.clone();
                // Old URL-scan entries did not store the scanned URL; backfill
                // it so their permalink points at the URL report.
                if cached.source == "url-scan"
                    && cached.url.is_none()
                    && !download_url.is_empty()
                {
                    cached.url = Some(download_url.to_string());
                    self.cache.entries.insert(sha256.clone(), cached.clone());
                    let _ = self.cache.save(&self.paths);
                }
                report(&format!(
                    "VirusTotal: cached result ({} · {} of {} engines)",
                    cached.verdict.label(),
                    cached.stats.bad(),
                    cached.stats.total()
                ));
                return Ok(cached);
            }
        }

        // 1. A known file needs no URL scan.
        report("VirusTotal: checking known file analysis …");
        if let Some(result) = self.file_report(&sha256)? {
            return self.finish(result);
        }

        // 2. Unknown file: let VirusTotal fetch the GitHub URL.
        report("VirusTotal: file unknown, starting URL scan (may take a few minutes) …");
        let analysis_id = self.submit_url(download_url)?;

        let mut hash_seen: Option<String> = None;
        let mut stats = Stats::default();
        let mut completed = false;

        for attempt in 0..self.cfg.max_polls {
            sleep(Duration::from_secs(self.cfg.poll_seconds));
            report(&format!(
                "VirusTotal: waiting for analysis ({}/{}) …",
                attempt + 1,
                self.cfg.max_polls
            ));
            let reply = self.get_json(&format!("/api/v3/analyses/{analysis_id}"))?;
            if !reply.is_success() {
                return Err(api_error(&reply));
            }
            let value: Value = serde_json::from_slice(&reply.body)?;
            let attributes = value
                .get("data")
                .and_then(|d| d.get("attributes"))
                .cloned()
                .unwrap_or(Value::Null);
            if let Some(hash) = value
                .pointer("/meta/file_info/sha256")
                .and_then(Value::as_str)
                .filter(|h| is_hex64(h))
            {
                hash_seen = Some(hash.to_ascii_lowercase());
            }
            if let Some(parsed) = attributes.get("stats") {
                stats = Stats::from_json(parsed);
            }
            if attributes.get("status").and_then(Value::as_str) == Some("completed") {
                completed = true;
                break;
            }
        }

        // 3. The URL object may know the content hash even when the analysis
        //    metadata does not.
        if hash_seen.is_none() {
            if let Some((url_stats, url_hash)) = self.url_report(download_url)? {
                if stats.total() == 0 {
                    stats = url_stats;
                }
                hash_seen = url_hash;
            }
        }

        if !completed && stats.total() == 0 && hash_seen.is_none() {
            return Err(Error::VirusTotalUnknown(
                "Analysis did not finish in time; please try again later".into(),
            ));
        }

        if stats.bad() > 0 {
            let result = VtResult {
                sha256: sha256.clone(),
                verdict: Verdict::Blocked,
                stats,
                checked_at: now_unix(),
                source: "url-scan".into(),
                detail: format!(
                    "{} of {} engines report the file as malicious or suspicious",
                    stats.bad(),
                    stats.total()
                ),
                url: Some(download_url.to_string()),
            };
            return self.finish(result);
        }

        match &hash_seen {
            Some(seen) if seen == &sha256 => {
                let result = VtResult {
                    sha256: sha256.clone(),
                    verdict: Verdict::Clean,
                    stats,
                    checked_at: now_unix(),
                    source: "url-scan".into(),
                    detail: format!(
                        "VirusTotal checked exactly this file: 0 of {} engines report anything",
                        stats.total()
                    ),
                    url: Some(download_url.to_string()),
                };
                self.finish(result)
            }
            Some(seen) => Err(Error::VirusTotalUnknown(format!(
                "VirusTotal checked a different file ({} instead of {})",
                short(seen),
                short(&sha256)
            ))),
            None => {
                if self.cfg.allow_unknown {
                    let result = VtResult {
                        sha256: sha256.clone(),
                        verdict: Verdict::Unknown,
                        stats,
                        checked_at: now_unix(),
                        source: "url-scan".into(),
                        detail: "VirusTotal could not confirm the content; \
                                 continuing because allow_unknown is set"
                            .into(),
                        url: Some(download_url.to_string()),
                    };
                    self.finish(result)
                } else {
                    Err(Error::VirusTotalUnknown(
                        "VirusTotal could not confirm the file".into(),
                    ))
                }
            }
        }
    }

    fn finish(&mut self, mut result: VtResult) -> Result<VtResult> {
        if result.verdict == Verdict::Unknown && !self.cfg.allow_unknown {
            return Err(Error::VirusTotalUnknown(result.detail));
        }
        if result.detail.is_empty() {
            result.detail = format!("{} of {} engines", result.stats.bad(), result.stats.total());
        }
        // Only definitive results are cached, so an unclear scan is retried.
        if result.verdict != Verdict::Unknown {
            self.cache.entries.insert(result.sha256.clone(), result.clone());
            if let Err(error) = self.cache.save(&self.paths) {
                // A broken cache must not block an otherwise verified install.
                let _ = error;
            }
        }
        Ok(result)
    }

    fn file_report(&mut self, sha256: &str) -> Result<Option<VtResult>> {
        let reply = self.get_json(&format!("/api/v3/files/{sha256}"))?;
        match reply.status {
            200 => {
                let value: Value = serde_json::from_slice(&reply.body)?;
                let attributes = value
                    .get("data")
                    .and_then(|d| d.get("attributes"))
                    .cloned()
                    .unwrap_or(Value::Null);
                let stats = Stats::from_json(attributes.get("last_analysis_stats").unwrap_or(&Value::Null));
                if stats.total() == 0 {
                    return Ok(None);
                }
                let verdict = if stats.bad() > 0 {
                    Verdict::Blocked
                } else {
                    Verdict::Clean
                };
                Ok(Some(VtResult {
                    sha256: sha256.to_string(),
                    verdict,
                    stats,
                    checked_at: now_unix(),
                    source: "file-report".into(),
                    detail: format!(
                        "{} of {} engines report something",
                        stats.bad(),
                        stats.total()
                    ),
                    url: None,
                }))
            }
            404 => Ok(None),
            _ => Err(api_error(&reply)),
        }
    }

    fn submit_url(&mut self, url: &str) -> Result<String> {
        self.throttle();
        let reply = net::post_form(
            &self.agent,
            &format!("{}/api/v3/urls", api_base()),
            &[("x-apikey", self.key.as_str())],
            &[("url", url)],
            MAX_JSON,
        )
        .map_err(|e| Error::VirusTotal(e.to_string()))?;
        if !reply.is_success() {
            return Err(api_error(&reply));
        }
        let value: Value = serde_json::from_slice(&reply.body)?;
        value
            .pointer("/data/id")
            .and_then(Value::as_str)
            .map(str::to_string)
            .ok_or_else(|| {
                Error::VirusTotal("URL scan response contained no analysis ID".into())
            })
    }

    fn url_report(&mut self, url: &str) -> Result<Option<(Stats, Option<String>)>> {
        let id = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(url.as_bytes());
        let reply = self.get_json(&format!("/api/v3/urls/{id}"))?;
        if reply.status == 404 {
            return Ok(None);
        }
        if !reply.is_success() {
            return Err(api_error(&reply));
        }
        let value: Value = serde_json::from_slice(&reply.body)?;
        let attributes = value
            .get("data")
            .and_then(|d| d.get("attributes"))
            .cloned()
            .unwrap_or(Value::Null);
        let stats = Stats::from_json(
            attributes
                .get("last_analysis_stats")
                .unwrap_or(&Value::Null),
        );
        let hash = attributes
            .get("last_http_response_content_sha256")
            .and_then(Value::as_str)
            .filter(|h| is_hex64(h))
            .map(str::to_ascii_lowercase);
        if stats.total() == 0 && hash.is_none() {
            return Ok(None);
        }
        Ok(Some((stats, hash)))
    }

    fn get_json(&mut self, path: &str) -> Result<net::Reply> {
        self.throttle();
        net::get(
            &self.agent,
            &format!("{}{path}", api_base()),
            &[("x-apikey", self.key.as_str())],
            MAX_JSON,
        )
        .map_err(|e| Error::VirusTotal(e.to_string()))
    }

    fn throttle(&mut self) {
        if let Some(last) = self.last_request {
            let minimum = Duration::from_secs(self.cfg.min_interval_seconds);
            if let Some(wait) = minimum.checked_sub(last.elapsed()) {
                if !wait.is_zero() {
                    sleep(wait);
                }
            }
        }
        self.last_request = Some(Instant::now());
    }
}

/// VirusTotal's public API. Tests may point this at a local mock.
fn api_base() -> String {
    std::env::var("VIRUSTOTAL_API_BASE")
        .unwrap_or_else(|_| "https://www.virustotal.com".to_string())
        .trim_end_matches('/')
        .to_string()
}

fn api_error(reply: &net::Reply) -> Error {
    let message = serde_json::from_slice::<Value>(&reply.body)
        .ok()
        .and_then(|v| {
            v.pointer("/error/message")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .unwrap_or_else(|| reply.body_text());
    let message = crate::github::truncate(message.trim(), 300);
    match reply.status {
        401 => Error::VirusTotal("API key was rejected (401)".into()),
        429 => Error::VirusTotal(
            "Quota exhausted (Public API: 4 requests/minute, 500/day)".into(),
        ),
        403 => Error::VirusTotal(format!("Access denied: {message}")),
        status => Error::VirusTotal(format!("HTTP {status}: {message}")),
    }
}

fn short(hash: &str) -> String {
    hash.chars().take(16).collect::<String>() + "…"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stats_distinguish_clean_and_bad() {
        let clean = Stats {
            harmless: 60,
            undetected: 10,
            ..Default::default()
        };
        assert_eq!(clean.total(), 70);
        assert_eq!(clean.bad(), 0);
        let bad = Stats {
            malicious: 2,
            ..clean
        };
        assert_eq!(bad.bad(), 2);
    }

    #[test]
    fn parses_vt_stat_objects_leniently() {
        let stats = Stats::from_json(&serde_json::json!({
            "malicious": 0, "suspicious": 1, "harmless": 5,
            "undetected": 3, "timeout": 0, "extra": "ignored"
        }));
        assert_eq!(stats.suspicious, 1);
        assert_eq!(stats.total(), 9);
    }

    #[test]
    fn url_ids_are_base64url_without_padding() {
        let id = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(b"https://github.com/storytold/gridcraft/releases/download/v1/x");
        assert!(!id.contains('='));
        assert!(!id.contains('+'));
        assert!(!id.contains('/'));
    }

    #[test]
    fn cache_roundtrips_and_prunes() {
        let dir = tempfile::tempdir().unwrap();
        let mut paths = Paths::from_env();
        paths.state_dir = dir.path().to_path_buf();
        let mut cache = VtCache::default();
        cache.entries.insert(
            "a".repeat(64),
            VtResult {
                sha256: "a".repeat(64),
                verdict: Verdict::Clean,
                stats: Stats::default(),
                checked_at: 100,
                source: "test".into(),
                detail: String::new(),
                url: None,
            },
        );
        cache.save(&paths).unwrap();
        let mut loaded = VtCache::load(&paths);
        assert_eq!(loaded.entries.len(), 1);
        loaded.prune(100 + 8 * 86_400, 7);
        assert!(loaded.entries.is_empty());
    }

    fn result(source: &str, url: Option<&str>) -> VtResult {
        VtResult {
            sha256: "a".repeat(64),
            verdict: Verdict::Clean,
            stats: Stats::default(),
            checked_at: 100,
            source: source.into(),
            detail: String::new(),
            url: url.map(str::to_string),
        }
    }

    #[test]
    fn file_reports_link_to_the_sha256_page() {
        let result = result("file-report", None);
        assert_eq!(
            result.permalink(),
            format!("https://www.virustotal.com/gui/file/{}", "a".repeat(64))
        );
    }

    #[test]
    fn url_scans_link_to_the_url_analysis() {
        let asset = "https://github.com/storytold/filmcraft/releases/download/v0.15.0/filmcraft-0.15.0-linux-x86_64.tar.gz";
        let result = result("url-scan", Some(asset));
        assert_eq!(result.permalink(), url_report_permalink(asset));
        assert!(result.permalink().contains("/gui/url/"));
        assert!(!result.permalink().contains('='));
    }

    #[test]
    fn old_url_scan_cache_entries_use_the_plan_url() {
        let asset = "https://github.com/storytold/filmcraft/releases/download/v0.15.0/filmcraft-0.15.0-linux-x86_64.tar.gz";
        let result = result("url-scan", None);
        assert_eq!(
            result.permalink_with(Some(asset)),
            url_report_permalink(asset)
        );
        // Without a fallback there is nothing better than the file page.
        assert!(result.permalink_with(None).contains("/gui/file/"));
    }

    #[test]
    fn old_cache_entries_load_without_the_url_field() {
        let json = serde_json::json!({
            "entries": {
                "a".repeat(64): {
                    "sha256": "a".repeat(64),
                    "verdict": "clean",
                    "stats": { "harmless": 1 },
                    "checked_at": 100,
                    "source": "url-scan",
                    "detail": ""
                }
            }
        });
        let cache: VtCache = serde_json::from_value(json).unwrap();
        assert!(cache.entries[&"a".repeat(64)].url.is_none());
    }
}
