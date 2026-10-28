//! 外部幻灯片 PDF：渲染页图并抽取页文字。
//!
//! 这一模式把视频当“翻页时钟”，把 PDF 当“成图来源”：视频负责给出每页
//! 出现在屏幕上的时间，PDF 页负责清晰度、页码和可检索的页文字。渲染与
//! 取字都走 poppler 的 `pdftoppm` / `pdftotext`，和 ffmpeg 一样属于外部
//! 工具（可用 `COURSE2MD_PDFTOPPM` / `COURSE2MD_PDFTOTEXT` 指定）。

use crate::{config::PipelineConfig, dispatch::TaskControl, tools};
use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use tokio::process::Command;

/// 匹配用的低分辨率宽度（灰度 PNG）。足够区分版面，又不拖慢 SSIM。
const MATCH_WIDTH: u32 = 320;

/// 一页幻灯片：图、页码、页文字和匹配用的灰度缩略图。
pub struct SlidePage {
    /// 第几套 PDF（1 起）
    pub deck: usize,
    /// PDF 页码（1 起）
    pub page: u32,
    /// 相对工作目录的成图路径，如 `slides/d1-0038.jpg`
    pub image: String,
    /// `pdftotext -layout` 抽出的页文字（已折叠空白）
    pub text: String,
    /// 匹配用灰度图（宽度 `MATCH_WIDTH`）
    pub gray: image::GrayImage,
}

/// 渲染并读取全部幻灯片页。调用方保证 `cfg.slides` 非空。
pub async fn prepare(
    cfg: &PipelineConfig,
    work: &Path,
    control: &TaskControl,
) -> Result<Vec<SlidePage>> {
    control.check()?;
    let slides_dir = work.join("slides");
    std::fs::create_dir_all(&slides_dir)?;
    // 每次渲染有独立目录；取消留下的旧页不得混进下一次较短的 PDF。
    let temporary = tempfile::Builder::new()
        .prefix(".slide-match-")
        .tempdir_in(work)?;
    let match_dir = temporary.path();
    let pdftoppm = tools::program("pdftoppm");
    let pdftotext = tools::program("pdftotext");

    let mut pages = Vec::new();
    for (index, pdf) in cfg.slides.iter().enumerate() {
        control.check()?;
        let deck = index + 1;
        anyhow::ensure!(
            pdf.is_file(),
            crate::tr!("找不到幻灯片 PDF：{}", pdf.display())
        );
        let page_text = extract_text(&pdftotext, pdf, control).await?;
        let full_prefix = match_dir.join(format!("full-{deck}"));
        let low_prefix = match_dir.join(format!("low-{deck}"));
        render(
            &pdftoppm,
            pdf,
            &full_prefix,
            &["-jpeg", "-r", "110", "-jpegopt", "quality=85"],
            control,
        )
        .await?;
        render(
            &pdftoppm,
            pdf,
            &low_prefix,
            &["-png", "-gray", "-scale-to", &MATCH_WIDTH.to_string()],
            control,
        )
        .await?;
        let fulls = numbered(&full_prefix, "jpg", control)?;
        let lows = numbered(&low_prefix, "png", control)?;
        anyhow::ensure!(
            !fulls.is_empty()
                && fulls.len() == lows.len()
                && fulls
                    .iter()
                    .zip(&lows)
                    .enumerate()
                    .all(|(i, (full, low))| { full.0 == low.0 && full.0 == (i + 1) as u32 }),
            crate::tr!("幻灯片渲染页码不完整或不一致：{}", pdf.display())
        );
        for ((page, full), (_, low)) in fulls.iter().zip(&lows) {
            control.check()?;
            let page = *page;
            let image = format!("slides/d{deck}-{page:04}.jpg");
            let dest = work.join(&image);
            std::fs::rename(full, &dest)
                .with_context(|| crate::tr!("无法保存幻灯片页：{}", dest.display()))?;
            let gray = image::open(low)
                .with_context(|| crate::tr!("无法读取幻灯片缩略图：{}", low.display()))?
                .to_luma8();
            let _ = std::fs::remove_file(low);
            pages.push(SlidePage {
                deck,
                page,
                image,
                text: page_text
                    .get(page as usize - 1)
                    .cloned()
                    .unwrap_or_default(),
                gray,
            });
        }
    }
    control.check()?;
    anyhow::ensure!(!pages.is_empty(), crate::tr!("幻灯片 PDF 没有可用页面"));
    Ok(pages)
}

/// 跑一次 `pdftoppm`，输出 `<prefix>-NN.<ext>`。
async fn render(
    pdftoppm: &Path,
    pdf: &Path,
    prefix: &Path,
    args: &[&str],
    control: &TaskControl,
) -> Result<()> {
    let mut cmd = Command::new(pdftoppm);
    cmd.args(args).arg(pdf).arg(prefix);
    run_pdf_cmd(&mut cmd, "pdftoppm", control).await?;
    Ok(())
}

/// 不包装控制错误，保留 Failure 类型供 pipeline 判断暂停/取消。
/// output future 被取消时 kill_on_drop 终止子进程，stdout/stderr 并行排空。
async fn run_pdf_cmd(
    cmd: &mut Command,
    what: &str,
    control: &TaskControl,
) -> Result<std::process::Output> {
    control.check()?;
    cmd.kill_on_drop(true).stdin(std::process::Stdio::null());
    let output = tokio::select! {
        biased;
        error = control.watch() => return Err(error),
        output = cmd.output() => output.with_context(|| crate::tr!(
            "无法启动 {}；请安装 Poppler（pdftoppm / pdftotext）以使用幻灯片 PDF", what
        ))?,
    };
    control.check()?;
    anyhow::ensure!(
        output.status.success(),
        crate::tr!(
            "外部工具 {} 执行失败（退出码 {}）：{}",
            what,
            output
                .status
                .code()
                .map(|code| code.to_string())
                .unwrap_or_else(|| "—".into()),
            crate::error::tail_lines(&String::from_utf8_lossy(&output.stderr), 5)
        )
    );
    Ok(output)
}

/// 读整本 PDF 的文字，按换页符切成每页一段。
async fn extract_text(pdftotext: &Path, pdf: &Path, control: &TaskControl) -> Result<Vec<String>> {
    let mut cmd = Command::new(pdftotext);
    cmd.args(["-layout"]).arg(pdf).arg("-");
    let output = run_pdf_cmd(&mut cmd, "pdftotext", control).await?;
    let text = String::from_utf8_lossy(&output.stdout);
    let mut pages: Vec<String> = text
        .split('\u{000c}')
        .map(|page| page.split_whitespace().collect::<Vec<_>>().join(" "))
        .collect();
    if pages.last().is_some_and(|page| page.is_empty()) {
        pages.pop();
    }
    Ok(pages)
}

/// 列出 `<prefix>-NN.<ext>` 并按页码数字排序。
fn numbered(prefix: &Path, ext: &str, control: &TaskControl) -> Result<Vec<(u32, PathBuf)>> {
    let dir = prefix.parent().context(crate::tr!("幻灯片临时路径无效"))?;
    let stem = prefix
        .file_name()
        .context(crate::tr!("幻灯片临时路径无效"))?
        .to_string_lossy()
        .into_owned();
    let mut found: Vec<(u32, PathBuf)> = Vec::new();
    let prefix = format!("{stem}-");
    let suffix = format!(".{ext}");
    for entry in std::fs::read_dir(dir)? {
        control.check()?;
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        let Some(digits) = name
            .strip_prefix(&prefix)
            .and_then(|s| s.strip_suffix(&suffix))
        else {
            continue;
        };
        if let Ok(page) = digits.parse::<u32>() {
            found.push((page, entry.path()));
        }
    }
    found.sort_by_key(|(page, _)| *page);
    Ok(found)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numbered_rejects_malformed_suffixes_and_preserves_actual_page_numbers() {
        let dir = tempfile::tempdir().unwrap();
        for name in [
            "full-1-10.jpg",
            "full-1-2.jpg",
            "full-1-02oops.jpg",
            "full-2-1.jpg",
            "full-1-3.png",
        ] {
            std::fs::write(dir.path().join(name), b"").unwrap();
        }
        let found = numbered(&dir.path().join("full-1"), "jpg", &TaskControl::default()).unwrap();
        assert_eq!(
            found.iter().map(|(page, _)| *page).collect::<Vec<_>>(),
            [2, 10]
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn pdf_process_cancellation_keeps_control_error_and_terminates_child() {
        use crate::dispatch::{ControlSource, Failure};
        use std::time::Duration;
        let dir = tempfile::tempdir().unwrap();
        let control_path = dir.path().join("control.json");
        let pid_path = dir.path().join("child.pid");
        let control = TaskControl::open(
            dir.path(),
            ControlSource {
                control_path: Some(control_path.clone()),
                ..Default::default()
            },
        )
        .unwrap();
        let mut cmd = Command::new("sh");
        cmd.args(["-c", "echo $$ > \"$1\"; exec sleep 30", "pdf-test"])
            .arg(&pid_path);
        let cancel = async {
            // Cancel only after the actual subprocess has started.
            while !pid_path.is_file() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            crate::checkpoint::atomic_write(&control_path, br#"{"intent":"cancel"}"#).unwrap();
        };
        let (result, ()) = tokio::time::timeout(Duration::from_secs(5), async {
            tokio::join!(run_pdf_cmd(&mut cmd, "pdftoppm", &control), cancel)
        })
        .await
        .expect("PDF cancellation must interrupt the running process");
        let error = result.unwrap_err();
        assert!(error.downcast_ref::<Failure>().is_some(), "{error:#}");
        let pid: i32 = std::fs::read_to_string(&pid_path)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        tokio::time::timeout(Duration::from_secs(2), async {
            // kill(pid, 0) only probes this test's child; it sends no signal.
            while unsafe { libc::kill(pid, 0) } == 0 {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("cancelled PDF child must be killed and reaped");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn pdf_tool_errors_keep_exit_status_and_stderr() {
        let mut cmd = Command::new("sh");
        cmd.args(["-c", "printf 'invalid-pdf-fixture\\n' >&2; exit 7"]);
        let error = run_pdf_cmd(&mut cmd, "pdftotext", &TaskControl::default())
            .await
            .unwrap_err();
        let message = error.to_string();
        assert!(
            message.contains("pdftotext")
                && message.contains('7')
                && message.contains("invalid-pdf-fixture"),
            "{message}"
        );
        assert!(error.downcast_ref::<crate::dispatch::Failure>().is_none());
    }
}
