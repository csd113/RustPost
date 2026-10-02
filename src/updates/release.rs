use anyhow::Context as _;
use ring::signature::{ED25519, UnparsedPublicKey};
use semver::Version;
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use std::io::Read as _;
use std::time::{Duration, Instant};

const API: &str = "https://api.github.com/repos/csd113/RustPost/releases?per_page=100";
const DOWNLOAD: &str = "https://github.com/csd113/RustPost/releases/download/";
pub(super) const MAX_ARTIFACT: u64 = 256 * 1024 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub format: u16,
    pub version: String,
    pub release_id: u64,
    pub target: String,
    pub filename: String,
    pub size: u64,
    pub sha256: String,
    pub executable_sha256: String,
    pub executable_size: u64,
    pub schema: i64,
    pub minimum_schema: i64,
    pub minimum_updater: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Release {
    pub id: u64,
    pub version: String,
    pub published_at: String,
    pub notes: String,
    pub manifest: Option<Manifest>,
    pub size: Option<u64>,
    pub verification: String,
    pub compatible: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Discovery {
    UpToDate,
    Available(Box<Release>),
    UnableToCheck(String),
}

#[derive(Debug, Deserialize)]
struct GithubRelease {
    id: u64,
    tag_name: String,
    draft: bool,
    prerelease: bool,
    published_at: Option<String>,
    body: Option<String>,
    assets: Vec<Asset>,
}
#[derive(Debug, Deserialize)]
struct Asset {
    name: String,
    size: u64,
    browser_download_url: String,
}

#[must_use]
pub fn platform_target() -> Option<&'static str> {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("linux", "x86_64") if cfg!(target_env = "gnu") => Some("x86_64-unknown-linux-gnu"),
        ("linux", "aarch64") if cfg!(target_env = "gnu") => Some("aarch64-unknown-linux-gnu"),
        ("macos", "aarch64") => Some("aarch64-apple-darwin"),
        ("windows", "x86_64") if cfg!(target_env = "msvc") => Some("x86_64-pc-windows-msvc"),
        _ => None,
    }
}

pub(super) fn stable_version(text: &str) -> anyhow::Result<Version> {
    let version = Version::parse(text.strip_prefix('v').unwrap_or(text))?;
    anyhow::ensure!(
        version.pre.is_empty() && version.build.is_empty(),
        "release version must be stable without build metadata"
    );
    Ok(version)
}

pub(super) fn asset_url(version: &str, name: &str) -> anyhow::Result<String> {
    let version = stable_version(version)?;
    anyhow::ensure!(
        name.len() <= 160
            && !name.is_empty()
            && name
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"-._".contains(&byte)),
        "invalid release asset name"
    );
    Ok(format!("{DOWNLOAD}v{version}/{name}"))
}

fn agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .user_agent("RustPost-updater/1")
        .max_redirects(0)
        .timeout_global(Some(Duration::from_secs(30)))
        .timeout_connect(Some(Duration::from_secs(5)))
        .build()
        .into()
}

// GitHub redirects release downloads to its HTTPS release asset CDN. Follow
// each hop ourselves so ureq can never contact an arbitrary redirect target.
pub(super) fn fetch(url: &str, limit: u64) -> anyhow::Result<Vec<u8>> {
    fetch_before(url, limit, Instant::now() + Duration::from_secs(30))
}

fn fetch_before(url: &str, limit: u64, deadline: Instant) -> anyhow::Result<Vec<u8>> {
    let mut url = url.to_owned();
    let client = agent();
    for _ in 0..4 {
        validate_source(&url)?;
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .context("release request timed out")?;
        let mut response = client
            .get(&url)
            .config()
            .timeout_global(Some(remaining))
            .build()
            .header("Accept", "application/vnd.github+json")
            .call()?;
        if response.status().is_redirection() {
            response
                .headers()
                .get("location")
                .context("release redirect has no location")?
                .to_str()?
                .clone_into(&mut url);
            continue;
        }
        anyhow::ensure!(
            response.status().is_success(),
            "GitHub release request failed"
        );
        let mut bytes = Vec::new();
        response
            .body_mut()
            .as_reader()
            .take(limit + 1)
            .read_to_end(&mut bytes)?;
        anyhow::ensure!(
            u64::try_from(bytes.len())? <= limit,
            "release response exceeds size limit"
        );
        return Ok(bytes);
    }
    anyhow::bail!("too many release download redirects")
}

pub(super) fn validate_source(url: &str) -> anyhow::Result<()> {
    // Parse with the HTTP library rather than prefix-checking a hostname.
    let uri: axum::http::Uri = url.parse()?;
    anyhow::ensure!(
        uri.scheme_str() == Some("https") && uri.port_u16().is_none(),
        "release source must use HTTPS"
    );
    let permitted = match uri.host() {
        Some("api.github.com") => uri.path() == "/repos/csd113/RustPost/releases",
        Some("github.com") => uri
            .path()
            .starts_with("/csd113/RustPost/releases/download/v"),
        Some("release-assets.githubusercontent.com") => {
            uri.path().starts_with("/github-production-release-asset/")
        }
        _ => false,
    };
    anyhow::ensure!(permitted, "release source is not allowlisted");
    Ok(())
}

pub(super) fn hex(bytes: &[u8]) -> String {
    {
        const DIGITS: &[u8; 16] = b"0123456789abcdef";
        let mut text = String::with_capacity(bytes.len() * 2);
        for byte in bytes {
            text.push(char::from(DIGITS[usize::from(byte >> 4)]));
            text.push(char::from(DIGITS[usize::from(byte & 15)]));
        }
        text
    }
}

pub(super) fn digest(bytes: &[u8]) -> String {
    hex(&Sha256::digest(bytes))
}

pub(super) fn verify_manifest(
    bytes: &[u8],
    signature: &[u8],
    key: &[u8],
    version: &str,
    release_id: u64,
    target: &str,
) -> anyhow::Result<Manifest> {
    UnparsedPublicKey::new(&ED25519, key)
        .verify(bytes, signature)
        .map_err(|_| anyhow::anyhow!("release signature failed verification"))?;
    let manifest: Manifest = serde_json::from_slice(bytes)?;
    anyhow::ensure!(
        manifest.format == 1
            && manifest.version == stable_version(version)?.to_string()
            && manifest.release_id == release_id
            && manifest.target == target,
        "release manifest identity is incompatible"
    );
    anyhow::ensure!(
        Version::parse(&manifest.minimum_updater)? <= Version::parse(super::VERSION)?,
        "release requires a newer updater"
    );
    anyhow::ensure!(
        manifest.size > 0
            && manifest.size <= MAX_ARTIFACT
            && manifest.executable_size > 0
            && manifest.executable_size <= MAX_ARTIFACT
            && manifest.minimum_schema > 0
            && manifest.minimum_schema <= manifest.schema,
        "invalid release manifest limits"
    );
    for hash in [&manifest.sha256, &manifest.executable_sha256] {
        anyhow::ensure!(
            hash.len() == 64
                && hash
                    .bytes()
                    .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase()),
            "invalid release checksum"
        );
    }
    anyhow::ensure!(
        manifest.filename == format!("rustpost-update-{target}.tar.gz"),
        "unexpected release artifact name"
    );
    Ok(manifest)
}

pub fn discover(current: &str, key: Option<&[u8]>) -> Discovery {
    discover_for_schema(current, key, None)
}
pub(super) fn discover_for_schema(
    current: &str,
    key: Option<&[u8]>,
    schema: Option<i64>,
) -> Discovery {
    match discover_inner(current, key, schema) {
        Ok(result) => result,
        Err(error) => Discovery::UnableToCheck(format!("Unable to check releases: {error}")),
    }
}
fn candidates(bytes: &[u8], current: &str) -> anyhow::Result<Vec<GithubRelease>> {
    let current = stable_version(current)?;
    let releases: Vec<GithubRelease> = serde_json::from_slice(bytes)?;
    let mut releases: Vec<_> = releases
        .into_iter()
        .filter(|r| !r.draft && !r.prerelease && r.published_at.is_some())
        .filter_map(|r| stable_version(&r.tag_name).ok().map(|v| (v, r)))
        .filter(|(v, _)| *v > current)
        .collect();
    releases.sort_by(|(a, _), (b, _)| b.cmp(a));
    Ok(releases.into_iter().map(|(_, r)| r).collect())
}
#[cfg(test)]
fn latest(bytes: &[u8], current: &str) -> anyhow::Result<Option<GithubRelease>> {
    Ok(candidates(bytes, current)?.into_iter().next())
}
fn discover_inner(
    current: &str,
    key: Option<&[u8]>,
    schema: Option<i64>,
) -> anyhow::Result<Discovery> {
    let deadline = Instant::now() + Duration::from_secs(30);
    let releases = candidates(&fetch_before(API, 2 * 1024 * 1024, deadline)?, current)?;
    let mut latest_unusable = None;
    for release in releases {
        let mut result = inspect_release(release, key, deadline)?;
        if result
            .manifest
            .as_ref()
            .is_some_and(|m| schema.is_some_and(|s| s < m.minimum_schema || s > m.schema))
        {
            result.compatible = false;
            result.manifest = None;
            result.verification = "Incompatible release: the installed database schema is outside its migration range.".into();
        }
        if result.compatible || key.is_none() {
            return Ok(Discovery::Available(Box::new(result)));
        }
        if latest_unusable.is_none() {
            latest_unusable = Some(result);
        }
        if Instant::now() >= deadline {
            break;
        }
    }
    Ok(latest_unusable.map_or(Discovery::UpToDate, |r| Discovery::Available(Box::new(r))))
}
fn bounded_notes(mut text: String) -> String {
    const LIMIT: usize = 32 * 1024;
    if text.len() > LIMIT {
        let mut boundary = LIMIT;
        while !text.is_char_boundary(boundary) {
            boundary -= 1;
        }
        text.truncate(boundary);
        text.push_str("\n[Release notes truncated; see the official release for the remainder.]");
    }
    text
}
fn inspect_release(
    release: GithubRelease,
    key: Option<&[u8]>,
    deadline: Instant,
) -> anyhow::Result<Release> {
    let version = stable_version(&release.tag_name)?.to_string();
    let mut result = Release {
        id: release.id,
        version,
        published_at: release
            .published_at
            .and_then(|date| chrono::DateTime::parse_from_rfc3339(&date).ok())
            .map_or_else(
                || "Release date unavailable".into(),
                |date| date.to_rfc3339(),
            ),
        notes: bounded_notes(release.body.unwrap_or_default()),
        manifest: None,
        size: None,
        verification: "No trusted signing key configured; installation disabled.".into(),
        compatible: false,
    };
    let Some(target) = platform_target() else {
        result.verification = "Unsupported operating system or architecture.".into();
        return Ok(result);
    };
    let legacy_name = match target {
        "x86_64-unknown-linux-gnu" => "rustpost-linux-x86_64.tar.gz",
        "aarch64-unknown-linux-gnu" => "rustpost-linux-aarch64.tar.gz",
        "aarch64-apple-darwin" => "rustpost-macos-aarch64.tar.gz",
        _ => "rustpost-windows-x86_64.zip",
    };
    result.size = release
        .assets
        .iter()
        .find(|asset| {
            asset.name == format!("rustpost-update-{target}.tar.gz") || asset.name == legacy_name
        })
        .map(|asset| asset.size);
    let Some(key) = key else {
        return Ok(result);
    };
    let verified = (|| {
        let name = format!("rustpost-update-{target}.json");
        let find = |name: &str| -> anyhow::Result<&Asset> {
            let matching: Vec<_> = release.assets.iter().filter(|a| a.name == name).collect();
            anyhow::ensure!(
                matching.len() == 1,
                "release is missing a unique compatible artifact"
            );
            let asset = matching[0];
            anyhow::ensure!(
                asset.browser_download_url == asset_url(&result.version, name)?,
                "disallowed release asset source"
            );
            Ok(asset)
        };
        find(&name)?;
        find(&format!("{name}.sig"))?;
        let manifest = verify_manifest(
            &fetch_before(&asset_url(&result.version, &name)?, 16 * 1024, deadline)?,
            &fetch_before(
                &asset_url(&result.version, &format!("{name}.sig"))?,
                64,
                deadline,
            )?,
            key,
            &result.version,
            result.id,
            target,
        )?;
        let artifact = find(&manifest.filename)?;
        anyhow::ensure!(
            artifact.size == manifest.size,
            "release artifact size does not match manifest"
        );
        Ok::<_, anyhow::Error>(manifest)
    })();
    match verified {
        Ok(manifest) => {
            result.size = Some(manifest.size);
            result.compatible = true;
            result.verification =
                "Ed25519 signature verified; SHA-256 will be checked before installation.".into();
            result.manifest = Some(manifest);
        }
        Err(error) => {
            let detail = error.to_string();
            let label = if detail.contains("compatible") || detail.contains("newer updater") {
                "Incompatible release"
            } else {
                "Release failed verification"
            };
            result.verification = format!("{label}: {error}");
        }
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn notes_are_utf8_bounded_before_journal_serialization() {
        let notes = bounded_notes("é".repeat(40_000));
        assert!(notes.len() < 33 * 1024);
        assert!(notes.ends_with("remainder.]"));
        assert!(std::str::from_utf8(notes.as_bytes()).is_ok());
    }

    #[test]
    fn stable_ordering_filters_prereleases_malformed_and_drafts() {
        let bytes = br#"[{"id":1,"tag_name":"v1.10.0","draft":false,"prerelease":false,"published_at":"today","body":null,"assets":[]},{"id":2,"tag_name":"v2.0.0-rc.1","draft":false,"prerelease":true,"published_at":"today","body":null,"assets":[]},{"id":3,"tag_name":"garbage","draft":false,"prerelease":false,"published_at":"today","body":null,"assets":[]}]"#;
        assert_eq!(latest(bytes, "1.9.0").expect("parse").expect("newer").id, 1);
        assert!(latest(bytes, "1.10.0").expect("parse").is_none());
        assert!(latest(bytes, "1.11.0").expect("parse").is_none());
        assert!(latest(b"not JSON", "1.0.0").is_err());
    }
    #[test]
    fn signed_manifest_integrity_identity_and_limits_fail_closed() {
        use ring::{
            rand::SystemRandom,
            signature::{Ed25519KeyPair, KeyPair as _},
        };
        let random = SystemRandom::new();
        let document = Ed25519KeyPair::generate_pkcs8(&random).expect("ephemeral test key");
        let key = Ed25519KeyPair::from_pkcs8(document.as_ref()).expect("test key");
        let target = "x86_64-unknown-linux-gnu";
        let manifest = Manifest {
            format: 1,
            version: "1.1.0".into(),
            release_id: 42,
            target: target.into(),
            filename: format!("rustpost-update-{target}.tar.gz"),
            size: 100,
            sha256: "a".repeat(64),
            executable_sha256: "b".repeat(64),
            executable_size: 200,
            schema: 5,
            minimum_schema: 4,
            minimum_updater: "1.0.0".into(),
        };
        let bytes = serde_json::to_vec(&manifest).expect("manifest");
        let signature = key.sign(&bytes);
        assert!(
            verify_manifest(
                &bytes,
                signature.as_ref(),
                key.public_key().as_ref(),
                "1.1.0",
                42,
                target
            )
            .is_ok()
        );
        for (version, id, expected_target) in [
            ("1.2.0", 42, target),
            ("1.1.0", 43, target),
            ("1.1.0", 42, "aarch64-unknown-linux-gnu"),
        ] {
            assert!(
                verify_manifest(
                    &bytes,
                    signature.as_ref(),
                    key.public_key().as_ref(),
                    version,
                    id,
                    expected_target
                )
                .is_err()
            );
        }
        assert!(
            verify_manifest(
                b"malformed",
                signature.as_ref(),
                key.public_key().as_ref(),
                "1.1.0",
                42,
                target
            )
            .is_err()
        );
        assert!(
            verify_manifest(
                &bytes,
                &[0; 64],
                key.public_key().as_ref(),
                "1.1.0",
                42,
                target
            )
            .is_err()
        );
        let malformed = b"{\"format\":1}";
        assert!(
            verify_manifest(
                malformed,
                key.sign(malformed).as_ref(),
                key.public_key().as_ref(),
                "1.1.0",
                42,
                target
            )
            .is_err()
        );
    }

    #[test]
    fn signed_manifest_rejects_invalid_limits_and_upgrade_requirements() {
        use ring::{
            rand::SystemRandom,
            signature::{Ed25519KeyPair, KeyPair as _},
        };
        let document = Ed25519KeyPair::generate_pkcs8(&SystemRandom::new()).expect("test key");
        let key = Ed25519KeyPair::from_pkcs8(document.as_ref()).expect("test key");
        let target = "x86_64-unknown-linux-gnu";
        let manifest = Manifest {
            format: 1,
            version: "1.1.0".into(),
            release_id: 42,
            target: target.into(),
            filename: format!("rustpost-update-{target}.tar.gz"),
            size: 100,
            sha256: "a".repeat(64),
            executable_sha256: "b".repeat(64),
            executable_size: 200,
            schema: 5,
            minimum_schema: 4,
            minimum_updater: "1.0.0".into(),
        };
        for altered in [
            Manifest {
                size: MAX_ARTIFACT + 1,
                ..manifest.clone()
            },
            Manifest {
                filename: "../binary".into(),
                ..manifest.clone()
            },
            Manifest {
                minimum_updater: "99.0.0".into(),
                ..manifest.clone()
            },
            Manifest {
                sha256: "bad".into(),
                ..manifest
            },
        ] {
            let bytes = serde_json::to_vec(&altered).expect("altered manifest");
            assert!(
                verify_manifest(
                    &bytes,
                    key.sign(&bytes).as_ref(),
                    key.public_key().as_ref(),
                    "1.1.0",
                    42,
                    target
                )
                .is_err()
            );
        }
    }

    #[test]
    fn sources_and_asset_names_are_strict() {
        for url in [
            "http://github.com/csd113/RustPost/releases/download/v1/x",
            "https://github.com.evil.test/csd113/RustPost/releases/download/v1/x",
            "https://github.com/other/repo/releases/download/v1/x",
            "https://github.com@evil.test/x",
            "https://github.com:443/csd113/RustPost/releases/download/v1/x",
        ] {
            assert!(validate_source(url).is_err(), "{url}");
        }
        assert!(asset_url("1.0.0", "../oops").is_err());
        assert!(asset_url("1.0.0-rc.1", "okay").is_err());
        assert!(validate_source(API).is_ok());
    }
}
