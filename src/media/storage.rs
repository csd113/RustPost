//! Storage for the media subsystem.

use super::*;

pub(super) fn safe_stored_media_path(
    paths: &RuntimePaths,
    path: &Path,
    media_kind: MediaKind,
) -> bool {
    if !path.is_absolute() || has_parent_component(path) {
        return false;
    }
    let allowed_roots = match media_kind {
        MediaKind::Image => [&paths.uploads_images, &paths.uploads_originals],
        MediaKind::Video => [&paths.uploads_videos, &paths.uploads_originals],
    };
    allowed_roots.iter().any(|root| path.starts_with(root))
}

pub(super) async fn hash_file(path: &Path) -> anyhow::Result<String> {
    use tokio::io::AsyncReadExt as _;

    let mut file = tokio::fs::File::open(path).await?;
    let mut buffer = vec![0_u8; MEDIA_SNIFF_PREFIX_BYTES];
    let mut hasher = Sha256::new();
    loop {
        let read = file.read(&mut buffer).await?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hex_lower(hasher.finalize().as_ref()))
}

pub(super) fn hex_lower(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut encoded = String::with_capacity(bytes.len() * 2);
    for &byte in bytes {
        encoded.push(char::from(HEX[usize::from(byte >> 4)]));
        encoded.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    encoded
}

pub(super) fn stable_media_basename(original_filename: &str, original_sha256: &str) -> String {
    let stem = Path::new(original_filename)
        .file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or("upload");
    let mut sanitized = String::with_capacity(stem.len().min(64) + 1 + HASH_PREFIX_LEN);
    let mut last_was_separator = false;
    for character in stem.chars().flat_map(char::to_lowercase) {
        let next = if character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.') {
            Some(character)
        } else if character.is_ascii_whitespace() {
            Some('-')
        } else {
            None
        };
        let Some(next) = next else {
            continue;
        };
        if matches!(next, '-' | '_' | '.') {
            if sanitized.is_empty() || last_was_separator {
                continue;
            }
            last_was_separator = true;
        } else {
            last_was_separator = false;
        }
        sanitized.push(next);
        if sanitized.len() >= 64 {
            break;
        }
    }
    while sanitized.ends_with(['-', '_', '.']) {
        sanitized.pop();
    }
    if sanitized.is_empty() {
        sanitized.push_str("upload");
    }
    let prefix_len = original_sha256.len().min(HASH_PREFIX_LEN);
    format!("{}-{}", sanitized, &original_sha256[..prefix_len])
}

/// Picks an unused output path and claims it atomically.
///
/// Concurrent uploads of identical bytes derive the same basename, so a plain
/// `exists()` check would let two conversions target the same file. Creating
/// the file with `create_new` claims the name at the filesystem level; callers
/// overwrite or rename over the claimed (empty) file.
pub(super) fn unique_media_path(
    dir: &Path,
    basename: &str,
    extension: &str,
) -> anyhow::Result<PathBuf> {
    for attempt in 0..8 {
        let candidate = if attempt == 0 {
            dir.join(format!("{basename}.{extension}"))
        } else {
            dir.join(format!(
                "{basename}-{}.{extension}",
                Uuid::new_v4().simple()
            ))
        };
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&candidate)
        {
            Ok(_) => return Ok(candidate),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error.into()),
        }
    }
    anyhow::bail!("unable to reserve an unused media filename")
}

pub(super) fn public_upload_path(paths: &RuntimePaths, path: &Path) -> anyhow::Result<String> {
    let filename = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| anyhow::anyhow!("media path has no file name"))?;
    if path.starts_with(&paths.uploads_originals) {
        return Ok(format!("/uploads/originals/{filename}"));
    }
    if path.starts_with(&paths.uploads_images) {
        return Ok(format!("/uploads/images/{filename}"));
    }
    if path.starts_with(&paths.uploads_videos) {
        return Ok(format!("/uploads/videos/{filename}"));
    }
    if path.starts_with(&paths.uploads_thumbs) {
        return Ok(format!("/uploads/thumbs/{filename}"));
    }
    anyhow::bail!("media path is outside upload directories");
}

pub(super) fn has_parent_component(path: &Path) -> bool {
    path.components()
        .any(|component| matches!(component, Component::ParentDir))
}

pub fn reject_path_tricks(filename: &str) -> anyhow::Result<()> {
    let path = Path::new(filename);
    if path.is_absolute()
        || filename.contains('/')
        || filename.contains('\\')
        || filename.contains("..")
        || filename.contains(':')
        || filename.chars().any(char::is_control)
    {
        anyhow::bail!("unsafe upload filename");
    }
    Ok(())
}

pub(super) fn safe_extension(mime: &str, kind: MediaKind) -> &'static str {
    match (kind, mime) {
        (MediaKind::Image, "image/jpeg") => "jpg",
        (MediaKind::Image, "image/png") => "png",
        (MediaKind::Image, "image/gif") => "gif",
        (MediaKind::Image, "image/webp") => "webp",
        (MediaKind::Video, "video/webm") => "webm",
        (MediaKind::Video, "video/quicktime") => "mov",
        (MediaKind::Video, _) => "mp4",
        (MediaKind::Image, _) => "img",
    }
}
