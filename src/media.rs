//! ffmpeg 子进程封装：音频抽取（16k 单声道 PCM wav）与媒体探测。

use anyhow::{Context, Result};
use std::path::Path;
use std::time::Duration;
use tokio::process::Command;

/// ffprobe 读元数据的上限：损坏或卡住的输入不能挂住流程。
const PROBE_TIMEOUT: Duration = Duration::from_secs(60);

/// 有界运行 ffprobe；超时或启动失败返回 None（探测失败均非致命）。
async fn probe_output(cmd: &mut Command) -> Option<std::process::Output> {
    let output = cmd
        .kill_on_drop(true)
        .stdin(std::process::Stdio::null())
        .output();
    tokio::time::timeout(PROBE_TIMEOUT, output).await.ok()?.ok()
}

/// 跑子进程并检查退出码；失败时返回带 stderr 尾部摘要的错误（供各模块复用）。
pub(crate) async fn run_cmd(cmd: &mut Command, what: &str) -> Result<std::process::Output> {
    let out = cmd
        .kill_on_drop(true)
        .output()
        .await
        .with_context(|| crate::tr!("无法启动 {} / Could not start {}", what, what))?;
    if !out.status.success() {
        return Err(crate::error::cmd_error(
            what,
            out.status.code(),
            &String::from_utf8_lossy(&out.stderr),
        ));
    }
    Ok(out)
}

/// 抽取 16kHz 单声道 s16 wav。已存在则跳过。
pub async fn extract_audio(media: &Path, dest: &Path) -> Result<()> {
    if dest.is_file() {
        tracing::info!(path = %dest.display(), "audio exists, skip extract");
        return Ok(());
    }
    if let Some(p) = dest.parent() {
        tokio::fs::create_dir_all(p).await?;
    }
    let temporary = tempfile::Builder::new()
        .suffix(".wav")
        .tempfile_in(dest.parent().unwrap_or(Path::new(".")))?;
    run_cmd(
        Command::new(crate::tools::program("ffmpeg"))
            .args(["-hide_banner", "-loglevel", "error", "-y"])
            .arg("-i")
            .arg(media)
            .args(["-vn", "-ac", "1", "-ar", "16000", "-c:a", "pcm_s16le"])
            .arg(temporary.path()),
        "ffmpeg",
    )
    .await?;
    temporary.persist(dest)?;
    Ok(())
}

#[derive(Debug, Clone, Copy)]
pub struct VideoInfo {
    pub width: u32,
    pub height: u32,
    pub duration: f64,
    /// 标称帧率，仅用于进度估计；精确扫描使用解码帧 PTS。探测不到时为 30。
    pub fps: f64,
}

/// ffprobe `-of json` 输出中我们关心的字段子集。
#[derive(serde::Deserialize)]
struct ProbeJson {
    #[serde(default)]
    streams: Vec<ProbeStream>,
    format: Option<ProbeFormat>,
}

#[derive(serde::Deserialize)]
struct ProbeStream {
    width: Option<u32>,
    height: Option<u32>,
    /// 形如 "30000/1001" 的帧率分式。
    r_frame_rate: Option<String>,
    avg_frame_rate: Option<String>,
}

#[derive(serde::Deserialize)]
struct ProbeFormat {
    /// ffprobe 的 duration 是字符串（如 "95.040000"）
    duration: Option<String>,
}

/// ffprobe 的帧率是分式（"30000/1001"）；0/0 表示未知。
fn parse_rate(raw: Option<&str>) -> Option<f64> {
    let (num, den) = raw?.split_once('/')?;
    let num: f64 = num.trim().parse().ok()?;
    let den: f64 = den.trim().parse().ok()?;
    (num.is_finite() && den.is_finite() && num > 0.0 && den > 0.0).then(|| num / den)
}

/// 视频宽高、时长与标称帧率。用 `-of json` 结构化输出，取代按逗号/空白切分的文本解析。
pub async fn probe_video(media: &Path) -> Option<VideoInfo> {
    let out = probe_output(
        Command::new(crate::tools::program("ffprobe"))
            .args([
                "-v",
                "error",
                "-select_streams",
                "v:0",
                "-show_entries",
                "stream=width,height,r_frame_rate,avg_frame_rate:format=duration",
                "-of",
                "json",
            ])
            .arg(media),
    )
    .await?;
    if !out.status.success() {
        return None;
    }
    let probe: ProbeJson = serde_json::from_slice(&out.stdout).ok()?;
    let stream = probe.streams.first()?;
    let duration = probe
        .format
        .and_then(|f| f.duration)
        .and_then(|d| d.parse::<f64>().ok())?;
    let fps = parse_rate(stream.r_frame_rate.as_deref())
        .or_else(|| parse_rate(stream.avg_frame_rate.as_deref()))
        .unwrap_or(30.0)
        .clamp(1.0, 240.0);
    Some(VideoInfo {
        width: stream.width?,
        height: stream.height?,
        duration,
        fps,
    })
}

/// `probe_duration` 两个入口共享的 ffprobe 参数（只输出时长数字）。
fn duration_args() -> [&'static str; 6] {
    [
        "-v",
        "error",
        "-show_entries",
        "format=duration",
        "-of",
        "default=noprint_wrappers=1:nokey=1",
    ]
}

/// 从 ffprobe 输出解析时长（秒）；进程失败或输出非法均为 None（非致命）。
fn parse_duration_text(text: &str) -> Option<f64> {
    text.trim().parse::<f64>().ok()
}

fn parse_duration(out: &std::process::Output) -> Option<f64> {
    out.status
        .success()
        .then(|| parse_duration_text(&String::from_utf8_lossy(&out.stdout)))
        .flatten()
}

/// 用 ffprobe 拿时长（秒）。失败时返回 None（非致命）。
pub async fn probe_duration(media: &Path) -> Option<f64> {
    let out = probe_output(
        Command::new(crate::tools::program("ffprobe"))
            .args(duration_args())
            .arg(media),
    )
    .await?;
    parse_duration(&out)
}

/// [`probe_duration`] 的同步版（供阻塞线程使用）。
pub fn probe_duration_blocking(media: &Path) -> Option<f64> {
    let mut cmd = std::process::Command::new(crate::tools::program("ffprobe"));
    cmd.args(duration_args()).arg(media);
    let out = crate::runtime::run_bounded("ffprobe", &mut cmd, PROBE_TIMEOUT).ok()?;
    out.status
        .success()
        .then(|| parse_duration_text(&out.stdout))
        .flatten()
}

/// Paired with the raw frame emitted by the same ffmpeg filter graph. Reading
/// integer PTS avoids showinfo's rounded pts_time (notably in long lectures).
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct FrameStamp {
    pub generation: u64,
    pub index: u64,
    pub seconds: f64,
}

#[derive(Default)]
pub(crate) struct FrameClock {
    time_base: Option<(f64, f64)>,
    generation: u64,
}

impl FrameClock {
    pub fn parse(&mut self, line: &str) -> Result<Option<FrameStamp>> {
        if !line.contains("Parsed_showinfo_") {
            return Ok(None);
        }
        if let Some((_, value)) = line.split_once("config in time_base:") {
            let value = value.split(',').next().unwrap_or("").trim();
            let valid = value.split_once('/').and_then(|(n, d)| {
                let (n, d) = (n.parse::<f64>().ok()?, d.parse::<f64>().ok()?);
                (n.is_finite() && d.is_finite() && n > 0.0 && d > 0.0).then_some((n, d))
            });
            anyhow::ensure!(
                valid.is_some(),
                crate::tr!("视频帧时间戳缺失或无效，无法可靠定位截图")
            );
            // A resolution/pixel-format change reinitializes the filter graph:
            // showinfo's local n resets, but the source PTS must stay continuous.
            if self.time_base.is_some() {
                self.generation += 1;
            }
            self.time_base = valid;
            return Ok(None);
        }
        let Some((_, count)) = line.split_once(" n:") else {
            return Ok(None);
        };
        let stamp = (|| {
            let index = count.split_whitespace().next()?.parse::<u64>().ok()?;
            let pts = line
                .split_once(" pts:")?
                .1
                .split_whitespace()
                .next()?
                .parse::<i64>()
                .ok()?;
            let (n, d) = self.time_base?;
            let seconds = pts as f64 * n / d;
            (seconds.is_finite() && seconds >= 0.0).then_some(FrameStamp {
                generation: self.generation,
                index,
                seconds,
            })
        })();
        stamp
            .map(Some)
            .context(crate::tr!("视频帧时间戳缺失或无效，无法可靠定位截图"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decoded_clock_tracks_filter_generations_and_new_time_bases() {
        let mut clock = FrameClock::default();
        let mut stamps = Vec::new();
        for (base, counts) in [("1/10", [0, 5]), ("1/100", [100, 150])] {
            clock
                .parse(&format!(
                    "[Parsed_showinfo_2 @ ptr] config in time_base: {base}, frame_rate: 2/1"
                ))
                .unwrap();
            for (n, pts) in counts.iter().enumerate() {
                stamps.push(
                    clock
                        .parse(&format!(
                            "[Parsed_showinfo_2 @ ptr] n: {n} pts:{pts} pts_time:unused"
                        ))
                        .unwrap()
                        .unwrap(),
                );
            }
        }
        assert_eq!(
            stamps
                .iter()
                .map(|s| (s.generation, s.index))
                .collect::<Vec<_>>(),
            [(0, 0), (0, 1), (1, 0), (1, 1)]
        );
        assert_eq!(
            stamps.iter().map(|s| s.seconds).collect::<Vec<_>>(),
            [0.0, 0.5, 1.0, 1.5]
        );
    }

    #[test]
    fn decoded_clock_keeps_long_lecture_precision_and_rejects_missing_pts() {
        let mut clock = FrameClock::default();
        assert_eq!(
            clock
                .parse("[Parsed_showinfo_2 @ ptr] config in time_base: 1/90000, frame_rate: 30/1")
                .unwrap(),
            None
        );
        let stamp = clock
            .parse("[Parsed_showinfo_2 @ ptr] n: 99 pts:648003003 pts_time:7200.03 fmt:rgb24")
            .unwrap()
            .unwrap();
        assert_eq!(stamp.index, 99);
        assert!((stamp.seconds - 7200.033366666667).abs() < 1e-10);
        assert!(
            clock
                .parse("[Parsed_showinfo_2 @ ptr] n: 100 pts: NOPTS pts_time:NOPTS")
                .is_err()
        );
        assert!(
            FrameClock::default()
                .parse("[Parsed_showinfo_2 @ ptr] n: 0 pts:0 pts_time:0")
                .is_err()
        );
        assert!(
            clock
                .parse("[Parsed_showinfo_2 @ ptr] config in time_base: 0/0, frame_rate: 30/1")
                .is_err()
        );
        assert_eq!(clock.parse("encoder diagnostic").unwrap(), None);
    }
}
