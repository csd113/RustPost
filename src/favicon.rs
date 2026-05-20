use std::path::{Path, PathBuf};

use anyhow::Context as _;
use axum::body::Body;
use axum::extract::multipart::Field;
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse as _, Response};
use tokio::io::AsyncWriteExt as _;
use uuid::Uuid;

use crate::media;
use crate::runtime::RuntimePaths;

const FAVICON_BASENAME: &str = "favicon";
const FAVICON_EXTENSIONS: [&str; 3] = ["ico", "png", "svg"];
const DEFAULT_FAVICON: &[u8] = &[
    0x00, 0x00, 0x01, 0x00, 0x01, 0x00, 0x01, 0x01, 0x00, 0x00, 0x01, 0x00, 0x20, 0x00, 0x45, 0x00,
    0x00, 0x00, 0x16, 0x00, 0x00, 0x00, 0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00,
    0x00, 0x0d, 0x49, 0x48, 0x44, 0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06,
    0x00, 0x00, 0x00, 0x1f, 0x15, 0xc4, 0x89, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x44, 0x41, 0x54, 0x78,
    0x9c, 0x63, 0xf8, 0xcf, 0xc0, 0x00, 0x00, 0x04, 0x00, 0x01, 0xfe, 0xa7, 0x69, 0x9d, 0x16, 0x00,
    0x00, 0x00, 0x00, 0x49, 0x45, 0x4e, 0x44, 0xae, 0x42, 0x60, 0x82,
];

// Favicons are small browser chrome assets. 256 KiB leaves room for ICO files
// with several embedded sizes while rejecting accidental large media uploads.
const MAX_FAVICON_BYTES: u64 = 256 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FaviconKind {
    Ico,
    Png,
    Svg,
}

impl FaviconKind {
    const fn extension(self) -> &'static str {
        match self {
            Self::Ico => "ico",
            Self::Png => "png",
            Self::Svg => "svg",
        }
    }

    pub const fn content_type(self) -> &'static str {
        match self {
            Self::Ico => "image/x-icon",
            Self::Png => "image/png",
            Self::Svg => "image/svg+xml; charset=utf-8",
        }
    }
}

#[derive(Debug, Clone)]
pub struct FaviconAsset {
    path: PathBuf,
    kind: FaviconKind,
    custom: bool,
}

impl FaviconAsset {
    pub fn state_label(&self) -> &'static str {
        if self.custom {
            "Custom favicon configured"
        } else {
            "Using built-in default favicon"
        }
    }

    pub const fn content_type(&self) -> &'static str {
        self.kind.content_type()
    }

    pub const fn is_custom(&self) -> bool {
        self.custom
    }
}

pub fn current(paths: &RuntimePaths) -> FaviconAsset {
    for ext in FAVICON_EXTENSIONS {
        let path = favicon_path(paths, ext);
        if path.is_file() {
            return FaviconAsset {
                path,
                kind: kind_from_extension(ext).unwrap_or(FaviconKind::Ico),
                custom: true,
            };
        }
    }
    FaviconAsset {
        path: PathBuf::new(),
        kind: FaviconKind::Ico,
        custom: false,
    }
}

pub async fn response(paths: &RuntimePaths) -> Response {
    let asset = current(paths);
    let (status, content_type, bytes) = if asset.custom {
        match tokio::fs::read(&asset.path).await {
            Ok(bytes) => (StatusCode::OK, asset.content_type(), bytes),
            Err(error) => {
                tracing::warn!(error = %error, "failed to read custom favicon; serving default");
                (
                    StatusCode::OK,
                    FaviconKind::Ico.content_type(),
                    DEFAULT_FAVICON.to_vec(),
                )
            }
        }
    } else {
        (
            StatusCode::OK,
            FaviconKind::Ico.content_type(),
            DEFAULT_FAVICON.to_vec(),
        )
    };
    let mut response = (status, Body::from(bytes)).into_response();
    let headers = response.headers_mut();
    headers.insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type));
    headers.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("public, max-age=3600"),
    );
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    response
}

pub async fn save_upload(paths: &RuntimePaths, mut field: Field<'_>) -> anyhow::Result<()> {
    let original_filename = field.file_name().unwrap_or("favicon").to_owned();
    media::reject_path_tricks(&original_filename)?;
    let kind = extension_kind(&original_filename)?;
    let staging = paths
        .tmp_uploads
        .join(format!("favicon-{}.upload", Uuid::new_v4().simple()));
    let mut file = tokio::fs::File::create(&staging).await?;
    let mut bytes = 0_u64;
    while let Some(chunk) = field.chunk().await? {
        bytes = bytes.saturating_add(u64::try_from(chunk.len())?);
        if bytes > MAX_FAVICON_BYTES {
            let _ = tokio::fs::remove_file(&staging).await;
            anyhow::bail!("favicon exceeds maximum size of 256 KiB");
        }
        file.write_all(&chunk).await?;
    }
    file.flush().await?;
    file.sync_all().await?;
    drop(file);

    let data = tokio::fs::read(&staging).await?;
    if let Err(error) = validate_bytes(kind, &data) {
        let _ = tokio::fs::remove_file(&staging).await;
        return Err(error);
    }

    tokio::fs::create_dir_all(&paths.assets_dir)
        .await
        .with_context(|| {
            format!(
                "failed to create asset directory {}",
                paths.assets_dir.display()
            )
        })?;
    let final_path = favicon_path(paths, kind.extension());
    if let Err(error) = tokio::fs::rename(&staging, &final_path).await {
        let _ = tokio::fs::remove_file(&staging).await;
        return Err(error.into());
    }
    remove_stale_favicons(paths, kind.extension()).await;
    Ok(())
}

pub async fn reset(paths: &RuntimePaths) -> anyhow::Result<()> {
    remove_stale_favicons(paths, "").await;
    Ok(())
}

async fn remove_stale_favicons(paths: &RuntimePaths, keep_extension: &str) {
    for ext in FAVICON_EXTENSIONS {
        if ext == keep_extension {
            continue;
        }
        let path = favicon_path(paths, ext);
        if let Err(error) = tokio::fs::remove_file(&path).await
            && error.kind() != std::io::ErrorKind::NotFound
        {
            tracing::debug!(path = %path.display(), error = %error, "failed to remove stale favicon");
        }
    }
}

fn favicon_path(paths: &RuntimePaths, ext: &str) -> PathBuf {
    paths.assets_dir.join(format!("{FAVICON_BASENAME}.{ext}"))
}

fn extension_kind(filename: &str) -> anyhow::Result<FaviconKind> {
    let ext = Path::new(filename)
        .extension()
        .and_then(|ext| ext.to_str())
        .map(str::to_ascii_lowercase)
        .ok_or_else(|| anyhow::anyhow!("favicon must be a .ico, .png, or .svg file"))?;
    kind_from_extension(&ext)
        .ok_or_else(|| anyhow::anyhow!("unsupported favicon type; upload .ico, .png, or .svg"))
}

fn kind_from_extension(ext: &str) -> Option<FaviconKind> {
    match ext {
        "ico" => Some(FaviconKind::Ico),
        "png" => Some(FaviconKind::Png),
        "svg" => Some(FaviconKind::Svg),
        _ => None,
    }
}

fn validate_bytes(kind: FaviconKind, data: &[u8]) -> anyhow::Result<()> {
    if data.is_empty() {
        anyhow::bail!("favicon file is empty");
    }
    match kind {
        FaviconKind::Png => validate_png(data),
        FaviconKind::Ico => validate_ico(data),
        FaviconKind::Svg => validate_svg(data),
    }
}

fn validate_png(data: &[u8]) -> anyhow::Result<()> {
    const PNG_SIGNATURE: &[u8; 8] = b"\x89PNG\r\n\x1a\n";
    if data.starts_with(PNG_SIGNATURE) {
        Ok(())
    } else {
        anyhow::bail!("PNG favicon has an invalid file signature")
    }
}

fn validate_ico(data: &[u8]) -> anyhow::Result<()> {
    if data.len() < 6 {
        anyhow::bail!("ICO favicon is too small");
    }
    let reserved = u16::from_le_bytes([data[0], data[1]]);
    let image_type = u16::from_le_bytes([data[2], data[3]]);
    let count = u16::from_le_bytes([data[4], data[5]]);
    if reserved == 0 && matches!(image_type, 1 | 2) && count > 0 {
        Ok(())
    } else {
        anyhow::bail!("ICO favicon has an invalid header")
    }
}

fn validate_svg(data: &[u8]) -> anyhow::Result<()> {
    let text = std::str::from_utf8(data).context("SVG favicon must be UTF-8 text")?;
    let lower = text.to_ascii_lowercase();
    let trimmed = lower.trim_start();
    if !trimmed.starts_with("<svg") && !trimmed.starts_with("<?xml") {
        anyhow::bail!("SVG favicon must start with an SVG document");
    }
    for blocked in [
        "<script",
        "<foreignobject",
        "javascript:",
        "data:text/html",
        " onload=",
        " onclick=",
        " onerror=",
        " onmouseover=",
        " href=\"data:",
        " href='data:",
    ] {
        if lower.contains(blocked) {
            anyhow::bail!("SVG favicon contains unsupported active content");
        }
    }
    if lower.contains("<svg") {
        Ok(())
    } else {
        anyhow::bail!("SVG favicon must contain an <svg> element")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_supported_magic_headers() {
        assert!(validate_bytes(FaviconKind::Png, b"\x89PNG\r\n\x1a\nrest").is_ok());
        assert!(validate_bytes(FaviconKind::Ico, &[0, 0, 1, 0, 1, 0]).is_ok());
        assert!(
            validate_bytes(
                FaviconKind::Svg,
                br#"<svg xmlns="http://www.w3.org/2000/svg"/>"#
            )
            .is_ok()
        );
    }

    #[test]
    fn rejects_mismatched_or_active_content() {
        assert!(validate_bytes(FaviconKind::Png, b"not png").is_err());
        assert!(validate_bytes(FaviconKind::Ico, &[1, 0, 1, 0, 0, 0]).is_err());
        assert!(
            validate_bytes(FaviconKind::Svg, br#"<svg><script>alert(1)</script></svg>"#).is_err()
        );
    }
}
