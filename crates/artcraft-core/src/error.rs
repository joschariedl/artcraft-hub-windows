use thiserror::Error;

#[derive(Debug, Error)]
pub enum Error {
    #[error("Network error: {0}")]
    Http(String),

    #[error("GitHub rate-limited the request. Please try again later or configure a GitHub token in settings.")]
    RateLimited,

    #[error("No release found for {0}")]
    NoRelease(String),

    #[error("Release-Asset missing: {0}")]
    MissingAsset(String),

    #[error("Invalid download URL: {0}")]
    InvalidUrl(String),

    #[error("SHA256SUMS.txt missing in release {0}")]
    MissingChecksums(String),

    #[error("SHA-256 mismatch: expected {expected}, got {actual}")]
    HashMismatch { expected: String, actual: String },

    #[error("The SHA-256 digest from GitHub and SHA256SUMS.txt disagree ({0})")]
    ChecksumConflict(String),

    #[error("SHA256SUMS.txt contains no entry for {0}")]
    ChecksumNotListed(String),

    #[error("VirusTotal is not configured. Nothing is installed without an API key.")]
    VirusTotalNotConfigured,

    #[error("VirusTotal: {0}")]
    VirusTotal(String),

    #[error("VirusTotal blocked the package: {0}")]
    VirusTotalBlocked(String),

    #[error("VirusTotal could not verify the file: {0}")]
    VirusTotalUnknown(String),

    #[error("Another ArtCraft operation is already running. Please wait for it to finish.")]
    Busy,

    #[error("A GPG signature is required, but release {0} does not publish one.")]
    SignatureRequired(String),

    #[error("GPG signature verification failed: {0}")]
    SignatureFailed(String),

    #[error("A GitHub attestation is required, but release {0} does not publish one.")]
    AttestationRequired(String),

    #[error("GitHub attestation verification failed: {0}")]
    AttestationFailed(String),

    #[error("No previous version is available for {0}.")]
    NoRollback(String),

    #[error("Security check failed: {0}")]
    UnsafeArchive(String),

    #[error("Installation failed: {0}")]
    Install(String),

    #[error("Configuration error: {0}")]
    Config(String),

    #[error("Filesystem error: {0}")]
    Io(#[from] std::io::Error),

    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("TOML error: {0}")]
    Toml(#[from] toml::de::Error),

    #[error("{0}")]
    Other(String),
}

pub type Result<T> = std::result::Result<T, Error>;

impl From<ureq::Error> for Error {
    fn from(value: ureq::Error) -> Self {
        Error::Http(value.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_messages_are_readable() {
        let e = Error::HashMismatch {
            expected: "a".into(),
            actual: "b".into(),
        };
        assert!(e.to_string().contains("a"));
    }
}
