//! Streaming downloads with an inseparable hash check and resume support.
//!
//! The artifact is written to a `.part` file while being hashed; only when the
//! SHA-256 matches the verified value from the release metadata is it moved
//! into place. Interrupted transfers resume with an HTTP `Range` request, and
//! the hash of the already-written prefix is recomputed before appending.

use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::thread::sleep;
use std::time::Duration;

use sha2::{Digest, Sha256};

use crate::error::{Error, Result};
use crate::github::sha256_hex;
use crate::net;
use crate::progress::Progress;

/// Refuse anything larger than this; the biggest Craft App tarball is < 150 MB.
pub const MAX_ARTIFACT: u64 = 512 * 1024 * 1024;

/// Downloads `url` to `dest`, verifying `expected_sha256`.
pub fn fetch(
    agent: &ureq::Agent,
    url: &str,
    dest: &Path,
    expected_sha256: &str,
    progress: &dyn Fn(Progress),
) -> Result<()> {
    fetch_with_retries(agent, url, dest, expected_sha256, 1, progress)
}

/// Downloads with bounded retries. Interrupted bytes stay in `<dest>.part` so
/// the next attempt continues with a range request instead of starting over.
pub fn fetch_with_retries(
    agent: &ureq::Agent,
    url: &str,
    dest: &Path,
    expected_sha256: &str,
    attempts: u32,
    progress: &dyn Fn(Progress),
) -> Result<()> {
    let attempts = attempts.max(1);
    let mut last_error: Option<Error> = None;
    let mut delay = Duration::from_millis(500);

    for attempt in 1..=attempts {
        match fetch_once(agent, url, dest, expected_sha256, progress) {
            Ok(()) => return Ok(()),
            Err(error @ Error::HashMismatch { .. }) => {
                // A wrong prefix must never be resumed; discard it and retry.
                let _ = fs::remove_file(part_path(dest));
                last_error = Some(error);
            }
            Err(error) => {
                last_error = Some(error);
            }
        }
        if attempt < attempts {
            sleep(delay);
            delay = (delay * 2).min(Duration::from_secs(8));
        }
    }

    Err(last_error.unwrap_or_else(|| Error::Http("download failed".into())))
}

fn fetch_once(
    agent: &ureq::Agent,
    url: &str,
    dest: &Path,
    expected_sha256: &str,
    progress: &dyn Fn(Progress),
) -> Result<()> {
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent)?;
    }
    let tmp = part_path(dest);
    let mut existing = fs::metadata(&tmp).map(|m| m.len()).unwrap_or(0);
    if existing > MAX_ARTIFACT {
        let _ = fs::remove_file(&tmp);
        existing = 0;
    }

    let mut request = agent
        .get(url)
        .header("Accept", "application/octet-stream");
    if existing > 0 {
        request = request.header("Range", &format!("bytes={existing}-"));
    }
    let response = request.call().map_err(|e| Error::Http(e.to_string()))?;

    let status = response.status().as_u16();
    let resumed = status == 206 && existing > 0;
    if status == 416 {
        // The server no longer has our prefix (or the file shrank).
        let _ = fs::remove_file(&tmp);
        return Err(Error::Http(format!(
            "Download of {url} cannot resume (HTTP 416)"
        )));
    }
    if status != 200 && !resumed {
        return Err(Error::Http(format!(
            "Download of {url} failed with HTTP {status}"
        )));
    }
    let start = if resumed { existing } else { 0 };

    let remaining = response.body().content_length();
    let total = remaining.map(|size| start + size);
    if total.is_some_and(|size| size > MAX_ARTIFACT) {
        let _ = fs::remove_file(&tmp);
        return Err(Error::Http(format!(
            "File is larger than the allowed limit ({} MB)",
            MAX_ARTIFACT / 1024 / 1024
        )));
    }

    let mut hasher = Sha256::new();
    let mut received = start;
    if start > 0 {
        let mut prefix = fs::File::open(&tmp)?;
        let mut buf = vec![0u8; 128 * 1024];
        let mut hashed = 0u64;
        while hashed < start {
            let limit = ((start - hashed) as usize).min(buf.len());
            let read = prefix.read(&mut buf[..limit])?;
            if read == 0 {
                break;
            }
            hasher.update(&buf[..read]);
            hashed += read as u64;
        }
        if hashed != start {
            let _ = fs::remove_file(&tmp);
            return Err(Error::Http("cached download prefix is incomplete".into()));
        }
    }

    let mut body = response.into_body();
    let mut reader = body.with_config().limit(MAX_ARTIFACT).reader();
    let mut file = if start > 0 {
        fs::OpenOptions::new().append(true).open(&tmp)?
    } else {
        fs::File::create(&tmp)?
    };
    let mut last_report = received;
    let mut buf = vec![0u8; 128 * 1024];

    let result = (|| -> Result<()> {
        loop {
            let read = reader.read(&mut buf)?;
            if read == 0 {
                break;
            }
            received += read as u64;
            if received > MAX_ARTIFACT {
                return Err(Error::Http("File exceeds the size limit".into()));
            }
            hasher.update(&buf[..read]);
            file.write_all(&buf[..read])?;
            if received - last_report >= 1024 * 1024 {
                last_report = received;
                progress(Progress::Download { received, total });
            }
        }
        file.flush()?;
        file.sync_all()?;
        Ok(())
    })();

    result?;
    drop(file);

    let actual = hex::encode(hasher.finalize());
    if !actual.eq_ignore_ascii_case(expected_sha256) {
        return Err(Error::HashMismatch {
            expected: expected_sha256.to_ascii_lowercase(),
            actual,
        });
    }

    fs::rename(&tmp, dest)?;
    progress(Progress::Download {
        received,
        total: Some(received),
    });
    Ok(())
}

/// Path of the resumable partial download.
pub fn part_path(dest: &Path) -> PathBuf {
    let name = dest
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("download");
    dest.with_file_name(format!("{name}.part"))
}

/// True when `path` already has the expected checksum.
pub fn has_checksum(path: &Path, expected_sha256: &str) -> bool {
    let Ok(bytes) = fs::read(path) else {
        return false;
    };
    sha256_hex(&bytes).eq_ignore_ascii_case(expected_sha256)
}

/// Small bounded GET, used for icons and other auxiliary files.
pub fn fetch_bytes(agent: &ureq::Agent, url: &str, max: u64) -> Result<Vec<u8>> {
    let reply = net::get(agent, url, &[], max)?;
    if !reply.is_success() {
        return Err(Error::Http(format!(
            "{url} replied with HTTP {}",
            reply.status
        )));
    }
    Ok(reply.body)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_cached_checksums() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.bin");
        fs::write(&path, b"hello").unwrap();
        assert!(has_checksum(
            &path,
            "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"
        ));
        assert!(!has_checksum(&path, "00"));
    }

    #[test]
    fn part_file_sits_next_to_the_target() {
        let dest = Path::new("/tmp/foo/gridcraft-1.0.tar.gz");
        assert_eq!(
            part_path(dest),
            PathBuf::from("/tmp/foo/gridcraft-1.0.tar.gz.part")
        );
    }
}
