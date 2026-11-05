//! Probe for the media subsystem.

use super::*;

/// Reads width/height from the headers of the image formats `RustPost` accepts.
/// Returns `None` for unknown or truncated headers.
pub(super) fn image_dimensions(data: &[u8]) -> Option<(u32, u32)> {
    const PNG_SIGNATURE: &[u8] = b"\x89PNG\r\n\x1a\n";
    if data.starts_with(PNG_SIGNATURE) {
        let width = read_be_u32(data, 16)?;
        let height = read_be_u32(data, 20)?;
        return Some((width, height));
    }
    if data.starts_with(b"GIF87a") || data.starts_with(b"GIF89a") {
        let width = read_le_u16(data, 6)?;
        let height = read_le_u16(data, 8)?;
        return Some((u32::from(width), u32::from(height)));
    }
    if data.starts_with(&[0xFF, 0xD8]) {
        return jpeg_dimensions(data);
    }
    if data.len() >= 30 && data.starts_with(b"RIFF") && &data[8..12] == b"WEBP" {
        return match &data[12..16] {
            b"VP8X" => {
                let width = read_le_u24(data, 24)? + 1;
                let height = read_le_u24(data, 27)? + 1;
                Some((width, height))
            }
            b"VP8 " => {
                let width = u32::from(read_le_u16(data, 26)? & 0x3FFF);
                let height = u32::from(read_le_u16(data, 28)? & 0x3FFF);
                Some((width, height))
            }
            b"VP8L" => {
                let bits = read_le_u32(data, 21)?;
                let width = (bits & 0x3FFF) + 1;
                let height = ((bits >> 14) & 0x3FFF) + 1;
                Some((width, height))
            }
            _ => None,
        };
    }
    None
}

pub(super) fn read_be_u32(data: &[u8], offset: usize) -> Option<u32> {
    let bytes = data.get(offset..offset + 4)?;
    Some(u32::from_be_bytes(bytes.try_into().ok()?))
}

pub(super) fn read_le_u32(data: &[u8], offset: usize) -> Option<u32> {
    let bytes = data.get(offset..offset + 4)?;
    Some(u32::from_le_bytes(bytes.try_into().ok()?))
}

pub(super) fn read_le_u16(data: &[u8], offset: usize) -> Option<u16> {
    let bytes = data.get(offset..offset + 2)?;
    Some(u16::from_le_bytes(bytes.try_into().ok()?))
}

pub(super) fn read_le_u24(data: &[u8], offset: usize) -> Option<u32> {
    let bytes = data.get(offset..offset + 3)?;
    Some(u32::from(bytes[0]) | (u32::from(bytes[1]) << 8) | (u32::from(bytes[2]) << 16))
}

pub(super) fn jpeg_dimensions(data: &[u8]) -> Option<(u32, u32)> {
    let mut index = 2usize;
    while index + 9 < data.len() {
        if data[index] != 0xFF {
            index += 1;
            continue;
        }
        let marker = data[index + 1];
        if matches!(marker, 0xD8 | 0xD9 | 0x01) || (0xD0..=0xD7).contains(&marker) {
            index += 2;
            continue;
        }
        let length = usize::from(u16::from_be_bytes([data[index + 2], data[index + 3]]));
        if length < 2 {
            return None;
        }
        // Start-of-frame markers (excluding DHT/JPG/DAC at C4/C8/CC).
        let is_sof = matches!(marker, 0xC0..=0xC3 | 0xC5..=0xC7 | 0xC9..=0xCB | 0xCD..=0xCF);
        if is_sof {
            let height = u32::from(u16::from_be_bytes([data[index + 5], data[index + 6]]));
            let width = u32::from(u16::from_be_bytes([data[index + 7], data[index + 8]]));
            return Some((width, height));
        }
        index += 2 + length;
    }
    None
}

/// Streams a staged upload once, sniffing its magic bytes and hashing it
/// without buffering the whole file. The bounded prefix is also reused for
/// image dimension checks.
pub(super) async fn sniff_and_hash(
    path: &Path,
) -> anyhow::Result<(Option<infer::Type>, String, Vec<u8>)> {
    use tokio::io::AsyncReadExt as _;

    let mut file = tokio::fs::File::open(path).await?;
    let mut buffer = vec![0_u8; MEDIA_SNIFF_PREFIX_BYTES];
    let mut prefix = Vec::with_capacity(MEDIA_SNIFF_PREFIX_BYTES);
    let mut hasher = Sha256::new();
    loop {
        let read = file.read(&mut buffer).await?;
        if read == 0 {
            break;
        }
        if prefix.len() < MEDIA_SNIFF_PREFIX_BYTES {
            let take = (MEDIA_SNIFF_PREFIX_BYTES - prefix.len()).min(read);
            prefix.extend_from_slice(&buffer[..take]);
        }
        hasher.update(&buffer[..read]);
    }
    let kind = infer::get(&prefix);
    let hash = hex_lower(hasher.finalize().as_ref());
    Ok((kind, hash, prefix))
}
