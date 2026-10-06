//! Optional FFmpeg helpers for video thumbnails. Audio playback does not depend
//! on these executables. All subprocess inputs are private local staging files.
use std::{
    io::Read,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::Arc,
    time::{Duration, Instant},
};

pub struct VideoPreview {
    pub image: Vec<u8>,
    pub metadata: String,
}

fn executable(name: &str) -> Result<PathBuf, String> {
    let directories = std::env::var_os("PATH")
        .map(|path| std::env::split_paths(&path).collect::<Vec<_>>())
        .unwrap_or_default();
    directories
        .into_iter()
        .chain([
            PathBuf::from("/opt/homebrew/bin"),
            PathBuf::from("/usr/local/bin"),
        ])
        .map(|path| path.join(name))
        .find(|path| path.is_file())
        .ok_or_else(|| format!("视频预览需要 FFmpeg（缺少 {name}）；请安装后重试"))
}

fn run(directory: &Path, name: &str, arguments: &[&str]) -> Result<Vec<u8>, String> {
    let output = directory.join(format!("{name}.stdout"));
    let errors = directory.join(format!("{name}.stderr"));
    let mut child = Command::new(executable(name)?)
        .args(arguments)
        .stdin(Stdio::null())
        .stdout(std::fs::File::create(&output).map_err(|e| e.to_string())?)
        .stderr(std::fs::File::create(&errors).map_err(|e| e.to_string())?)
        .spawn()
        .map_err(|e| e.to_string())?;
    let start = Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait().map_err(|e| e.to_string())? {
            break status;
        }
        if start.elapsed() > Duration::from_secs(20)
            || std::fs::metadata(&output).is_ok_and(|m| m.len() > 16 * 1024 * 1024)
            || std::fs::metadata(&errors).is_ok_and(|m| m.len() > 1024 * 1024)
        {
            let _ = child.kill();
            let _ = child.wait();
            return Err("视频解码超过预览时间或大小上限".into());
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    if !status.success() {
        let mut message = String::new();
        std::fs::File::open(errors)
            .map_err(|e| e.to_string())?
            .take(4096)
            .read_to_string(&mut message)
            .map_err(|e| e.to_string())?;
        return Err(format!("无法读取视频：{}", message.trim()));
    }
    std::fs::read(output).map_err(|e| e.to_string())
}

pub fn video(name: &str, bytes: &[u8]) -> Result<VideoPreview, String> {
    let directory = tempfile::tempdir().map_err(|e| e.to_string())?;
    let extension = name.rsplit('.').next().unwrap_or("mp4").to_lowercase();
    let path = directory.path().join(format!("video.{extension}"));
    std::fs::write(&path, bytes).map_err(|e| e.to_string())?;
    let path = path.to_string_lossy();
    let metadata = run(
        directory.path(),
        "ffprobe",
        &[
            "-v",
            "error",
            "-max_alloc",
            "67108864",
            "-protocol_whitelist",
            "file,pipe",
            "-show_entries",
            "format=duration:stream=codec_type,codec_name,width,height,sample_rate,channels",
            "-of",
            "json",
            &path,
        ],
    )?;
    let metadata: serde_json::Value =
        serde_json::from_slice(&metadata).map_err(|e| e.to_string())?;
    let image = run(
        directory.path(),
        "ffmpeg",
        &[
            "-v",
            "error",
            "-nostdin",
            "-threads",
            "2",
            "-max_alloc",
            "67108864",
            "-protocol_whitelist",
            "file,pipe",
            "-i",
            &path,
            "-map",
            "0:v:0",
            "-frames:v",
            "1",
            "-vf",
            "scale=960:960:force_original_aspect_ratio=decrease",
            "-threads",
            "2",
            "-f",
            "image2pipe",
            "-vcodec",
            "png",
            "pipe:1",
        ],
    )?;
    if image.is_empty() {
        return Err("视频没有可预览的画面".into());
    }
    let duration = metadata["format"]["duration"]
        .as_str()
        .and_then(|v| v.parse::<f64>().ok())
        .filter(|v| v.is_finite())
        .unwrap_or(0.);
    let streams = metadata["streams"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or(&[]);
    let info = streams
        .iter()
        .map(|stream| {
            if stream["codec_type"] == "video" {
                format!(
                    "{} × {} · {}",
                    stream["width"],
                    stream["height"],
                    stream["codec_name"].as_str().unwrap_or("unknown")
                )
            } else {
                format!(
                    "{} · {} Hz · {} 声道",
                    stream["codec_name"].as_str().unwrap_or("unknown"),
                    stream["sample_rate"].as_str().unwrap_or("—"),
                    stream["channels"]
                )
            }
        })
        .collect::<Vec<_>>()
        .join("\n");
    Ok(VideoPreview {
        image,
        metadata: format!(
            "时长 {}\n{info}",
            timestamp(Duration::from_secs_f64(duration.clamp(0., 864000.)))
        ),
    })
}

pub fn timestamp(duration: Duration) -> String {
    let seconds = duration.as_secs();
    format!(
        "{:02}:{:02}:{:02}",
        seconds / 3600,
        seconds / 60 % 60,
        seconds % 60
    )
}

/// Reusable staged audio source; decoding and device creation happen off the UI
/// thread. Arc keeps the bytes alive without a second whole-file copy.
pub type AudioBytes = Arc<Vec<u8>>;
