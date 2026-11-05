//! Trust anchors beyond the checksum list.
//!
//! The checksum list itself is fetched over TLS and compared against GitHub's
//! asset digest, which protects against corrupted downloads but not against a
//! compromised release account. Three optional anchors add more confidence:
//!
//! * GitHub/Sigstore provenance attestations (`gh attestation verify`),
//! * a detached GPG signature over `SHA256SUMS.txt`,
//! * a detached GPG signature over the release artifact.
//!
//! Missing anchors are a warning by default. Setting `require_*` in the config
//! turns a missing anchor into a hard failure.

use std::path::Path;
use std::process::{Command, Stdio};

use crate::catalog::AppDef;
use crate::config::Config;
use crate::error::{Error, Result};
use crate::github::{self, ReleasePlan};
use crate::progress::{self, ProgressFn};

/// Result of an optional verification.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TrustState {
    Verified,
    /// The release simply does not publish this anchor.
    Missing,
    /// The tool needed for the check is not installed or not configured.
    Unavailable,
}

impl TrustState {
    pub fn is_present(self) -> bool {
        self != Self::Missing
    }
}

/// Verifies GitHub's Sigstore attestations for the downloaded artifact.
pub fn verify_attestations(
    repo: &str,
    archive: &Path,
    cfg: &Config,
    progress: ProgressFn<'_>,
) -> Result<TrustState> {
    if !cfg.verify_attestations && !cfg.require_attestations {
        return Ok(TrustState::Unavailable);
    }
    if !command_exists("gh") {
        if cfg.require_attestations {
            return Err(Error::AttestationFailed(
                "`gh` is not installed, so attestations cannot be verified".into(),
            ));
        }
        progress::message(
            progress,
            "Note: gh not installed; skipping attestation verification.",
        );
        return Ok(TrustState::Unavailable);
    }

    let output = Command::new("gh")
        .args(["attestation", "verify"])
        .arg(archive)
        .args(["--repo", repo, "--format", "json"])
        .stdin(Stdio::null())
        .output()
        .map_err(|e| Error::AttestationFailed(e.to_string()))?;
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    let state = classify_attestation(output.status.success(), &stderr)?;

    match state {
        TrustState::Verified => progress::message(progress, "GitHub attestation verified."),
        TrustState::Missing if cfg.require_attestations => {
            return Err(Error::AttestationRequired(repo.to_string()));
        }
        TrustState::Missing => {
            progress::message(
                progress,
                "No GitHub attestation published; continuing (warning only).",
            );
        }
        TrustState::Unavailable => {}
    }
    Ok(state)
}

/// Classifies `gh attestation verify`. A 404 from the attestations endpoint (or
/// an explicit "no attestations" message) means the release simply has none;
/// any other failure is a real verification problem.
pub fn classify_attestation(success: bool, stderr: &str) -> Result<TrustState> {
    if success {
        return Ok(TrustState::Verified);
    }
    let lower = stderr.to_ascii_lowercase();
    let missing = lower.contains("404")
        && lower.contains("attestation")
        || lower.contains("no attestations")
        || lower.contains("no attestation found");
    if missing {
        Ok(TrustState::Missing)
    } else {
        Err(Error::AttestationFailed(shorten(stderr)))
    }
}

/// Verifies a detached signature over `SHA256SUMS.txt`.
pub fn verify_manifest_signature(
    agent: &ureq::Agent,
    app: &AppDef,
    plan: &ReleasePlan,
    cfg: &Config,
    progress: ProgressFn<'_>,
) -> Result<TrustState> {
    if !cfg.verify_signed_manifest && !cfg.require_signed_manifest {
        return Ok(TrustState::Unavailable);
    }
    let sums_url = plan.sums_url(app.repo);
    let sums = match github::fetch_sums_bytes(agent, app.repo, &plan.tag, cfg.network_attempts()) {
        Ok(bytes) => bytes,
        Err(error) => {
            if cfg.require_signed_manifest {
                return Err(Error::SignatureRequired(format!(
                    "{} (SHA256SUMS.txt unavailable: {error})",
                    plan.tag
                )));
            }
            progress::message(
                progress,
                format!("Note: signed manifest check skipped ({error})."),
            );
            return Ok(TrustState::Missing);
        }
    };

    let Some(signature) = fetch_detached_signature(agent, &sums_url, cfg.network_attempts())? else {
        if cfg.require_signed_manifest {
            return Err(Error::SignatureRequired(plan.tag.clone()));
        }
        progress::message(
            progress,
            "No signature for SHA256SUMS.txt published; continuing (warning only).",
        );
        return Ok(TrustState::Missing);
    };

    let state = verify_gpg_bytes(&sums, &signature, cfg, progress)?;
    if state == TrustState::Verified {
        progress::message(progress, "SHA256SUMS.txt signature verified.");
    }
    Ok(state)
}

/// Verifies an optional GPG signature published next to the release artifact.
pub fn verify_artifact_signature(
    agent: &ureq::Agent,
    plan: &ReleasePlan,
    archive: &Path,
    cfg: &Config,
    progress: ProgressFn<'_>,
) -> Result<TrustState> {
    let Some(signature) =
        fetch_detached_signature(agent, &plan.asset_url, cfg.network_attempts())?
    else {
        if cfg.require_gpg_signature {
            return Err(Error::SignatureRequired(plan.tag.clone()));
        }
        progress::message(
            progress,
            "No GPG signature published for the archive; continuing (warning only).",
        );
        return Ok(TrustState::Missing);
    };

    let state = verify_gpg_file(archive, &signature, cfg, progress)?;
    if state == TrustState::Verified {
        progress::message(progress, "GPG signature verified.");
    }
    Ok(state)
}

fn fetch_detached_signature(
    agent: &ureq::Agent,
    base_url: &str,
    attempts: u32,
) -> Result<Option<Vec<u8>>> {
    for suffix in [".sig", ".asc"] {
        let url = format!("{base_url}{suffix}");
        let reply = match crate::net::get_retry(agent, &url, &[], 2 * 1024 * 1024, attempts) {
            Ok(reply) => reply,
            Err(_) => continue,
        };
        if reply.status == 200 && !reply.body.is_empty() {
            return Ok(Some(reply.body));
        }
    }
    Ok(None)
}

fn verify_gpg_bytes(
    content: &[u8],
    signature: &[u8],
    cfg: &Config,
    progress: ProgressFn<'_>,
) -> Result<TrustState> {
    let dir = tempfile::tempdir()?;
    let content_path = dir.path().join("content");
    std::fs::write(&content_path, content)?;
    verify_gpg_file(&content_path, signature, cfg, progress)
}

fn verify_gpg_file(
    content: &Path,
    signature: &[u8],
    cfg: &Config,
    progress: ProgressFn<'_>,
) -> Result<TrustState> {
    if !command_exists("gpg") {
        if cfg.require_gpg_signature || cfg.require_signed_manifest {
            return Err(Error::SignatureFailed(
                "gpg is not installed, so signatures cannot be verified".into(),
            ));
        }
        progress::message(progress, "Note: gpg not installed; skipping signature check.");
        return Ok(TrustState::Unavailable);
    }

    let dir = tempfile::tempdir()?;
    let sig_path = dir.path().join("signature.bin");
    std::fs::write(&sig_path, signature)?;

    let mut command = Command::new("gpg");
    if !cfg.gpg_keyring.trim().is_empty() {
        command
            .arg("--no-default-keyring")
            .arg("--keyring")
            .arg(cfg.gpg_keyring.trim());
    }
    let output = command
        .arg("--status-fd")
        .arg("1")
        .arg("--verify")
        .arg(&sig_path)
        .arg(content)
        .output()
        .map_err(|e| Error::SignatureFailed(e.to_string()))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        if !cfg.require_gpg_signature && stderr.contains("No public key") {
            progress::message(
                progress,
                "GPG signature published but the signing key is not in your keyring; \
                 continuing (warning only). Import key 568DC1F68F8FAECC797F3C7B006DAF6BB0EF334E \
                 to verify."
                    .to_string(),
            );
            return Ok(TrustState::Unavailable);
        }
        return Err(Error::SignatureFailed(shorten(&stderr)));
    }

    if !cfg.gpg_trusted_fingerprints.is_empty() {
        let fingerprint = parse_validsig(&output.stdout);
        let Some(fingerprint) = fingerprint else {
            return Err(Error::SignatureFailed(
                "gpg did not report a signer fingerprint".into(),
            ));
        };
        let trusted: Vec<String> = cfg
            .gpg_trusted_fingerprints
            .iter()
            .map(|f| crate::config::normalize_fingerprint(f))
            .collect();
        if !trusted.iter().any(|t| {
            fingerprint.ends_with(t) || t.ends_with(&fingerprint)
        }) {
            return Err(Error::SignatureFailed(format!(
                "signer fingerprint {fingerprint} is not trusted"
            )));
        }
    }
    Ok(TrustState::Verified)
}


fn parse_validsig(stdout: &[u8]) -> Option<String> {
    let text = String::from_utf8_lossy(stdout);
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("[GNUPG:] VALIDSIG ") {
            let fingerprint = rest.split_whitespace().next()?;
            return Some(crate::config::normalize_fingerprint(fingerprint));
        }
    }
    None
}

fn command_exists(name: &str) -> bool {
    let Some(path) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&path)
        .map(|dir| dir.join(name))
        .any(|candidate| candidate.is_file())
}

fn shorten(text: &str) -> String {
    github::truncate(text.trim(), 500)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_attestations() {
        assert_eq!(
            classify_attestation(true, "").unwrap(),
            TrustState::Verified
        );
        assert_eq!(
            classify_attestation(
                false,
                "Error: HTTP 404: Not Found (https://api.github.com/repos/a/b/attestations/sha256:x)"
            )
            .unwrap(),
            TrustState::Missing
        );
        assert_eq!(
            classify_attestation(false, "Error: no attestations found").unwrap(),
            TrustState::Missing
        );
        assert!(matches!(
            classify_attestation(false, "Error: certificate identity mismatch"),
            Err(Error::AttestationFailed(_))
        ));
    }

    #[test]
    fn trust_states_distinguish_missing_from_unavailable() {
        assert!(TrustState::Verified.is_present());
        assert!(!TrustState::Missing.is_present());
        assert!(TrustState::Unavailable.is_present());
    }
}
