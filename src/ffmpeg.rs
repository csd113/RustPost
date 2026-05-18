use std::path::Path;
use std::process::{Output, Stdio};
use std::time::Duration;

use tokio::process::Command;
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
    let output = Command::new(&settings.ffmpeg_path)
        .arg("-version")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await;
    let Ok(output) = output else {
        return FfmpegStatus {
            available: false,
            version: String::new(),
            supports_webp: false,
            supports_vp9: false,
            error: Some("ffmpeg command not found".to_owned()),
        };
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
    let encoders = Command::new(&settings.ffmpeg_path)
        .arg("-hide_banner")
        .arg("-encoders")
        .output()
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
    let result = run_ffmpeg(&settings.ffmpeg_path, &args, Duration::from_secs(120)).await?;
    if result.status.success() {
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
    let result = run_ffmpeg(&settings.ffmpeg_path, &args, Duration::from_secs(300)).await?;
    if result.status.success() {
        Ok(stderr_summary(&result.stderr))
    } else {
        anyhow::bail!(stderr_summary(&result.stderr));
    }
}

async fn run_ffmpeg(path: &str, args: &[String], duration: Duration) -> anyhow::Result<Output> {
    let mut command = Command::new(path);
    command
        .kill_on_drop(true)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for arg in args {
        command.arg(arg);
    }
    let child = command.spawn()?;
    timeout(duration, child.wait_with_output())
        .await
        .map_err(|_elapsed| anyhow::anyhow!("ffmpeg timed out"))?
        .map_err(Into::into)
}

fn image_webp_args(settings: &MediaSettings, input: &Path, output: &Path) -> Vec<String> {
    vec![
        "-y".to_owned(),
        "-i".to_owned(),
        input.display().to_string(),
        "-vf".to_owned(),
        "scale='min(1600,iw)':-2".to_owned(),
        "-c:v".to_owned(),
        "libwebp".to_owned(),
        "-quality".to_owned(),
        settings.webp_quality.to_string(),
        output.display().to_string(),
    ]
}

fn video_webm_args(settings: &MediaSettings, input: &Path, output: &Path) -> Vec<String> {
    vec![
        "-y".to_owned(),
        "-i".to_owned(),
        input.display().to_string(),
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
        output.display().to_string(),
    ]
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

#[cfg(test)]
mod tests {
    use std::path::Path;

    use crate::config::MediaSettings;

    #[test]
    fn vp9_args_document_safe_color_handling() {
        let args = super::video_webm_args(
            &MediaSettings {
                ffmpeg_path: "ffmpeg".to_owned(),
                convert_images_to_webp: true,
                convert_videos_to_webm: true,
                keep_original_uploads: false,
                max_image_size: 1,
                max_video_size: 1,
                generate_video_thumbnails: false,
                allowed_image_mime_types: Vec::new(),
                allowed_video_mime_types: Vec::new(),
                webp_quality: 82,
                vp9_crf: 32,
                vp9_deadline: "good".to_owned(),
            },
            Path::new("in.mp4"),
            Path::new("out.webm"),
        );
        assert!(args.iter().any(|arg| arg == "yuv420p"));
        assert!(args.iter().filter(|arg| arg.as_str() == "bt709").count() >= 3);
    }

    #[tokio::test]
    async fn missing_ffmpeg_reports_error() {
        let result = super::run_ffmpeg(
            "definitely-not-rustpost-ffmpeg",
            &["-version".to_owned()],
            std::time::Duration::from_millis(100),
        )
        .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn ffmpeg_timeout_is_reported() {
        let result = super::run_ffmpeg(
            "sh",
            &["-c".to_owned(), "sleep 2".to_owned()],
            std::time::Duration::from_millis(50),
        )
        .await;
        assert!(result.is_err());
        assert!(
            result
                .expect_err("timeout")
                .to_string()
                .contains("timed out")
        );
    }
}
