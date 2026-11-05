use std::path::Path;
use std::process::{Output, Stdio};
use std::time::Duration;

use tokio::io::AsyncReadExt as _;
use tokio::process::Command;
use tokio::sync::Semaphore;

/// Bound simultaneous CPU-heavy decoders and their aggregate memory use.
static CONVERSIONS: Semaphore = Semaphore::const_new(2);
const STDERR_LIMIT: usize = 64 * 1024;
const STDOUT_LIMIT: usize = 256 * 1024;
use tokio::time::timeout;

use crate::config::MediaSettings;

#[derive(Debug, Clone)]
pub struct FfmpegStatus {
    pub available: bool,
    pub version: String,
    pub supports_webp: bool,
    pub supports_vp9: bool,
    pub error: Option<String>,
}

impl FfmpegStatus {
    #[must_use]
    pub fn summary(&self) -> String {
        if self.available {
            format!(
                "available {}; webp={}; vp9={}",
                self.version, self.supports_webp, self.supports_vp9
            )
        } else {
            format!(
                "unavailable: {}",
                self.error.as_deref().unwrap_or("not found")
            )
        }
    }
}

pub async fn probe(settings: &MediaSettings) -> FfmpegStatus {
    let output = run_ffmpeg(
        &settings.ffmpeg_path,
        &["-version".to_owned()],
        Duration::from_secs(10),
    )
    .await;
    let output = match output {
        Ok(output) => output,
        Err(error) => {
            return FfmpegStatus {
                available: false,
                version: String::new(),
                supports_webp: false,
                supports_vp9: false,
                error: Some(format!("ffmpeg probe failed: {error}")),
            };
        }
    };
    if !output.status.success() {
        return FfmpegStatus {
            available: false,
            version: String::new(),
            supports_webp: false,
            supports_vp9: false,
            error: Some("ffmpeg -version failed".to_owned()),
        };
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let version = stdout.lines().next().unwrap_or("ffmpeg").to_owned();
    let encoders = run_ffmpeg(
        &settings.ffmpeg_path,
        &["-hide_banner".to_owned(), "-encoders".to_owned()],
        Duration::from_secs(10),
    )
    .await;
    let encoder_text = encoders
        .ok()
        .map(|out| String::from_utf8_lossy(&out.stdout).to_string())
        .unwrap_or_default();
    FfmpegStatus {
        available: true,
        version,
        supports_webp: encoder_text.contains("libwebp") || encoder_text.contains(" webp "),
        supports_vp9: encoder_text.contains("libvpx-vp9") || encoder_text.contains(" vp9 "),
        error: None,
    }
}

pub async fn convert_image_to_webp(
    settings: &MediaSettings,
    input: &Path,
    output: &Path,
) -> anyhow::Result<String> {
    let args = image_webp_args(settings, input, output);
    let result = run_ffmpeg(&settings.ffmpeg_path, &args, Duration::from_mins(2)).await?;
    if result.status.success() {
        validate_output(output, settings.max_image_size, "image/webp").await?;
        Ok(stderr_summary(&result.stderr))
    } else {
        anyhow::bail!(stderr_summary(&result.stderr));
    }
}

pub async fn convert_image_to_webp_thumbnail(
    settings: &MediaSettings,
    input: &Path,
    output: &Path,
    size: u16,
) -> anyhow::Result<String> {
    let args = image_webp_thumbnail_args(settings, input, output, size);
    let result = run_ffmpeg(&settings.ffmpeg_path, &args, Duration::from_mins(2)).await?;
    if result.status.success() {
        validate_output(output, settings.max_image_size, "image/webp").await?;
        Ok(stderr_summary(&result.stderr))
    } else {
        anyhow::bail!(stderr_summary(&result.stderr));
    }
}

pub async fn convert_video_to_webm(
    settings: &MediaSettings,
    input: &Path,
    output: &Path,
) -> anyhow::Result<String> {
    let args = video_webm_args(settings, input, output);
    let result = run_ffmpeg(&settings.ffmpeg_path, &args, Duration::from_mins(5)).await?;
    if result.status.success() {
        validate_output(output, settings.max_video_size, "video/webm").await?;
        Ok(stderr_summary(&result.stderr))
    } else {
        anyhow::bail!(stderr_summary(&result.stderr));
    }
}

async fn run_ffmpeg(path: &str, args: &[String], duration: Duration) -> anyhow::Result<Output> {
    let _permit = timeout(Duration::from_secs(30), CONVERSIONS.acquire())
        .await
        .map_err(|_| anyhow::anyhow!("media processing is busy; try again shortly"))??;
    let mut child = Command::new(path)
        .args(args)
        .stdin(Stdio::null())
        .kill_on_drop(true)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow::anyhow!("ffmpeg stdout unavailable"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| anyhow::anyhow!("ffmpeg stderr unavailable"))?;
    let result = timeout(duration, async {
        tokio::try_join!(
            child.wait(),
            read_bounded_output(stdout, STDOUT_LIMIT, false),
            read_bounded_output(stderr, STDERR_LIMIT, true)
        )
    })
    .await;
    match result {
        Ok(Ok((status, stdout, stderr))) => Ok(Output {
            status,
            stdout,
            stderr,
        }),
        Ok(Err(error)) => {
            let _ = child.kill().await;
            let _ = child.wait().await;
            Err(error.into())
        }
        Err(_) => {
            let _ = child.kill().await;
            let _ = child.wait().await;
            anyhow::bail!("ffmpeg timed out");
        }
    }
}

/// Drain continuously to avoid pipe deadlocks, retaining only a bounded tail.
async fn read_bounded_output<R: tokio::io::AsyncRead + Unpin>(
    mut reader: R,
    limit: usize,
    tail: bool,
) -> std::io::Result<Vec<u8>> {
    let mut output = Vec::with_capacity(limit);
    let mut buffer = vec![0u8; 8192];
    loop {
        let read = reader.read(&mut buffer).await?;
        if read == 0 {
            break;
        }
        if tail {
            let overflow = output.len().saturating_add(read).saturating_sub(limit);
            if overflow > 0 {
                output.drain(..overflow);
            }
            output.extend_from_slice(&buffer[..read]);
        } else {
            let remaining = limit.saturating_sub(output.len()).min(read);
            output.extend_from_slice(&buffer[..remaining]);
        }
    }
    Ok(output)
}

async fn validate_output(path: &Path, max_bytes: u64, mime: &str) -> anyhow::Result<()> {
    let metadata = tokio::fs::metadata(path).await?;
    anyhow::ensure!(
        metadata.is_file() && metadata.len() > 0 && metadata.len() < max_bytes,
        "media conversion produced an empty or oversized output"
    );
    let mut file = tokio::fs::File::open(path).await?;
    let mut prefix = [0u8; 4096];
    let read = file.read(&mut prefix).await?;
    anyhow::ensure!(
        infer::get(&prefix[..read]).is_some_and(|kind| kind.mime_type() == mime),
        "media conversion produced an invalid format"
    );
    Ok(())
}

fn safety_args() -> Vec<String> {
    [
        "-nostdin",
        "-hide_banner",
        "-loglevel",
        "error",
        "-protocol_whitelist",
        "file,pipe",
        "-threads",
        "2",
        "-max_pixels",
        "100000000",
        "-y",
    ]
    .into_iter()
    .map(str::to_owned)
    .collect()
}

fn image_webp_args(settings: &MediaSettings, input: &Path, output: &Path) -> Vec<String> {
    safety_args()
        .into_iter()
        .chain([
            "-i".to_owned(),
            input.display().to_string(),
            "-vf".to_owned(),
            "scale='min(1600,iw)':-2".to_owned(),
            "-threads".to_owned(),
            "2".to_owned(),
            "-map_metadata".to_owned(),
            "-1".to_owned(),
            "-c:v".to_owned(),
            "libwebp".to_owned(),
            "-quality".to_owned(),
            settings.webp_quality.to_string(),
            "-fs".to_owned(),
            settings.max_image_size.to_string(),
            output.display().to_string(),
        ])
        .collect()
}

fn image_webp_thumbnail_args(
    settings: &MediaSettings,
    input: &Path,
    output: &Path,
    size: u16,
) -> Vec<String> {
    let size = size.to_string();
    safety_args()
        .into_iter()
        .chain([
            "-i".to_owned(),
            input.display().to_string(),
            "-vf".to_owned(),
            format!("scale={size}:{size}:force_original_aspect_ratio=increase,crop={size}:{size}"),
            "-threads".to_owned(),
            "2".to_owned(),
            "-map_metadata".to_owned(),
            "-1".to_owned(),
            "-c:v".to_owned(),
            "libwebp".to_owned(),
            "-quality".to_owned(),
            settings.webp_quality.to_string(),
            "-fs".to_owned(),
            settings.max_image_size.to_string(),
            output.display().to_string(),
        ])
        .collect()
}

fn video_webm_args(settings: &MediaSettings, input: &Path, output: &Path) -> Vec<String> {
    safety_args()
        .into_iter()
        .chain([
            "-i".to_owned(),
            input.display().to_string(),
            "-threads".to_owned(),
            "2".to_owned(),
            "-map_metadata".to_owned(),
            "-1".to_owned(),
            "-c:v".to_owned(),
            "libvpx-vp9".to_owned(),
            "-pix_fmt".to_owned(),
            "yuv420p".to_owned(),
            "-colorspace".to_owned(),
            "bt709".to_owned(),
            "-color_primaries".to_owned(),
            "bt709".to_owned(),
            "-color_trc".to_owned(),
            "bt709".to_owned(),
            "-crf".to_owned(),
            settings.vp9_crf.to_string(),
            "-deadline".to_owned(),
            settings.vp9_deadline.clone(),
            "-b:v".to_owned(),
            "0".to_owned(),
            "-c:a".to_owned(),
            "libopus".to_owned(),
            "-fs".to_owned(),
            settings.max_video_size.to_string(),
            output.display().to_string(),
        ])
        .collect()
}

pub fn stderr_summary(stderr: &[u8]) -> String {
    String::from_utf8_lossy(stderr)
        .lines()
        .rev()
        .take(8)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect::<Vec<_>>()
        .join("\n")
}
