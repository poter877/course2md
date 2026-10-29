//! 场景检测的合成视频回归测试。
//! 用 ffmpeg 现场生成“色块换页”视频（无需提交 fixture），验证状态机的关键行为：
//! - 换页被检出、时间戳为候选首次出现时间（而非 cooldown 到期时间）
//! - cooldown 期间检测不休眠：跳过的中间页、后续页真实起点仍正确
//!
//! 采样间隔 0.5s，采样时刻为 0、0.5、1.0…；所有换页都放在两次采样之间
//! （x.25 / x.75），源视频 10fps，离最近采样点各有 0.2s 余量。
//! 因此每个采样点看到的画面是确定的，可以断言完整的输出序列。
//!
//! 运行需要 ffmpeg；本地缺少时跳过，CI 中缺少则失败。

mod common;

use std::path::Path;
use std::process::Command;

const INTERVAL: f64 = 0.5;

/// 用 lavfi 生成白底 1280x720 10fps 视频，`pages` 依次整屏填充 (颜色, 起, 止)。
/// 黑/灰/白亮度差异大、SSIM 可区分；不用 drawtext 以兼容无 freetype 的 ffmpeg 构建。
fn make_video(path: &Path, secs: u32, pages: &[(&str, f64, f64)]) {
    let filter = pages
        .iter()
        .map(|(color, from, to)| {
            format!("drawbox=color={color}:t=fill:enable='between(t,{from},{to})'")
        })
        .collect::<Vec<_>>()
        .join(",");
    let st = Command::new("ffmpeg")
        .args(["-hide_banner", "-loglevel", "error", "-y", "-f", "lavfi"])
        .args(["-i", &format!("color=c=white:s=1280x720:d={secs}:r=10")])
        .args(["-vf", &filter])
        .arg(path)
        .status()
        .expect("spawn ffmpeg");
    assert!(st.success());
}

/// 两个场景测试共享的 PipelineConfig 基座；差异字段在调用处用结构体更新语法覆盖。
fn test_cfg(video: &Path, dir: &Path) -> course2md::config::PipelineConfig {
    course2md::config::PipelineConfig {
        url: video.display().to_string(),
        out_root: dir.to_path_buf(),
        similarity: 0.9,
        sample_interval: INTERVAL,
        cooldown: 10.0,
        slide_mode: course2md::config::SlideMode::First,
        stable_secs: 0.0,
        max_height: 1080,
        roi: None,
        threads: 2,
        provider: course2md::config::AsrProvider::Cpu,
        max_speech: 20.0,
        formats: vec![course2md::config::OutputFormat::Md],
        model_dir: dir.to_path_buf(),
        keep_video: true,
        no_download: true,
        resume: false,
        llm: Default::default(),
        asr_api: Default::default(),
        asr_model: None,
        gpu_layers: course2md::config::DEFAULT_GPU_LAYERS,
        mmproj_offload: true,
        transcript_source: course2md::config::TranscriptSource::Asr,
        subtitle_file: None,
        subtitle_offset: 0.0,
        slides: Vec::new(),
        slide_match_threshold: course2md::config::DEFAULT_SLIDE_MATCH_THRESHOLD,
    }
}

fn onsets(cfg: &course2md::config::PipelineConfig, video: &Path) -> Vec<f64> {
    tokio::runtime::Runtime::new()
        .unwrap()
        .block_on(course2md::scene::run(
            cfg,
            video,
            &course2md::execution::Job::detached(cfg.out_root.clone()),
        ))
        .expect("scene run")
        .iter()
        .map(|frame| frame.t)
        .collect()
}

#[test]
fn scene_detects_slides_with_true_timestamps() {
    if !common::ffmpeg_available() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let video = dir.path().join("synthetic.mp4");
    // 0-6.25s 白 A、6.25-7.25s 黑 B（过渡页）、7.25-14.25s 灰 C、14.25-20s 白 D。
    make_video(
        &video,
        20,
        &[
            ("black", 6.25, 7.25),
            ("gray", 7.25, 14.25),
            ("white", 14.25, 20.),
        ],
    );
    let cfg = test_cfg(&video, dir.path());

    // cooldown=10s、first 模式：
    //   0s 发射 A；6.5s B 成为候选，7.5s 被 C 替换（候选持续更新，检测无盲区）；
    //   10s cooldown 到期时发射 C，时间戳是 C 首次被采到的 7.5s（而非 10s）；
    //   14.5s D 成为候选，距上次发射 < 10s 不发射，直到 EOF 冲刷补发尾页。
    assert_eq!(onsets(&cfg, &video), [0.0, 7.5, 14.5]);
}

/// stable 模式：未稳定满 stable_secs 的过渡页不发射，只发射最终稳定态，
/// 且时间线用 onset（候选首现）。
#[test]
fn stable_mode_waits_for_settled_state() {
    if !common::ffmpeg_available() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let video = dir.path().join("synthetic.mp4");
    // 0-2.25s 白、2.25-3.75s 黑（过渡页）、3.75-10s 灰（最终稳定态）。
    make_video(&video, 10, &[("black", 2.25, 3.75), ("gray", 3.75, 10.)]);
    let cfg = course2md::config::PipelineConfig {
        cooldown: 2.0,
        slide_mode: course2md::config::SlideMode::Stable,
        stable_secs: 1.2,
        ..test_cfg(&video, dir.path())
    };

    // 白页 0s 起，1.5s 时稳定满 1.2s 发射；黑页只在 2.5/3.0/3.5s 被采到，
    // 观察到的稳定时长 1.0s < 1.2s，不发射；灰页 4.0s 首现，5.5s 发射。
    assert_eq!(onsets(&cfg, &video), [0.0, 4.0]);
}

/// precise 模式：逐帧分析，稳定态、颜色高亮和短暂闪现都要保留，
/// 而持续移动的小光标不产生截图。
#[test]
fn precise_mode_keeps_every_visual_state() {
    if !common::ffmpeg_available() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let video = dir.path().join("synthetic.mp4");

    // 12s / 10fps / 1280x720 白底：
    //   2s 新增第一行文字（小方块），4s 新增第二行；
    //   6.0–6.5s 闪现红色批注，随后消失；
    //   8s 第一行变成灰色（纯颜色变化）；
    //   10s 整页变灰（换页）。
    let filter = [
        "drawbox=x=100:y=100:w=200:h=40:color=black:t=fill:enable='between(t,2,12)'",
        "drawbox=x=100:y=200:w=200:h=40:color=black:t=fill:enable='between(t,4,12)'",
        "drawbox=x=500:y=300:w=200:h=40:color=red:t=fill:enable='between(t,6,6.5)'",
        "drawbox=x=100:y=100:w=200:h=40:color=gray:t=fill:enable='between(t,8,12)'",
        "drawbox=x=0:y=0:w=1280:h=720:color=gray:t=fill:enable='between(t,10,12)'",
    ]
    .join(",");
    let st = Command::new("ffmpeg")
        .args(["-hide_banner", "-loglevel", "error", "-y", "-f", "lavfi"])
        .args(["-i", "color=c=white:s=1280x720:d=12:r=10"])
        .args(["-vf", &filter])
        .args(["-pix_fmt", "yuv420p"])
        .arg(&video)
        .status()
        .expect("spawn ffmpeg");
    assert!(st.success());

    let cfg = course2md::config::PipelineConfig {
        slide_mode: course2md::config::SlideMode::Precise,
        ..test_cfg(&video, dir.path())
    };
    let found = onsets(&cfg, &video);

    // 每个视觉状态恰好一张；瞬时批注（6.0s）也在其中。
    let expected = [0.0, 2.0, 4.0, 6.0, 6.6, 8.0, 10.0];
    let mut matched = Vec::new();
    for &want in &expected {
        let hit = found
            .iter()
            .copied()
            .find(|t| (t - want).abs() < 0.35 && !matched.contains(t));
        assert!(hit.is_some(), "missing change at {want}s; found {found:?}");
        matched.push(hit.unwrap());
    }
    assert_eq!(
        matched.len(),
        found.len(),
        "unexpected extra screenshots: {found:?}"
    );
}

#[test]
fn precise_short_flash_restores_image_and_subtitle_ownership() {
    if !common::ffmpeg_available() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let video = dir.path().join("flash.mp4");
    // One 10fps frame, shorter than the detector's settling window.
    make_video(&video, 4, &[("black", 1.99, 2.01)]);
    let cfg = course2md::config::PipelineConfig {
        slide_mode: course2md::config::SlideMode::Precise,
        ..test_cfg(&video, dir.path())
    };
    let frames = tokio::runtime::Runtime::new()
        .unwrap()
        .block_on(course2md::scene::run(
            &cfg,
            &video,
            &course2md::execution::Job::detached(dir.path().to_path_buf()),
        ))
        .unwrap();
    assert_eq!(
        frames.iter().map(|f| f.t).collect::<Vec<_>>(),
        [0.0, 2.0, 2.1]
    );
    // Check actual extracted pixels, not only event timestamps.
    let luminance: Vec<_> = frames
        .iter()
        .map(|f| {
            image::open(dir.path().join(&f.image))
                .unwrap()
                .to_luma8()
                .get_pixel(100, 100)
                .0[0]
        })
        .collect();
    assert!(
        luminance[0] > 240 && luminance[1] < 15 && luminance[2] > 240,
        "{luminance:?}"
    );
    let speech = course2md::subtitle::parse_subtitle(
        "1\n00:00:02,000 --> 00:00:02,080\nflash\n\n2\n00:00:02,100 --> 00:00:03,000\nrestored\n",
    );
    let sections = course2md::timeline::merge(frames, speech, 4.0);
    assert!(sections[0].speech.is_empty());
    assert_eq!(sections[1].speech[0].text, "flash");
    assert_eq!(sections[1].end, 2.1);
    assert_eq!(sections[2].speech[0].text, "restored");
    assert_eq!(sections[2].end, 4.0);
}

/// Minimal two-page PDF with different text and background colors.
fn write_slide_pdf(path: &Path) {
    let streams = [
        "1 1 1 rg 0 0 320 180 re f 0 0 0 rg BT /F1 24 Tf 30 90 Td (Alpha) Tj ET",
        "0 0 0 rg 0 0 320 180 re f 1 1 1 rg BT /F1 24 Tf 30 90 Td (Beta) Tj ET",
    ];
    let mut objects = vec![
        "<< /Type /Catalog /Pages 2 0 R >>".to_string(),
        "<< /Type /Pages /Kids [3 0 R 4 0 R] /Count 2 >>".to_string(),
        "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 320 180] /Resources << /Font << /F1 5 0 R >> >> /Contents 6 0 R >>".to_string(),
        "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 320 180] /Resources << /Font << /F1 5 0 R >> >> /Contents 7 0 R >>".to_string(),
        "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".to_string(),
    ];
    objects.extend(streams.map(|s| format!("<< /Length {} >>\nstream\n{s}\nendstream", s.len())));
    let mut pdf = String::from("%PDF-1.4\n");
    let mut offsets = Vec::new();
    for (i, object) in objects.iter().enumerate() {
        offsets.push(pdf.len());
        pdf.push_str(&format!("{} 0 obj\n{object}\nendobj\n", i + 1));
    }
    let xref = pdf.len();
    pdf.push_str(&format!(
        "xref\n0 {}\n0000000000 65535 f \n",
        objects.len() + 1
    ));
    for offset in offsets {
        pdf.push_str(&format!("{offset:010} 00000 n \n"));
    }
    pdf.push_str(&format!(
        "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n",
        objects.len() + 1
    ));
    std::fs::write(path, pdf).unwrap();
}

#[test]
fn pdf_prepare_renders_text_and_pages_without_reusing_stale_outputs() {
    // Poppler is optional in CI; the cancellation tests use a controlled process.
    if ["pdftoppm", "pdftotext"]
        .iter()
        .any(|tool| Command::new(tool).arg("-v").output().is_err())
    {
        eprintln!("Poppler unavailable; skipping PDF rendering integration test");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let pdf = dir.path().join("slides.pdf");
    write_slide_pdf(&pdf);
    let stale = dir.path().join(".slide-match");
    std::fs::create_dir(&stale).unwrap();
    std::fs::write(stale.join("full-1-003.jpg"), b"stale partial render").unwrap();
    std::fs::write(stale.join("low-1-003.png"), b"stale partial render").unwrap();
    let cfg = course2md::config::PipelineConfig {
        slides: vec![pdf.clone()],
        ..test_cfg(&pdf, dir.path())
    };
    let pages = tokio::runtime::Runtime::new()
        .unwrap()
        .block_on(course2md::slides::prepare(
            &cfg,
            dir.path(),
            &course2md::dispatch::TaskControl::default(),
        ))
        .unwrap();
    assert_eq!(pages.len(), 2);
    assert_eq!(pages[0].text, "Alpha");
    assert_eq!(pages[1].text, "Beta");
    for (i, page) in pages.iter().enumerate() {
        assert_eq!(page.deck, 1);
        assert_eq!(page.page, (i + 1) as u32);
        assert_eq!(page.gray.dimensions(), (320, 180));
        assert!(image::open(dir.path().join(&page.image)).is_ok());
    }
    assert!(pages[0].gray.get_pixel(0, 0).0[0] > 240);
    assert!(pages[1].gray.get_pixel(0, 0).0[0] < 15);
    assert!(!std::fs::read_dir(dir.path()).unwrap().any(|entry| {
        entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with(".slide-match-")
    }));
}

fn run_precise(video: &Path, root: &Path) -> Vec<course2md::timeline::FrameEvent> {
    let cfg = course2md::config::PipelineConfig {
        slide_mode: course2md::config::SlideMode::Precise,
        ..test_cfg(video, root)
    };
    tokio::runtime::Runtime::new()
        .unwrap()
        .block_on(course2md::scene::run(
            &cfg,
            video,
            &course2md::execution::Job::detached(root.to_path_buf()),
        ))
        .unwrap()
}

/// Encode known source frames, optionally giving every frame a nonuniform PTS.
/// ffprobe independently supplies the expected decoded timeline, not nominal FPS.
fn encode_frames(
    root: &Path,
    frames: &[image::RgbImage],
    pts: Option<&[f64]>,
) -> std::path::PathBuf {
    let inputs = root.join("input");
    std::fs::create_dir_all(&inputs).unwrap();
    for (i, image) in frames.iter().enumerate() {
        image.save(inputs.join(format!("{i:04}.png"))).unwrap();
    }
    let video = root.join("source.mp4");
    let mut cmd = Command::new("ffmpeg");
    cmd.args([
        "-hide_banner",
        "-loglevel",
        "error",
        "-y",
        "-framerate",
        "30",
        "-i",
    ])
    .arg(inputs.join("%04d.png"));
    if let Some(pts) = pts {
        assert_eq!(frames.len(), pts.len());
        let mut expression = "0".to_string();
        for (i, t) in pts.iter().enumerate().rev() {
            expression = format!(
                "if(eq(N,{i}),{},{expression})",
                (t * 90000.0).round() as i64
            );
        }
        cmd.args(["-vf", &format!("settb=1/90000,setpts='{expression}'")]);
    }
    let output = cmd
        .args([
            "-fps_mode",
            "passthrough",
            "-enc_time_base",
            "1/90000",
            "-c:v",
            "libx264",
            "-crf",
            "0",
            "-pix_fmt",
            "yuv420p",
            "-video_track_timescale",
            "90000",
        ])
        .arg(&video)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    video
}

fn decoded_pts(video: &Path) -> Vec<f64> {
    let output = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-select_streams",
            "v:0",
            "-show_frames",
            "-show_entries",
            "frame=best_effort_timestamp_time",
            "-of",
            "json",
        ])
        .arg(video)
        .output()
        .unwrap();
    assert!(output.status.success());
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    value["frames"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| {
            f["best_effort_timestamp_time"]
                .as_str()
                .unwrap()
                .parse()
                .unwrap()
        })
        .collect()
}

fn solid(value: u8) -> image::RgbImage {
    image::RgbImage::from_pixel(480, 270, image::Rgb([value; 3]))
}

fn assert_frame_values(
    root: &Path,
    frames: &[course2md::timeline::FrameEvent],
    times: &[f64],
    values: &[u8],
) {
    assert_eq!(
        frames.len(),
        times.len(),
        "events: {:?}",
        frames.iter().map(|f| f.t).collect::<Vec<_>>()
    );
    for ((frame, expected_t), expected_value) in frames.iter().zip(times).zip(values) {
        assert!(
            (frame.t - expected_t).abs() < 0.000002,
            "{} != {expected_t}",
            frame.t
        );
        let image = image::open(root.join(&frame.image)).unwrap().to_luma8();
        let value = image.get_pixel(100, 100).0[0];
        assert!(
            value.abs_diff(*expected_value) < 5,
            "at {}: pixel {value}, expected {expected_value}",
            frame.t
        );
    }
}

#[test]
fn precise_keeps_single_frame_page_sequence_and_return_at_fractional_pts() {
    if !common::ffmpeg_available() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let values = [255, 0, 90, 180, 255];
    let video = encode_frames(dir.path(), &values.map(solid), None);
    let pts = decoded_pts(&video);
    let frames = run_precise(&video, dir.path());
    assert_frame_values(dir.path(), &frames, &pts, &values);
}

#[test]
fn precise_vfr_uses_decoded_pts_including_a_sub_nominal_frame_flash() {
    if !common::ffmpeg_available() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let values = [255, 0, 90, 180, 255, 0, 90, 255];
    let times = [0.0, 0.031, 0.032, 0.089, 0.410, 1.200, 1.201, 2.1];
    let video = encode_frames(dir.path(), &values.map(solid), Some(&times));
    let pts = decoded_pts(&video);
    assert_eq!(pts.len(), times.len());
    assert!(pts.windows(2).map(|p| p[1] - p[0]).any(|dt| dt < 0.002));
    let frames = run_precise(&video, dir.path());
    assert_frame_values(dir.path(), &frames, &pts, &values);
}

fn paint_box(image: &mut image::RgbImage, x: u32, y: u32, width: u32, height: u32) {
    for py in y..(y + height).min(image.height()) {
        for px in x..(x + width).min(image.width()) {
            image.put_pixel(px, py, image::Rgb([0; 3]));
        }
    }
}

fn assert_image_matches(
    root: &Path,
    frame: &course2md::timeline::FrameEvent,
    expected: &image::RgbImage,
) {
    let actual = image::open(root.join(&frame.image)).unwrap().to_rgb8();
    assert_eq!(actual.dimensions(), expected.dimensions());
    let error: f64 = actual
        .as_raw()
        .iter()
        .zip(expected.as_raw())
        .map(|(a, b)| f64::from(a.abs_diff(*b)))
        .sum();
    let mean = error / actual.as_raw().len() as f64;
    assert!(
        mean < 2.0,
        "at {}: image differs from its timed state, mean error {mean}",
        frame.t
    );
}

#[test]
fn precise_short_local_pages_and_board_growth_keep_each_state_onset() {
    if !common::ffmpeg_available() {
        return;
    }
    // Two-frame local changes (67 ms) never reach the 120 ms settling timer.
    // These differ in less than 10% of the image, so full-screen cut detection
    // alone cannot preserve them. A later return to the original page is real.
    let base = solid(255);
    let mut states = vec![base.clone()];
    for x in [30, 170, 310] {
        let mut state = base.clone();
        paint_box(&mut state, x, 80, 100, 25);
        states.push(state);
    }
    states.push(base.clone());
    let mut growth = vec![base];
    for i in 0..5 {
        let mut state = growth.last().unwrap().clone();
        paint_box(&mut state, 40, 40 + i * 30, 260, 10);
        growth.push(state);
    }
    for (states, hold_frames) in [(states, 2), (growth, 15)] {
        let dir = tempfile::tempdir().unwrap();
        let source: Vec<_> = states
            .iter()
            .flat_map(|state| std::iter::repeat_n(state.clone(), hold_frames))
            .collect();
        let video = encode_frames(dir.path(), &source, None);
        let pts = decoded_pts(&video);
        let frames = run_precise(&video, dir.path());
        assert_eq!(
            frames.len(),
            states.len(),
            "events: {:?}",
            frames.iter().map(|f| f.t).collect::<Vec<_>>()
        );
        for (i, (frame, state)) in frames.iter().zip(&states).enumerate() {
            assert!((frame.t - pts[i * hold_frames]).abs() < 0.000002);
            assert_image_matches(dir.path(), frame, state);
        }
    }
}

#[test]
fn precise_continuous_code_scroll_keeps_bounded_snapshots_and_the_final_view() {
    if !common::ffmpeg_available() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let source: Vec<_> = (0..160)
        .map(|n| {
            let mut image = solid(255);
            // Text-like rows continuously move upward, never settling.
            for row in 0..12 {
                let y = (row * 24 + 270 - (n * 2) % 270) % 270;
                paint_box(&mut image, 30, y, 180 + (row % 4) * 40, 6);
            }
            image
        })
        .collect();
    let video = encode_frames(dir.path(), &source, None);
    let pts = decoded_pts(&video);
    let frames = run_precise(&video, dir.path());
    assert!(
        (3..=5).contains(&frames.len()),
        "continuous motion must be bounded: {frames:?}"
    );
    assert!(frames.windows(2).all(|pair| pair[1].t - pair[0].t <= 2.1));
    for frame in &frames {
        let index = pts
            .iter()
            .position(|t| (t - frame.t).abs() < 0.000002)
            .expect("snapshot onset is a decoded frame PTS");
        assert_image_matches(dir.path(), frame, &source[index]);
    }
    let last = frames.last().unwrap();
    assert!(
        (last.t - pts.last().unwrap()).abs() < 0.000002,
        "the final viewport must not be replaced by an earlier peak"
    );
    assert_image_matches(dir.path(), last, source.last().unwrap());
}

#[test]
fn precise_normalizes_nonzero_container_start_consistently_with_seeking() {
    if !common::ffmpeg_available() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let values = [255, 0, 90, 255];
    let video = encode_frames(
        dir.path(),
        &values.map(solid),
        Some(&[0.0, 0.037, 0.062, 0.4]),
    );
    let shifted = dir.path().join("shifted.mp4");
    let output = Command::new("ffmpeg")
        .args(["-hide_banner", "-loglevel", "error", "-y", "-i"])
        .arg(&video)
        .args(["-c", "copy", "-output_ts_offset", "7.0"])
        .arg(&shifted)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let absolute_pts = decoded_pts(&shifted);
    assert!((absolute_pts[0] - 7.0).abs() < 0.000002);
    let relative: Vec<_> = absolute_pts.iter().map(|t| t - absolute_pts[0]).collect();
    let frames = run_precise(&shifted, dir.path());
    assert_frame_values(dir.path(), &frames, &relative, &values);
}

#[test]
fn every_scan_mode_and_capture_use_the_probed_first_video_stream() {
    if !common::ffmpeg_available() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let video = dir.path().join("dual.mkv");
    let output = Command::new("ffmpeg")
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-y",
            "-f",
            "lavfi",
            "-i",
            "color=red:s=160x90:r=2:d=1",
            "-f",
            "lavfi",
            "-i",
            "color=blue:s=320x180:r=2:d=1",
            "-map",
            "0:v:0",
            "-map",
            "1:v:0",
            "-c:v",
            "ffv1",
            "-disposition:v:0",
            "0",
            "-disposition:v:1",
            "0",
        ])
        .arg(&video)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let info = runtime
        .block_on(course2md::media::probe_video(&video))
        .unwrap();
    assert_eq!((info.width, info.height), (160, 90));
    for mode in [
        course2md::config::SlideMode::First,
        course2md::config::SlideMode::Stable,
        course2md::config::SlideMode::Precise,
    ] {
        let work = tempfile::tempdir().unwrap();
        let cfg = course2md::config::PipelineConfig {
            slide_mode: mode,
            ..test_cfg(&video, work.path())
        };
        let frames = runtime
            .block_on(course2md::scene::run(
                &cfg,
                &video,
                &course2md::execution::Job::detached(work.path().to_path_buf()),
            ))
            .unwrap();
        assert!(!frames.is_empty());
        for frame in frames {
            let image = image::open(work.path().join(frame.image))
                .unwrap()
                .to_rgb8();
            assert_eq!(image.dimensions(), (160, 90));
            let pixel = image.get_pixel(80, 45).0;
            assert!(
                pixel[0] > 240 && pixel[1] < 10 && pixel[2] < 10,
                "{mode:?}: {pixel:?}"
            );
        }
    }
}

#[test]
fn precise_accepts_filter_reinitialization_with_continuous_pts() {
    if !common::ffmpeg_available() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let mut bytes = Vec::new();
    for (i, size) in ["160x90", "320x180"].iter().enumerate() {
        let segment = dir.path().join(format!("segment-{i}.ts"));
        let output = Command::new("ffmpeg")
            .args([
                "-hide_banner",
                "-loglevel",
                "error",
                "-y",
                "-f",
                "lavfi",
                "-i",
                &format!("color=red:s={size}:r=2:d=1"),
                "-c:v",
                "libx264",
                "-bf",
                "0",
                "-f",
                "mpegts",
            ])
            .arg(&segment)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        bytes.extend(std::fs::read(segment).unwrap());
    }
    let video = dir.path().join("changed.ts");
    std::fs::write(&video, bytes).unwrap();
    // Check the reproduction itself: two filter generations, each starting n=0.
    let output = Command::new("ffmpeg")
        .args(["-hide_banner", "-nostats", "-loglevel", "info", "-an", "-i"])
        .arg(&video)
        .args([
            "-map",
            "0:v:0",
            "-vf",
            "scale=480:270,format=rgb24,showinfo=checksum=0",
            "-fps_mode",
            "passthrough",
            "-f",
            "null",
            "-",
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(
        stderr.matches("config in time_base:").count(),
        2,
        "{stderr}"
    );
    let local: Vec<_> = stderr
        .lines()
        .filter_map(|line| {
            line.split_once(" n:")
                .map(|(_, tail)| tail.split_whitespace().next().unwrap())
        })
        .collect();
    assert_eq!(local, ["0", "1", "0", "1"]);
    let frames = run_precise(&video, dir.path());
    assert_eq!(frames.len(), 1, "same red slide survives resolution change");
    assert_eq!(frames[0].t, 0.0);
    let pixel = image::open(dir.path().join(&frames[0].image))
        .unwrap()
        .to_rgb8()
        .get_pixel(20, 20)
        .0;
    assert!(
        pixel[0] > 240 && pixel[1] < 10 && pixel[2] < 10,
        "{pixel:?}"
    );
}
