//! Online firmware version check.
//!
//! NinjaForce compares versions server-side; the protocol itself exposes no
//! version feed. This calls the same endpoint their web panel does. Note the
//! endpoint is undocumented and could change or vanish without warning, so every
//! failure here is soft -- callers still get the installed versions.

use std::time::Duration;

const API: &str = "https://api.ninjaforce.co/firmware/get_latest_version";

/// Not a credential: a constant shipped in NinjaForce's public JavaScript.
const AUTH: &str = "ninjaforce:win";

#[derive(serde::Deserialize)]
struct Reply {
    #[serde(default)]
    success: bool,
    #[serde(default)]
    data: Option<String>,
}

/// Latest firmware string for a USB product id, or `None` if unknown.
///
/// The endpoint wants the pid in decimal; hex spellings return 404.
pub fn latest_for(product_id: u16, timeout: Duration) -> Option<String> {
    let reply: Reply = ureq::get(&format!("{API}?pid={}", product_id))
        .set("Authorization", AUTH)
        .timeout(timeout)
        .call()
        .ok()?
        .into_json()
        .ok()?;
    if !reply.success {
        return None;
    }
    reply
        .data
        .filter(|value| !value.is_empty())
        .map(|value| value.to_uppercase())
}
