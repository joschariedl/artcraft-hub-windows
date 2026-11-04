use std::cmp::Ordering;

/// A release tag such as `v0.13.0` or `0.13.0`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReleaseVersion {
    /// Version without a leading `v`, exactly as it appears in the tag.
    pub raw: String,
    /// Parsed semver, when possible.
    pub parsed: Option<semver::Version>,
}

impl ReleaseVersion {
    pub fn parse(tag: &str) -> Self {
        let raw = tag.trim().trim_start_matches(['v', 'V']).to_string();
        let parsed = semver::Version::parse(&raw).ok();
        Self { raw, parsed }
    }

    /// Compare two versions. Falls back to natural string ordering when either
    /// side is not semver, so odd tags still behave predictably.
    pub fn cmp_version(&self, other: &Self) -> Ordering {
        match (&self.parsed, &other.parsed) {
            (Some(a), Some(b)) => a.cmp(b),
            _ => self.raw.cmp(&other.raw),
        }
    }

    pub fn is_newer_than(&self, installed: &str) -> bool {
        let installed = ReleaseVersion::parse(installed);
        self.cmp_version(&installed) == Ordering::Greater
    }
}

impl std::fmt::Display for ReleaseVersion {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.raw)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_v_and_compares() {
        let latest = ReleaseVersion::parse("v0.17.0");
        assert_eq!(latest.raw, "0.17.0");
        assert!(latest.is_newer_than("0.16.9"));
        assert!(!latest.is_newer_than("0.17.0"));
        assert!(!latest.is_newer_than("0.18.0"));
    }

    #[test]
    fn compares_prereleases() {
        let latest = ReleaseVersion::parse("v1.0.0-beta.2");
        assert!(latest.is_newer_than("1.0.0-beta.1"));
        assert!(!latest.is_newer_than("1.0.0"));
    }
}
