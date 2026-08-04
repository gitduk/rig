//! Version resolution for install/update flows. Read-only HTTP GETs — no
//! filesystem or process side effects.

pub mod crates_io;
pub mod github;
pub mod npm;

use anyhow::Context;
use serde::de::DeserializeOwned;

pub(super) const USER_AGENT: &str = concat!("rig/", env!("CARGO_PKG_VERSION"));

/// GET + parse, shared by all three version-lookup APIs — they differ only
/// in the URL and the response type.
pub(super) fn fetch_json<T: DeserializeOwned>(url: &str, what: &str) -> anyhow::Result<T> {
    let text = ureq::get(url)
        .header("User-Agent", USER_AGENT)
        .call()
        .with_context(|| format!("failed to query {url}"))?
        .body_mut()
        .read_to_string()
        .with_context(|| format!("failed to read response body from {url}"))?;
    serde_json::from_str(&text)
        .with_context(|| format!("failed to parse {what} response from {url}"))
}
