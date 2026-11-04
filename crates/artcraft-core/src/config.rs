use std::fs;
use std::io::Write;
#[cfg(unix)]
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::paths::Paths;

/// User configuration (`~/.config/artcraft-hub/config.toml`).
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Optional GitHub token. Raises the API limit from 60 to 5000 requests/hour.
    pub github_token: String,
    /// VirusTotal API key. May also be supplied via `VIRUSTOTAL_API_KEY`.
    pub virustotal_api_key: String,
    /// How long resolved release metadata may be reused.
    pub metadata_cache_minutes: u64,
    /// How often the Omarchy widget / hub should look for updates.
    pub check_interval_minutes: u64,
    /// Refuse releases that do not ship a `SHA256SUMS.txt`.
    pub require_sha256sums: bool,
    /// Also install the 1024 px PNG icon from the app's repository.
    pub install_1024_icons: bool,
    /// Number of versions kept per app (the active one plus older rollback
    /// targets). The rest are pruned after a successful install.
    pub keep_versions: u64,
    /// How many release lookups may run at the same time.
    pub parallel_checks: u64,
    /// Network attempts (including the first one) for transient failures.
    pub network_retries: u64,
    /// Verify GitHub/Sigstore provenance attestations with `gh`, when the
    /// release has them.
    pub verify_attestations: bool,
    /// Fail when a release has no GitHub attestation.
    pub require_attestations: bool,
    /// Verify a detached signature over `SHA256SUMS.txt` when one exists.
    pub verify_signed_manifest: bool,
    /// Fail when `SHA256SUMS.txt` is not signed.
    pub require_signed_manifest: bool,
    /// Fail when a release has no GPG signature for the artifact.
    pub require_gpg_signature: bool,
    /// Optional dedicated GPG keyring. Empty means the user's default keyring.
    pub gpg_keyring: String,
    /// Trusted GPG key fingerprints (hex, spaces allowed). Empty means any
    /// valid signature is accepted.
    pub gpg_trusted_fingerprints: Vec<String>,
    /// Follow the active Omarchy theme instead of the built-in palette.
    pub follow_omarchy_theme: bool,
    pub vt: VtConfig,
    /// UI language for the hub (`de` or `en`). Only `de` is complete today.
    pub language: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct VtConfig {
    /// VirusTotal's public API allows 4 requests/minute.
    pub min_interval_seconds: u64,
    /// How long a VirusTotal verdict may be reused for the same SHA-256.
    pub cache_days: u64,
    /// Never enable this unless you fully understand the risk.
    pub allow_unknown: bool,
    /// Delay before the first status poll of a submitted URL scan.
    pub poll_seconds: u64,
    /// Maximum number of status polls per artifact.
    pub max_polls: u32,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            github_token: String::new(),
            virustotal_api_key: String::new(),
            metadata_cache_minutes: 30,
            check_interval_minutes: 60,
            require_sha256sums: true,
            install_1024_icons: true,
            keep_versions: 2,
            parallel_checks: 4,
            network_retries: 3,
            verify_attestations: true,
            require_attestations: false,
            verify_signed_manifest: true,
            require_signed_manifest: false,
            require_gpg_signature: false,
            gpg_keyring: String::new(),
            gpg_trusted_fingerprints: Vec::new(),
            follow_omarchy_theme: true,
            vt: VtConfig::default(),
            language: "en".to_string(),
        }
    }
}

impl Default for VtConfig {
    fn default() -> Self {
        Self {
            min_interval_seconds: 16,
            cache_days: 7,
            allow_unknown: false,
            poll_seconds: 20,
            max_polls: 15,
        }
    }
}

impl Config {
    pub fn load(paths: &Paths) -> Result<Self> {
        let path = paths.config_file();
        if !path.exists() {
            return Ok(Self::default());
        }
        let text = fs::read_to_string(&path)?;
        let mut cfg: Config = toml::from_str(&text)?;
        cfg.normalize();
        Ok(cfg)
    }

    pub fn save(&self, paths: &Paths) -> Result<()> {
        fs::create_dir_all(&paths.config_dir)?;
        let body = toml::to_string_pretty(self).map_err(|e| Error::Config(e.to_string()))?;
        let text = format!(
            "# ArtCraft Hub – configuration\n\
             # This file contains secrets and is written with mode 0600.\n\n{body}"
        );
        let path = paths.config_file();
        let tmp = path.with_extension("toml.tmp");
        {
            let mut file = fs::OpenOptions::new()
                .create(true)
                .truncate(true)
                .write(true)
                .open(&tmp)?;
            file.write_all(text.as_bytes())?;
            file.sync_all()?;
        }
        #[cfg(unix)] fs::set_permissions(&tmp, fs::Permissions::from_mode(0o600))?;
        fs::rename(&tmp, &path)?;
        Ok(())
    }

    /// Clamps values into their supported ranges and trims free-form fields.
    pub fn normalize(&mut self) {
        self.metadata_cache_minutes = self.metadata_cache_minutes.clamp(5, 24 * 60);
        self.check_interval_minutes = self.check_interval_minutes.clamp(5, 24 * 60);
        self.keep_versions = self.keep_versions.clamp(1, 10);
        self.parallel_checks = self.parallel_checks.clamp(1, 8);
        self.network_retries = self.network_retries.clamp(1, 6);
        self.gpg_keyring = self.gpg_keyring.trim().to_string();
        self.gpg_trusted_fingerprints = self
            .gpg_trusted_fingerprints
            .iter()
            .map(|f| normalize_fingerprint(f))
            .filter(|f| !f.is_empty())
            .collect();
        self.vt.min_interval_seconds = self.vt.min_interval_seconds.clamp(1, 120);
        self.vt.cache_days = self.vt.cache_days.clamp(1, 365);
        self.vt.poll_seconds = self.vt.poll_seconds.clamp(10, 300);
        self.vt.max_polls = self.vt.max_polls.clamp(1, 30);
    }

    /// Effective VirusTotal key (config file wins over the environment only
    /// when non-empty; the environment is the fallback).
    pub fn vt_key(&self) -> Option<String> {
        let key = if self.virustotal_api_key.trim().is_empty() {
            std::env::var("VIRUSTOTAL_API_KEY").unwrap_or_default()
        } else {
            self.virustotal_api_key.clone()
        };
        let key = key.trim().to_string();
        (!key.is_empty()).then_some(key)
    }

    pub fn github_token(&self) -> Option<String> {
        let token = if self.github_token.trim().is_empty() {
            std::env::var("GITHUB_TOKEN")
                .or_else(|_| std::env::var("GH_TOKEN"))
                .unwrap_or_default()
        } else {
            self.github_token.clone()
        };
        let token = token.trim().to_string();
        (!token.is_empty()).then_some(token)
    }

    pub fn has_vt_key(&self) -> bool {
        self.vt_key().is_some()
    }

    pub fn metadata_ttl_secs(&self) -> i64 {
        (self.metadata_cache_minutes * 60) as i64
    }

    pub fn network_attempts(&self) -> u32 {
        self.network_retries.clamp(1, 6) as u32
    }

    pub fn keeps_versions(&self) -> usize {
        self.keep_versions.clamp(1, 10) as usize
    }
}

pub fn normalize_fingerprint(raw: &str) -> String {
    raw.chars()
        .filter(|c| c.is_ascii_hexdigit())
        .map(|c| c.to_ascii_uppercase())
        .collect()
}

pub fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
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
    #[cfg(unix)] fs::set_permissions(&tmp, fs::Permissions::from_mode(0o600))?;
    fs::rename(&tmp, path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_safe() {
        let c = Config::default();
        assert!(!c.vt.allow_unknown);
        assert!(c.require_sha256sums);
        assert_eq!(c.vt.min_interval_seconds, 16);
    }

    #[test]
    fn roundtrips() {
        let c = Config::default();
        let text = toml::to_string_pretty(&c).unwrap();
        let back: Config = toml::from_str(&text).unwrap();
        assert_eq!(back.check_interval_minutes, c.check_interval_minutes);
    }
}
