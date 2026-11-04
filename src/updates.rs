//! Read-only discovery of the latest published stable GitHub release.

use anyhow::Context as _;
use semver::Version;
use serde::Deserialize;
use std::io::Read as _;
use std::time::Duration;

const MAX_RESPONSE: u64 = 512 * 1024;
const API: &str = "https://api.github.com/repos/csd113/RustPost/releases/latest";
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Debug, Clone)]
pub enum Discovery {
    UpToDate,
    Available { version: String, url: String },
    UnableToCheck,
}

#[derive(Deserialize)]
struct GithubRelease {
    tag_name: String,
    draft: bool,
    prerelease: bool,
    published_at: Option<String>,
}

pub fn discover() -> Discovery {
    match latest_release() {
        Ok(discovery) => discovery,
        Err(error) => {
            tracing::warn!(error = %error, "GitHub release check failed");
            Discovery::UnableToCheck
        }
    }
}

fn latest_release() -> anyhow::Result<Discovery> {
    let client: ureq::Agent = ureq::Agent::config_builder()
        .user_agent(concat!("RustPost/", env!("CARGO_PKG_VERSION")))
        .max_redirects(0)
        .timeout_global(Some(Duration::from_secs(15)))
        .timeout_connect(Some(Duration::from_secs(5)))
        .build()
        .into();
    let mut response = client
        .get(API)
        .header("Accept", "application/vnd.github+json")
        .header("X-GitHub-Api-Version", "2022-11-28")
        .call()?;
    anyhow::ensure!(
        response.status().is_success(),
        "GitHub release request failed"
    );
    let mut bytes = Vec::new();
    response
        .body_mut()
        .as_reader()
        .take(MAX_RESPONSE + 1)
        .read_to_end(&mut bytes)?;
    anyhow::ensure!(
        u64::try_from(bytes.len())? <= MAX_RESPONSE,
        "GitHub release response is too large"
    );
    let release: GithubRelease = serde_json::from_slice(&bytes)?;
    anyhow::ensure!(
        !release.draft && !release.prerelease && release.published_at.is_some(),
        "GitHub did not return a published stable release"
    );
    let version = Version::parse(
        release
            .tag_name
            .strip_prefix('v')
            .unwrap_or(&release.tag_name),
    )
    .context("invalid GitHub release version")?;
    anyhow::ensure!(
        version.pre.is_empty() && version.build.is_empty(),
        "GitHub release is not stable"
    );
    if version <= Version::parse(VERSION)? {
        return Ok(Discovery::UpToDate);
    }
    // Construct the link from the validated tag, never a URL from API content.
    Ok(Discovery::Available {
        version: version.to_string(),
        url: format!(
            "https://github.com/csd113/RustPost/releases/tag/{}",
            release.tag_name
        ),
    })
}
