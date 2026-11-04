//! Thin wrapper around `ureq`: one place that configures timeouts, status
//! handling, body limits, retries and response validators.
//!
//! Every byte that reaches this layer is untrusted. Bodies are capped, non-2xx
//! answers are returned instead of panicking, and callers decide what a 404 or
//! a 429 means.

use std::thread::sleep;
use std::time::Duration;

use crate::error::{Error, Result};

pub const USER_AGENT: &str = concat!(
    "ArtCraftHub/",
    env!("CARGO_PKG_VERSION"),
    " (+https://github.com/derfleischmann/artcraft-hub)"
);

/// A bounded HTTP answer.
pub struct Reply {
    pub status: u16,
    pub body: Vec<u8>,
    /// Seconds from `Retry-After`, when the server sent one.
    pub retry_after: Option<u64>,
    /// Strong/weak entity tag for conditional requests.
    pub etag: Option<String>,
    /// Last modification time for conditional requests.
    pub last_modified: Option<String>,
}

impl Reply {
    pub fn body_text(&self) -> String {
        String::from_utf8_lossy(&self.body).to_string()
    }

    pub fn is_success(&self) -> bool {
        (200..300).contains(&self.status)
    }
}

/// An agent with a total timeout. Status codes are *not* turned into errors so
/// the caller can react to 404/429 individually.
pub fn agent(timeout: Duration) -> ureq::Agent {
    ureq::Agent::config_builder()
        .user_agent(USER_AGENT)
        .http_status_as_error(false)
        .timeout_global(Some(timeout))
        .build()
        .into()
}

pub fn get(agent: &ureq::Agent, url: &str, headers: &[(&str, &str)], max_body: u64) -> Result<Reply> {
    let mut req = agent.get(url);
    for (name, value) in headers {
        req = req.header(*name, *value);
    }
    let response = req.call().map_err(map_error)?;
    read_reply(response, max_body)
}

/// GET with bounded retries for transient failures (timeouts, 5xx, 429).
///
/// `attempts` includes the first request. `Retry-After` is honoured, but
/// capped so a hostile or broken server cannot stall an install indefinitely.
pub fn get_retry(
    agent: &ureq::Agent,
    url: &str,
    headers: &[(&str, &str)],
    max_body: u64,
    attempts: u32,
) -> Result<Reply> {
    let attempts = attempts.max(1);
    let mut delay = Duration::from_millis(500);
    let mut last_error: Option<Error> = None;

    for attempt in 1..=attempts {
        match get(agent, url, headers, max_body) {
            Ok(reply) if attempt < attempts && is_retryable_status(reply.status) => {
                let wait = reply
                    .retry_after
                    .map(Duration::from_secs)
                    .unwrap_or(delay)
                    .min(Duration::from_secs(15));
                last_error = Some(Error::Http(format!(
                    "{url} replied with HTTP {}",
                    reply.status
                )));
                sleep(wait);
                delay = (delay * 2).min(Duration::from_secs(8));
            }
            Ok(reply) => return Ok(reply),
            Err(error) if attempt < attempts => {
                last_error = Some(error);
                sleep(delay);
                delay = (delay * 2).min(Duration::from_secs(8));
            }
            Err(error) => return Err(error),
        }
    }

    Err(last_error.unwrap_or_else(|| Error::Http(format!("{url} failed"))))
}

pub fn post_form(
    agent: &ureq::Agent,
    url: &str,
    headers: &[(&str, &str)],
    form: &[(&str, &str)],
    max_body: u64,
) -> Result<Reply> {
    let mut req = agent.post(url);
    for (name, value) in headers {
        req = req.header(*name, *value);
    }
    let response = req.send_form(form.iter().copied()).map_err(map_error)?;
    read_reply(response, max_body)
}

pub fn is_retryable_status(status: u16) -> bool {
    matches!(status, 408 | 425 | 429 | 500 | 502 | 503 | 504 | 522 | 524)
}

fn read_reply(mut response: ureq::http::Response<ureq::Body>, max_body: u64) -> Result<Reply> {
    let status = response.status().as_u16();
    let header = |name: &str| {
        response
            .headers()
            .get(name)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string)
    };
    let retry_after = header("retry-after").and_then(|v| v.trim().parse().ok());
    let etag = header("etag");
    let last_modified = header("last-modified");
    let body = response
        .body_mut()
        .with_config()
        .limit(max_body)
        .read_to_vec()
        .map_err(|e| Error::Http(e.to_string()))?;
    Ok(Reply {
        status,
        body,
        retry_after,
        etag,
        last_modified,
    })
}

fn map_error(error: ureq::Error) -> Error {
    match error {
        ureq::Error::StatusCode(code) => Error::Http(format!("HTTP status {code}")),
        other => Error::Http(other.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_conversion_never_fails() {
        let reply = Reply {
            status: 200,
            body: vec![0xff, b'a'],
            retry_after: None,
            etag: None,
            last_modified: None,
        };
        assert!(reply.body_text().contains('a'));
        assert!(reply.is_success());
    }

    #[test]
    fn retries_only_transient_statuses() {
        for status in [408, 425, 429, 500, 502, 503, 504, 522, 524] {
            assert!(is_retryable_status(status), "{status}");
        }
        for status in [200, 301, 304, 400, 401, 403, 404, 422] {
            assert!(!is_retryable_status(status), "{status}");
        }
    }
}
