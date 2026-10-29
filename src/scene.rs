//! 幻灯片抽帧：对齐 yt-slide-mark
//! `stable`/`first` 模式按 `sample_interval` 采样，与上一张保留帧做 SSIM；
//! 低于 `similarity` 则保存，并跳过 `cooldown` 秒。
//! `precise` 模式逐帧分析：稳定态、瞬时闪现态和连续变化的过程帧都保留。

use crate::config::{PipelineConfig, Roi};
use crate::media;
use crate::progress::Stage;
use crate::timeline::FrameEvent;
use anyhow::{Context, Result};
use image::GrayImage;
use image_compare::Algorithm;
use std::borrow::Cow;
use std::path::Path;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
use tokio::process::Command;

/// SSIM 采样宽度：足够区分版面变化，同时控制 rawvideo 带宽与 SSIM 计算开销
const SAMPLE_WIDTH: u32 = 640;
/// 全分辨率抽帧并发度：每帧一个 ffmpeg 进程，4 路即可利用并行度又不至于进程风暴
const EXTRACT_CONCURRENCY: usize = 4;

fn scaled_wh(ow: u32, oh: u32, target_w: u32) -> (u32, u32) {
    let h = ((oh as u64 * target_w as u64) / ow.max(1) as u64) as u32;
    (target_w, (h & !1).max(2))
}

/// 无 ROI 时借用原图（零拷贝）；有 ROI 时返回裁剪后的新图。
fn crop_roi(img: &GrayImage, roi: Option<Roi>) -> Cow<'_, GrayImage> {
    let Some(r) = roi else {
        return Cow::Borrowed(img);
    };
    let (w, h) = img.dimensions();
    let (x1, y1, x2, y2) = r.pixels(w, h);
    Cow::Owned(
        image::imageops::crop_imm(img, x1, y1, (x2 - x1).max(1), (y2 - y1).max(1)).to_image(),
    )
}

fn ssim(a: &GrayImage, b: &GrayImage) -> f64 {
    image_compare::gray_similarity_structure(&Algorithm::MSSIMSimple, a, b)
        .map(|s| s.score)
        .unwrap_or(0.0)
}

/// 单点精确抽帧（全分辨率 JPEG）。
async fn extract_frame(
    media: &Path,
    t: f64,
    dest: &Path,
    control: &crate::dispatch::TaskControl,
) -> Result<()> {
    control.check()?;
    // ffmpeg seek timestamps have microsecond precision. Round down, never up:
    // rounding 2/30 to .067 seeks past that one-frame page and captures the next.
    let ss = format!("{:.6}", (t * 1_000_000.0).floor() / 1_000_000.0);
    // -q:v 2：JPEG 高质量档（2~31，越小越好），讲义文字可读性优先
    let mut cmd = Command::new(crate::tools::program("ffmpeg"));
    cmd.kill_on_drop(true)
        .args(["-hide_banner", "-loglevel", "error", "-y", "-ss", &ss])
        .arg("-i")
        .arg(media)
        .args(["-map", "0:v:0", "-frames:v", "1", "-q:v", "2"])
        .arg(dest);
    let output = tokio::select! {
        error = control.watch() => return Err(error),
        output = cmd.output() => output.context(crate::tr!("无法启动 {} / Could not start {}", "ffmpeg", "ffmpeg"))?,
    };
    control.check()?;
    anyhow::ensure!(
        output.status.success(),
        crate::tr!(
            "外部工具 {} 执行失败（退出码 {}）：{}",
            "ffmpeg",
            output.status,
            crate::error::tail_lines(&String::from_utf8_lossy(&output.stderr), 5)
        )
    );
    anyhow::ensure!(
        dest.is_file(),
        "{}: {}",
        crate::tr!("未提取到截图"),
        dest.display()
    );
    Ok(())
}

/// 第 n 张（1 起）截图的文件名。
fn slide_name(n: usize) -> String {
    format!("slide_{n:04}.jpg")
}

// —— precise 模式：逐帧分析，尽量不漏任何画面变化 ——

/// 精确模式的分析分辨率（宽）。小字/代码行的局部改动在 480 宽仍可分辨，
/// 同时把逐帧 rawvideo 带宽控制在可接受范围。
const SCAN_WIDTH: u32 = 480;
/// 逐像素判为「变了」的颜色差（0–255），低于此值视为编码噪声。
const SCAN_PIXEL_DELTA: u8 = 12;
/// 分块边长（像素）。局部变化按块统计，避免整页平均稀释小改动。
const SCAN_TILE: u32 = 20;
/// 某块内变化像素占比达到此值，认为该块发生了内容变化。
const SCAN_TILE_CHANGED: f64 = 0.30;
/// 全画面变化像素占比达到此值，也认为发生了内容变化（整页切换、大块新增）。
const SCAN_AREA_CHANGED: f64 = 0.002;
/// 判定「画面正在运动」的帧间阈值（低于稳定判定，留出滞回）。
const SCAN_MOTION_TILE: f64 = 0.10;
const SCAN_MOTION_AREA: f64 = 0.0006;
/// 帧间稳定多久算进入新状态。
const SCAN_SETTLE_SECS: f64 = 0.12;
/// 连续变化超过多久强制保留一帧（老师一直写、动画持续时也不会丢过程帧）。
const SCAN_MAX_HOLD_SECS: f64 = 2.0;
/// A large abrupt cut is distinct even if the outgoing page lasted one frame.
/// Smaller continuous motion remains throttled; brief static runs are flushed
/// separately when the next change arrives.
const SCAN_CUT_AREA: f64 = 0.30;

/// 降采样后的 RGB 签名；ROI 在缩放后裁剪。
#[derive(Clone)]
struct ScanSig {
    w: u32,
    h: u32,
    rgb: Vec<u8>,
}

#[derive(Clone, Copy, Debug, Default)]
struct ScanDiff {
    /// 变化最剧烈的那一块内的变化像素占比。
    local: f64,
    /// 全画面变化像素占比。
    area: f64,
}

impl ScanDiff {
    fn changed(self) -> bool {
        self.local >= SCAN_TILE_CHANGED || self.area >= SCAN_AREA_CHANGED
    }
    fn moving(self) -> bool {
        self.local >= SCAN_MOTION_TILE || self.area >= SCAN_MOTION_AREA
    }
    /// 峰值挑选：以变化面积为主，局部变化加权辅助。
    fn score(self) -> f64 {
        self.area + self.local * 0.02
    }
}

fn scan_diff(a: &ScanSig, b: &ScanSig) -> ScanDiff {
    if a.w != b.w || a.h != b.h {
        return ScanDiff {
            local: 1.0,
            area: 1.0,
        };
    }
    let w = a.w as usize;
    let h = a.h as usize;
    let tiles_x = a.w.div_ceil(SCAN_TILE);
    let tiles_y = a.h.div_ceil(SCAN_TILE);
    let mut counts = vec![0u32; (tiles_x * tiles_y) as usize];
    let mut changed = 0u64;
    for y in 0..h {
        for x in 0..w {
            let i = (y * w + x) * 3;
            let d = a.rgb[i]
                .abs_diff(b.rgb[i])
                .max(a.rgb[i + 1].abs_diff(b.rgb[i + 1]))
                .max(a.rgb[i + 2].abs_diff(b.rgb[i + 2]));
            if d > SCAN_PIXEL_DELTA {
                changed += 1;
                let tx = (x as u32 / SCAN_TILE) as usize;
                let ty = (y as u32 / SCAN_TILE) as usize;
                counts[ty * tiles_x as usize + tx] += 1;
            }
        }
    }
    // 边缘块按实际尺寸算占比。
    let mut local = 0.0f64;
    for ty in 0..tiles_y {
        for tx in 0..tiles_x {
            let cw = SCAN_TILE.min(a.w - tx * SCAN_TILE) as f64;
            let ch = SCAN_TILE.min(a.h - ty * SCAN_TILE) as f64;
            let ratio = f64::from(counts[(ty * tiles_x + tx) as usize]) / (cw * ch);
            local = local.max(ratio);
        }
    }
    ScanDiff {
        local,
        area: changed as f64 / (w * h) as f64,
    }
}

/// 缩放后的整帧（RGB）裁成 ROI 签名。
fn make_scan_sig(rgb: &[u8], w: u32, h: u32, roi: Option<Roi>) -> ScanSig {
    let Some(r) = roi else {
        return ScanSig {
            w,
            h,
            rgb: rgb.to_vec(),
        };
    };
    let (x1, y1, x2, y2) = r.pixels(w, h);
    let cw = x2 - x1;
    let ch = y2 - y1;
    let mut out = Vec::with_capacity((cw * ch * 3) as usize);
    for y in y1..y2 {
        let start = ((y * w + x1) * 3) as usize;
        out.extend_from_slice(&rgb[start..start + (cw * 3) as usize]);
    }
    ScanSig {
        w: cw,
        h: ch,
        rgb: out,
    }
}

/// 逐帧精确检测状态机。每个解码帧都参与判断：
/// - 与「上一次截图的画面」偏离 → 进入偏离期，记录偏离峰值帧；
/// - 画面稳定达到 `SCAN_SETTLE_SECS` → 新稳定态，用清晰帧截图；
/// - 偏离后又回到原状态 → 瞬时闪现，用峰值帧截图；
/// - 连续变化超过 `SCAN_MAX_HOLD_SECS` → 强制保留过程帧。
struct PreciseDetector {
    reference: Option<ScanSig>,
    previous: Option<ScanSig>,
    previous_t: f64,
    state_start: f64,
    peak: Option<f64>,
    peak_score: f64,
    diverging: bool,
    diverge_start: f64,
    stable_since: Option<f64>,
    last_emit: f64,
    times: Vec<(f64, f64)>,
}

impl PreciseDetector {
    fn new() -> Self {
        Self {
            reference: None,
            previous: None,
            previous_t: 0.0,
            state_start: 0.0,
            peak: None,
            peak_score: f64::NEG_INFINITY,
            diverging: false,
            diverge_start: 0.0,
            stable_since: None,
            last_emit: f64::NEG_INFINITY,
            times: Vec::new(),
        }
    }

    fn push(&mut self, t: f64, sig: ScanSig) {
        if self.reference.is_none() {
            // 首帧总是第一张截图。
            self.times.push((t, t));
            self.last_emit = t;
            self.reference = Some(sig.clone());
            self.previous = Some(sig);
            self.previous_t = t;
            return;
        }
        let mut d_ref = scan_diff(self.reference.as_ref().unwrap(), &sig);
        let d_prev = self
            .previous
            .as_ref()
            .map(|p| scan_diff(p, &sig))
            .unwrap_or_default();
        // A short page can end before the settling timer fires. Keep its
        // actual state before replacing the candidate with the next page.
        // With VFR, one decoded frame may itself have been displayed for seconds.
        if self.diverging
            && d_ref.changed()
            && d_prev.changed()
            && (self.stable_since.is_some()
                || t - self.previous_t >= SCAN_SETTLE_SECS
                || d_prev.area >= SCAN_CUT_AREA)
        {
            let capture = self.stable_since.unwrap_or(self.previous_t);
            if capture > self.last_emit {
                self.emit(self.state_start, capture);
                self.reference = self.previous.clone();
                self.reset_divergence();
                d_ref = scan_diff(self.reference.as_ref().unwrap(), &sig);
            }
        }
        if d_ref.changed() {
            if !self.diverging {
                self.diverging = true;
                self.diverge_start = t;
                self.state_start = t;
                self.peak = None;
                self.peak_score = f64::NEG_INFINITY;
                self.stable_since = None;
            }
            let score = d_ref.score();
            if score > self.peak_score {
                self.peak_score = score;
                self.peak = Some(t);
            }
            if d_prev.moving() {
                self.state_start = t;
                self.stable_since = None;
            } else {
                let since = *self.stable_since.get_or_insert(self.previous_t);
                if t - since >= SCAN_SETTLE_SECS && t > self.last_emit {
                    self.emit(self.state_start, t);
                    self.reference = Some(sig.clone());
                    self.reset_divergence();
                }
            }
            if self.diverging && t - self.diverge_start >= SCAN_MAX_HOLD_SECS && t > self.last_emit
            {
                // Process snapshots describe the state at capture, not what was
                // on screen two seconds earlier when motion first began.
                self.emit(t, t);
                self.reference = Some(sig.clone());
                self.reset_divergence();
            }
        } else if self.diverging {
            // 偏离后又回到原状态：闪现和恢复都是时间线事件。
            // 只留峰值会让后续字幕一直归到已经消失的闪现画面。
            if let Some(peak_t) = self.peak.take()
                && peak_t > self.last_emit
            {
                self.emit(self.diverge_start, peak_t);
                self.emit(t, t);
            }
            self.reset_divergence();
        }
        self.previous = Some(sig);
        self.previous_t = t;
    }

    /// Keep the final visible state, even when an earlier scrolling frame had
    /// the largest difference. Its onset is the last material motion boundary.
    fn finish(&mut self) {
        if self.diverging && self.previous_t > self.last_emit {
            self.emit(self.state_start, self.previous_t);
            self.reset_divergence();
        }
    }

    fn emit(&mut self, onset: f64, capture: f64) {
        self.times.push((onset, capture));
        self.last_emit = capture;
    }

    fn reset_divergence(&mut self) {
        self.diverging = false;
        self.peak = None;
        self.stable_since = None;
    }
}

fn valid_next_stamp(previous: Option<media::FrameStamp>, stamp: media::FrameStamp) -> bool {
    match previous {
        None => stamp.index == 0,
        Some(prev) => {
            let sequential = if stamp.generation == prev.generation {
                stamp.index == prev.index + 1
            } else {
                stamp.generation > prev.generation && stamp.index == 0
            };
            sequential && stamp.seconds > prev.seconds
        }
    }
}

/// 逐帧扫描：每个解码帧都做分块差异判断，输出要截图的 (onset, capture) 时间点。
async fn sample_precise(
    cfg: &PipelineConfig,
    media: &Path,
    control: &crate::dispatch::TaskControl,
    info: &media::VideoInfo,
) -> Result<Vec<(f64, f64)>> {
    let (tw, th) = scaled_wh(info.width, info.height, SCAN_WIDTH);
    let fps = info.fps;
    // No fps filter: it resamples VFR and can discard short pages. PTS and pixels
    // come from the same graph; passthrough also prevents output-side duplication.
    let vf = format!("scale={tw}:{th},format=rgb24,showinfo=checksum=0");
    tracing::info!(
        w = info.width,
        h = info.height,
        duration = format_args!("{:.0}s", info.duration),
        fps,
        "precise scan"
    );

    let mut child = Command::new(crate::tools::program("ffmpeg"))
        .kill_on_drop(true)
        .args(["-hide_banner", "-nostats", "-loglevel", "info", "-an"])
        .arg("-i")
        .arg(media)
        .args([
            "-map",
            "0:v:0",
            "-vf",
            &vf,
            "-fps_mode",
            "passthrough",
            "-f",
            "rawvideo",
            "pipe:1",
        ])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .context(crate::tr!(
            "无法读取视频画面 / Could not start ffmpeg frame sampling"
        ))?;
    let mut stdout = child.stdout.take().context("ffmpeg stdout")?;
    let stderr = child.stderr.take().context("ffmpeg stderr")?;
    let (stamps_tx, mut stamps_rx) = tokio::sync::mpsc::channel(16);
    let stderr_task = tokio::spawn(async move {
        let mut lines = BufReader::new(stderr).lines();
        let mut clock = media::FrameClock::default();
        let mut tail = std::collections::VecDeque::new();
        while let Some(line) = lines.next_line().await? {
            if let Some(stamp) = clock.parse(&line)? {
                if stamps_tx.send(stamp).await.is_err() {
                    break;
                }
            } else if !line.contains("Parsed_showinfo_") {
                if tail.len() == 8 {
                    tail.pop_front();
                }
                tail.push_back(line);
            }
        }
        Ok::<_, anyhow::Error>(tail.into_iter().collect::<Vec<_>>().join("\n"))
    });

    let frame_len = (tw as usize) * (th as usize) * 3;
    let mut buf = vec![0u8; frame_len];
    let estimate = (info.duration * fps).round().max(1.0) as u64;
    let pb = crate::progress::Bar::new(Stage::ScenesScan, estimate)
        .with_template("{spinner:.green} scan {pos}/{len} frames {msg}");
    pb.set_position(0);

    let mut detector = PreciseDetector::new();
    let mut i: u64 = 0;
    let mut previous_stamp: Option<media::FrameStamp> = None;
    loop {
        control.check()?;
        let read = tokio::select! {
            error = control.watch() => return Err(error),
            read = stdout.read_exact(&mut buf) => read,
        };
        match read {
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => break,
            Err(e) => return Err(e.into()),
        }
        let stamp = tokio::select! {
            error = control.watch() => return Err(error),
            stamp = stamps_rx.recv() => stamp.context(crate::tr!("视频帧时间戳缺失或无效，无法可靠定位截图"))?,
        };
        anyhow::ensure!(
            valid_next_stamp(previous_stamp, stamp),
            crate::tr!("视频帧时间戳缺失或无效，无法可靠定位截图")
        );
        let t = stamp.seconds;
        previous_stamp = Some(stamp);
        i += 1;
        pb.inc(1);
        let sig = make_scan_sig(&buf, tw, th, cfg.roi);
        detector.push(t, sig);
        if i.is_multiple_of(60) {
            pb.set_message(crate::tr!(
                "已找到 {} 张候选截图 / Found {} candidate slides",
                detector.times.len(),
                detector.times.len()
            ));
        }
    }
    detector.finish();

    let status = tokio::select! {
        error = control.watch() => return Err(error),
        status = child.wait() => status?,
    };
    let tail = stderr_task.await??;
    if !status.success() {
        anyhow::bail!(crate::tr!(
            "外部工具 {} 执行失败（退出码 {}）：{}",
            "ffmpeg",
            status,
            tail
        ));
    }
    anyhow::ensure!(
        stamps_rx.try_recv().is_err(),
        crate::tr!("视频帧时间戳缺失或无效，无法可靠定位截图")
    );
    pb.finish();
    tracing::info!(
        slides = detector.times.len(),
        frames = i,
        "precise scan done"
    );
    Ok(detector.times)
}

/// 1fps（或 sample_interval）灰度流 + SSIM，得到新幻灯片时间点。
async fn sample_timestamps(
    cfg: &PipelineConfig,
    media: &Path,
    control: &crate::dispatch::TaskControl,
) -> Result<Vec<(f64, f64)>> {
    control.check()?;
    let info = tokio::select! {
        error = control.watch() => return Err(error),
        info = media::probe_video(media) => info,
    }.context(crate::tr!("无法读取视频尺寸，请检查文件是否可播放 / Could not read video dimensions; check that the file plays correctly"))?;
    if cfg.slide_mode == crate::config::SlideMode::Precise {
        return sample_precise(cfg, media, control, &info).await;
    }
    let interval = cfg.sample_interval;
    let (tw, th) = scaled_wh(info.width, info.height, SAMPLE_WIDTH);
    let fps = 1.0 / interval;
    let vf = format!("fps={fps:.6},scale={tw}:{th},format=gray");

    tracing::info!(
        w = info.width,
        h = info.height,
        duration = format_args!("{:.0}s", info.duration),
        interval,
        similarity = cfg.similarity,
        cooldown = cfg.cooldown,
        "slide sample"
    );

    let mut child = Command::new(crate::tools::program("ffmpeg"))
        .kill_on_drop(true)
        .args(["-hide_banner", "-loglevel", "error", "-an"])
        .arg("-i")
        .arg(media)
        .args(["-map", "0:v:0", "-vf", &vf, "-f", "rawvideo", "pipe:1"])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .context(crate::tr!(
            "无法读取视频画面 / Could not start ffmpeg frame sampling"
        ))?;
    let mut stdout = child.stdout.take().context("ffmpeg stdout")?;
    // stderr 必须全程 drain：piped 而不读的话，写满 64KB 管道缓冲会死锁。
    // 缓存尾部供失败诊断，debug 时逐行转发。
    let mut stderr_buf: Vec<u8> = Vec::new();
    let mut stderr = child.stderr.take().context("ffmpeg stderr")?;
    let stderr_task = tokio::spawn(async move {
        use tokio::io::AsyncReadExt;
        let _ = stderr.read_to_end(&mut stderr_buf).await;
        stderr_buf
    });
    let frame_len = (tw as usize) * (th as usize);
    let mut buf = vec![0u8; frame_len];
    // Duration/interval gives an estimate, not the number ffmpeg will actually
    // decode (especially for VFR input). The bar reports this estimate as the
    // denominator so users see forward progress; actual samples may land one
    // or two frames off, which the label tolerates. Candidate selection
    // happens within this scan.
    let estimate = (info.duration / interval).round().max(1.0) as u64;
    let pb = crate::progress::Bar::new(Stage::ScenesScan, estimate)
        .with_template("{spinner:.green} sample {pos}/{len} frames {msg}");
    pb.set_position(0);

    // 三状态检测：检测永不休眠（cooldown 只限制「发射」，不再造成盲区）。
    //   last_emitted  已输出的视觉状态
    //   candidate     正在观察的候选画面（含首次出现时间，用于真实时间戳）
    // 发射条件：候选与上一输出差异显著 + 稳定时长足够 + 距上次发射 >= cooldown。
    let stable_for = if matches!(cfg.slide_mode, crate::config::SlideMode::Stable) {
        cfg.stable_secs
    } else {
        0.0
    };
    let mut times: Vec<(f64, f64)> = vec![];
    let mut last_emitted: Option<GrayImage> = None;
    let mut candidate: Option<GrayImage> = None;
    let mut candidate_first_t: Option<f64> = None;
    let mut candidate_last_t: Option<f64> = None;
    let mut last_emit_t: f64 = -f64::INFINITY;
    let mut i: u64 = 0;
    loop {
        control.check()?;
        let read = tokio::select! {
            error = control.watch() => return Err(error),
            read = stdout.read_exact(&mut buf) => read,
        };
        match read {
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => break,
            Err(e) => return Err(e.into()),
        }
        // 理想采样时刻 t = i * interval：fps 滤镜的取整与 VFR 补帧会让真实帧
        // 时间略有漂移（通常 < 一个采样间隔）；场景检测只需秒级精度，可接受，
        // 故不逐帧解析 pts。
        let t = i as f64 * interval;
        i += 1;
        pb.inc(1);
        // 帧缓冲复用：move 进 GrayImage 计算，结束时 into_raw 收回供下一帧
        // （取消每帧 230KB 的 buf.clone()；仅物化 candidate 时才产生拥有所有权的拷贝）
        let gray = GrayImage::from_raw(tw, th, std::mem::take(&mut buf)).ok_or_else(|| {
            anyhow::anyhow!("灰度帧尺寸不匹配 {tw}x{th} / Grayscale frame size mismatch {tw}x{th}")
        })?;
        {
            let cmp = crop_roi(&gray, cfg.roi);

            let differs_from_emitted = match &last_emitted {
                None => true,
                Some(prev) => {
                    prev.dimensions() != cmp.dimensions() || ssim(prev, &cmp) < cfg.similarity
                }
            };
            if !differs_from_emitted {
                // 画面回到已输出状态：候选是过渡帧（动画/抖动），丢弃
                candidate = None;
                candidate_first_t = None;
            } else {
                // 与已输出状态不同：跟踪候选（若候选本身又变了，说明动画进行中，重置起点）
                let candidate_changed = match &candidate {
                    None => true,
                    Some(c) => c.dimensions() != cmp.dimensions() || ssim(c, &cmp) < cfg.similarity,
                };
                if candidate_changed {
                    // 仅候选变更时才物化为拥有所有权的图；其余路径 cmp 只借不拷
                    candidate = Some(cmp.into_owned());
                    candidate_first_t = Some(t);
                    candidate_last_t = Some(t);
                } else {
                    // 同一视觉状态的后续采样：更新代表帧时间（取稳定后的最后一帧）
                    candidate_last_t = Some(t);
                }
                let onset_t = candidate_first_t.unwrap_or(t);
                let capture_t = candidate_last_t.unwrap_or(t);
                let stable = t - onset_t >= stable_for;
                let gap_ok = t - last_emit_t >= cfg.cooldown;
                if stable && gap_ok {
                    // 发射两个时间戳：onset 用于时间线对齐，capture 用于全分辨率截帧
                    // （stable 模式下 capture 取确认稳定时的代表帧，避开 transition 早期态）
                    times.push((onset_t, capture_t));
                    last_emitted = candidate.take();
                    candidate_first_t = None;
                    candidate_last_t = None;
                    last_emit_t = t;
                    pb.set_message(crate::tr!(
                        "已找到 {} 张候选截图 / Found {} candidate slides",
                        times.len(),
                        times.len()
                    ));
                }
            }
        }
        buf = gray.into_raw();
    }
    // EOF 冲刷：最后一个已稳定的候选即使还没过 cooldown 也补发（否则尾页永远丢失）
    if let (Some(_), Some(onset), Some(capture)) = (&candidate, candidate_first_t, candidate_last_t)
    {
        let stable = capture - onset >= stable_for;
        if stable {
            times.push((onset, capture));
        }
    }
    let status = tokio::select! {
        error = control.watch() => return Err(error),
        status = child.wait() => status?,
    };
    let stderr_bytes = stderr_task.await.unwrap_or_default();
    if !status.success() {
        let tail = crate::error::tail_lines(&String::from_utf8_lossy(&stderr_bytes), 5);
        anyhow::bail!(crate::tr!(
            "外部工具 {} 执行失败（退出码 {}）：{}",
            "ffmpeg",
            status,
            tail
        ));
    }
    pb.finish();
    tracing::info!(slides = times.len(), frames = i, mode = %cfg.slide_mode, "ssim scan done");
    Ok(times)
}

/// 场景阶段入口。
pub async fn run(
    cfg: &PipelineConfig,
    media: &Path,
    job: &crate::execution::Job,
) -> Result<Vec<FrameEvent>> {
    let control = &job.control;
    let frames_dir = job.frames_dir();
    tokio::fs::create_dir_all(&frames_dir).await?;
    let t0 = std::time::Instant::now();
    crate::progress::start(Stage::ScenesScan);
    let times = sample_timestamps(cfg, media, control).await?;
    crate::progress::done(Stage::ScenesScan);
    anyhow::ensure!(
        !times.is_empty(),
        crate::tr!("未采样到任何帧 / No frames sampled")
    );

    crate::progress::start(Stage::ScenesExtract);
    let pb = crate::progress::Bar::new(Stage::ScenesExtract, times.len() as u64)
        .with_template("{spinner:.green} extract {pos}/{len} [{bar:32.cyan/blue}] {msg}");
    // Emit before launching ffmpeg: the first image may take a long time.
    pb.set_position(0);
    // JoinSet 限流并发抽帧：最多 EXTRACT_CONCURRENCY 个 ffmpeg 进程并行；
    // 按帧索引收集结果后排序，保证输出文件名与结果顺序与串行版完全一致。
    let mut set: tokio::task::JoinSet<(usize, Result<()>)> = tokio::task::JoinSet::new();
    let mut pending = times.iter().copied().enumerate();
    let mut results: Vec<(usize, Result<()>)> = Vec::with_capacity(times.len());
    loop {
        control.check()?;
        while set.len() < EXTRACT_CONCURRENCY {
            let Some((i, (_, capture_t))) = pending.next() else {
                break;
            };
            // 截帧用代表帧时间（稳定后），时间线用 onset（首次出现）
            let path = frames_dir.join(slide_name(i + 1));
            let media = media.to_path_buf();
            let control = control.clone();
            set.spawn(async move { (i, extract_frame(&media, capture_t, &path, &control).await) });
        }
        match set.join_next().await {
            Some(Ok((i, Ok(())))) => {
                pb.inc(1);
                results.push((i, Ok(())));
            }
            Some(Ok((i, Err(error)))) => {
                set.abort_all();
                let (onset_t, _) = times[i];
                return Err(error).with_context(|| {
                    // 中文只写具体原因：界面已用「截图未生成」命名这项失败
                    let secs = onset_t.max(0.) as u64;
                    format!(
                        "{} #{} ({}:{:02})",
                        crate::tr!("无法提取截图"),
                        i + 1,
                        secs / 60,
                        secs % 60
                    )
                });
            }
            Some(Err(e)) => {
                return Err(e).context(crate::tr!(
                    "截图任务异常终止 / Slide extraction stopped unexpectedly"
                ));
            }
            None => break,
        }
    }
    results.sort_by_key(|(i, _)| *i);
    let mut frames = Vec::with_capacity(results.len());
    for (i, r) in results {
        let (onset_t, _) = times[i];
        r.with_context(|| {
            // 中文只写具体原因：界面已用「截图未生成」命名这项失败
            let secs = onset_t.max(0.) as u64;
            format!(
                "{} #{} ({}:{:02})",
                crate::tr!("无法提取截图"),
                i + 1,
                secs / 60,
                secs % 60
            )
        })?;
        let name = slide_name(i + 1);
        frames.push(FrameEvent {
            t: onset_t,
            image: format!("frames/{name}"),
        });
    }
    tracing::info!(
        frames = frames.len(),
        secs = format_args!("{:.1}", t0.elapsed().as_secs_f64()),
        "slides extracted"
    );
    pb.finish();
    crate::progress::done(Stage::ScenesExtract);
    Ok(frames)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filter_generation_resets_only_local_count_not_pts_order() {
        let stamp = |generation, index, seconds| media::FrameStamp {
            generation,
            index,
            seconds,
        };
        assert!(valid_next_stamp(None, stamp(0, 0, 0.0)));
        assert!(valid_next_stamp(Some(stamp(0, 0, 0.0)), stamp(0, 1, 0.5)));
        assert!(valid_next_stamp(Some(stamp(0, 1, 0.5)), stamp(1, 0, 1.0)));
        assert!(!valid_next_stamp(Some(stamp(0, 1, 0.5)), stamp(0, 0, 1.0)));
        assert!(!valid_next_stamp(Some(stamp(0, 1, 0.5)), stamp(1, 1, 1.0)));
        assert!(!valid_next_stamp(Some(stamp(0, 1, 0.5)), stamp(1, 0, 0.5)));
        assert!(!valid_next_stamp(Some(stamp(0, 1, 0.5)), stamp(1, 0, 0.0)));
    }

    #[test]
    fn scale_keeps_even_height() {
        let (w, h) = scaled_wh(1280, 410, 640);
        assert_eq!(w, 640);
        assert_eq!(h % 2, 0);
    }

    fn white(w: u32, h: u32) -> ScanSig {
        ScanSig {
            w,
            h,
            rgb: vec![255; (w * h * 3) as usize],
        }
    }

    /// 在签名上画一块纯色矩形（模拟新增文字行/代码块/批注）。
    fn paint(sig: &mut ScanSig, x0: u32, y0: u32, w: u32, h: u32, value: u8) {
        for y in y0..(y0 + h).min(sig.h) {
            for x in x0..(x0 + w).min(sig.w) {
                let i = ((y * sig.w + x) * 3) as usize;
                sig.rgb[i] = value;
                sig.rgb[i + 1] = value;
                sig.rgb[i + 2] = value;
            }
        }
    }

    #[test]
    fn scan_diff_is_sensitive_to_small_content_change_but_ignores_cursor() {
        let base = white(480, 270);

        // 一行新文字/代码（480 宽下约 150×12）必须判为变化。
        let mut text = base.clone();
        paint(&mut text, 40, 100, 150, 12, 0);
        assert!(scan_diff(&base, &text).changed());

        // 颜色高亮（只改颜色、面积不小）也必须判为变化。
        let mut highlight = base.clone();
        paint(&mut highlight, 40, 140, 150, 12, 255);
        for y in 140..152 {
            for x in 40..190 {
                let i = ((y * highlight.w + x) * 3) as usize;
                highlight.rgb[i] = 255;
                highlight.rgb[i + 1] = 0;
                highlight.rgb[i + 2] = 0;
            }
        }
        assert!(scan_diff(&base, &highlight).changed());

        // 鼠标指针（480 宽下 3×3 左右）不算内容变化。
        let mut cursor = base.clone();
        paint(&mut cursor, 300, 200, 3, 3, 0);
        let d = scan_diff(&base, &cursor);
        assert!(
            !d.changed(),
            "cursor must not count as a slide change: {d:?}"
        );
        assert!(!d.moving(), "cursor must not count as motion");
    }

    const FPS: f64 = 30.0;

    fn frame_times(count: usize) -> impl Iterator<Item = f64> {
        (0..count).map(|i| i as f64 / FPS)
    }

    #[test]
    fn precise_detector_keeps_every_settled_state() {
        let base = white(480, 270);
        let mut detector = PreciseDetector::new();
        let mut states = vec![base.clone()];
        let mut third = base.clone();
        paint(&mut third, 60, 60, 150, 12, 0);
        let mut fourth = third.clone();
        paint(&mut fourth, 60, 90, 150, 12, 0);
        states.push(third);
        states.push(fourth);

        // 每个状态持续 1 秒。
        for t in frame_times(90) {
            let index = ((t / 1.0) as usize).min(states.len() - 1);
            detector.push(t, states[index].clone());
        }
        detector.finish();

        let onsets: Vec<f64> = detector.times.iter().map(|(onset, _)| *onset).collect();
        assert_eq!(onsets, [0.0, 1.0, 2.0]);
    }

    #[test]
    fn precise_detector_keeps_transient_flash() {
        let base = white(480, 270);
        let mut flashed = base.clone();
        paint(&mut flashed, 200, 120, 150, 16, 0);

        let mut detector = PreciseDetector::new();
        for t in frame_times(120) {
            // 2.0s 起闪现两帧（约 0.067s）后恢复。
            let showing = (2.0..2.1).contains(&t);
            detector.push(
                t,
                if showing {
                    flashed.clone()
                } else {
                    base.clone()
                },
            );
        }
        detector.finish();

        // 首帧截图 + 闪现 + 恢复；恢复之后不重复发射。
        assert_eq!(
            detector.times.len(),
            3,
            "flash and restoration must each be captured exactly once"
        );
        let (onset, capture) = detector.times[1];
        assert!((onset - 2.0).abs() < 0.05, "onset={onset}");
        assert!((capture - 2.0).abs() < 0.05, "capture={capture}");
        assert_eq!(detector.times[2], (2.1, 2.1));
    }

    #[test]
    fn flash_keeps_onset_and_unfinished_tail_keeps_final_state() {
        let base = white(80, 40);
        let mut small = base.clone();
        paint(&mut small, 10, 10, 10, 10, 0);
        let mut peak = base.clone();
        paint(&mut peak, 10, 10, 40, 20, 0);
        for restore in [false, true] {
            let mut detector = PreciseDetector::new();
            detector.push(0.0, base.clone());
            detector.push(1.0, small.clone());
            detector.push(1.03, peak.clone());
            if restore {
                detector.push(1.06, base.clone());
            }
            detector.finish();
            if restore {
                assert_eq!(detector.times, [(0.0, 0.0), (1.0, 1.03), (1.06, 1.06)]);
            } else {
                assert_eq!(detector.times[1], (1.03, 1.03));
                assert_eq!(detector.times.len(), 2);
            }
        }
    }

    #[test]
    fn precise_detector_forces_progress_frame_during_continuous_change() {
        let mut detector = PreciseDetector::new();
        // 连续 5 秒每帧都在下方加一行，画面从未稳定。
        for (i, t) in frame_times(150).enumerate() {
            let mut sig = white(480, 270);
            let row = (i as u32).min(269);
            paint(&mut sig, 0, row, 480, 1, 0);
            detector.push(t, sig);
        }
        detector.finish();

        assert!(
            detector.times.len() >= 3,
            "must keep process frames: {:?}",
            detector.times
        );
        assert!(
            detector
                .times
                .windows(2)
                .all(|pair| pair[1].0 - pair[0].0 <= SCAN_MAX_HOLD_SECS + 0.5),
            "process frames must be at most max_hold apart: {:?}",
            detector.times
        );
    }
}
