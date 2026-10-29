//! Read published versions and isolate damaged library entries.
use crate::backend::Done;
use anyhow::{Context, Result};
use course2md::artifact;
use gpui::SharedString;
use std::{
    collections::VecDeque,
    path::{Path, PathBuf},
    time::SystemTime,
};

fn version_damaged() -> &'static str {
    course2md::tr!("笔记版本信息已损坏")
}
fn body_unreadable() -> &'static str {
    course2md::tr!("笔记正文无法读取，原文件已保留")
}

#[derive(Clone)]
pub struct Course {
    pub dir: PathBuf,
    pub title: String,
    pub modified: SystemTime,
    pub slides: usize,
    pub thumbnail: Option<PathBuf>,
    pub manifest: Option<artifact::Manifest>,
    pub warning: Option<String>,
}
impl Course {
    /// A just-published version. `read_preview` reads its manifest on open and
    /// reports a failure there instead of opening an empty note.
    pub fn from_completed(done: &Done) -> Self {
        Self {
            dir: done.out_dir.clone(),
            title: done.title.clone(),
            modified: SystemTime::now(),
            slides: done.slides,
            thumbnail: None,
            manifest: None,
            warning: None,
        }
    }
    pub fn content_description(&self) -> String {
        if self.slides == 1 {
            // English counts its noun; the catalog keeps the singular apart.
            course2md::tr!("含 1 张截图").to_owned()
        } else if self.slides > 0 {
            course2md::tr!("含 {} 张截图", self.slides)
        } else {
            course2md::tr!("文字笔记").to_owned()
        }
    }
    pub fn incomplete(&self) -> bool {
        self.manifest.as_ref().is_some_and(has_incomplete_content)
    }
    /// Published versions live in `<note>/versions/<id>`; the note folder owns them.
    pub fn storage_dir(&self) -> PathBuf {
        self.dir
            .parent()
            .filter(|parent| parent.file_name().is_some_and(|name| name == "versions"))
            .and_then(Path::parent)
            .unwrap_or(&self.dir)
            .to_path_buf()
    }
}
/// Optional file exports describe a task's output, not the completeness of its note.
/// Published manifests stay immutable when a later task successfully repairs exports.
pub(crate) fn has_incomplete_content(manifest: &artifact::Manifest) -> bool {
    !processing_issues(manifest).is_empty()
}
pub struct LibraryScan {
    pub courses: Vec<Course>,
    pub issues: Vec<String>,
}

/// Listing checks that each required file exists at its recorded size; content
/// digests are verified when the note is opened.
fn assets_present(dir: &Path, manifest: &artifact::Manifest) -> bool {
    manifest
        .assets
        .iter()
        .filter(|asset| !asset.path.starts_with("exports/"))
        .all(|asset| {
            artifact::safe_asset_path(dir, &asset.path)
                .and_then(|path| Ok(std::fs::metadata(path)?.len()))
                .is_ok_and(|bytes| bytes == asset.bytes)
        })
}

fn version_course(dir: &Path) -> Result<Course> {
    let _read_guard = course2md::note_edit::read_guard(dir)?;
    let manifest = artifact::read_manifest(&dir.join("manifest.json"))?;
    anyhow::ensure!(
        artifact::safe_asset_path(dir, &manifest.document).is_ok_and(|path| path.is_file()),
        body_unreadable()
    );
    let images = manifest
        .frames
        .iter()
        .filter_map(|frame| artifact::safe_asset_path(dir, &frame.image).ok())
        .collect::<Vec<_>>();
    Ok(Course {
        dir: dir.to_path_buf(),
        title: manifest.title.clone(),
        modified: SystemTime::UNIX_EPOCH + std::time::Duration::from_millis(manifest.created_at_ms),
        slides: images.len(),
        thumbnail: images.into_iter().next(),
        manifest: Some(manifest),
        warning: None,
    })
}

fn current_course(dir: &Path) -> Result<Course> {
    let current = || -> Result<Course> {
        let pointer: artifact::CurrentVersion =
            serde_json::from_slice(&std::fs::read(dir.join("current.json"))?)?;
        anyhow::ensure!(
            pointer.schema == 1,
            course2md::tr!("这份笔记由更新版本的应用生成，请更新应用")
        );
        let manifest_path = artifact::safe_asset_path(dir, &pointer.manifest)?;
        let course = version_course(manifest_path.parent().context(version_damaged())?)?;
        anyhow::ensure!(
            course.manifest.as_ref().is_some_and(|manifest| {
                manifest.course_id == pointer.course_id && manifest.version_id == pointer.version_id
            }),
            version_damaged()
        );
        Ok(course)
    };
    current().or_else(|error| {
        let mut intact = std::fs::read_dir(dir.join("versions"))
            .into_iter()
            .flatten()
            .filter_map(Result::ok)
            .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
            .filter_map(|entry| version_course(&entry.path()).ok())
            .filter(|course| {
                course
                    .manifest
                    .as_ref()
                    .is_some_and(|manifest| assets_present(&course.dir, manifest))
            })
            .collect::<Vec<_>>();
        intact.sort_by_key(|course| course.manifest.as_ref().map(|m| m.revision));
        let mut course = intact.pop().ok_or(error)?;
        course.warning =
            Some(course2md::tr!("最新版本无法读取，正在显示上一个完好的版本。").into());
        Ok(course)
    })
}

pub fn scan_library(root: &Path) -> Result<LibraryScan> {
    let mut scan = LibraryScan {
        courses: Vec::new(),
        issues: Vec::new(),
    };
    if !root.exists() {
        return Ok(scan);
    }
    let mut queue = VecDeque::from([(root.to_path_buf(), 0)]);
    while let Some((dir, depth)) = queue.pop_front() {
        if dir.join("current.json").is_file() {
            match current_course(&dir) {
                Ok(course) => scan.courses.push(course),
                Err(error) => scan.issues.push(course2md::tr!(
                    "{}：{}",
                    dir.display(),
                    format!("{error:#}")
                )),
            }
            continue;
        }
        if depth < 8 {
            match std::fs::read_dir(&dir) {
                Ok(entries) => {
                    for entry in entries {
                        match entry {
                            Ok(entry) => {
                                if entry.file_name().to_string_lossy().starts_with('.')
                                    || entry.file_name() == "versions"
                                {
                                    continue;
                                }
                                match entry.file_type() {
                                    Ok(kind) if kind.is_dir() => {
                                        queue.push_back((entry.path(), depth + 1))
                                    }
                                    Ok(_) => {}
                                    Err(error) => scan.issues.push(course2md::tr!(
                                        "{}：{}",
                                        entry.path().display(),
                                        error
                                    )),
                                }
                            }
                            Err(error) => {
                                scan.issues
                                    .push(course2md::tr!("{}：{}", dir.display(), error))
                            }
                        }
                    }
                }
                Err(error) => scan
                    .issues
                    .push(course2md::tr!("{}：{}", dir.display(), error)),
            }
        }
    }
    scan.courses.sort_by_key(|c| std::cmp::Reverse(c.modified));
    Ok(scan)
}

pub fn default_output() -> PathBuf {
    course2md::config::home_dir()
        .unwrap_or_else(|| ".".into())
        .join("Documents")
        .join("course2md")
}

#[derive(Clone)]
pub enum PreviewBlock {
    Image(PathBuf),
    Heading {
        text: SharedString,
        anchor: String,
        seconds: Option<f64>,
    },
    Paragraph {
        text: SharedString,
        anchor: String,
    },
    /// A bilingual note's translation of the paragraph before it.
    Translation {
        text: SharedString,
        anchor: String,
    },
}
impl PreviewBlock {
    /// Summary blocks are identified here once, not by anchor prefixes in the reader.
    pub fn is_summary(&self) -> bool {
        match self {
            Self::Heading { anchor, .. } => anchor == "summary",
            Self::Paragraph { anchor, .. } => {
                anchor == "summary-tldr" || anchor.starts_with("key-point-")
            }
            Self::Image(_) | Self::Translation { .. } => false,
        }
    }
    /// Section headings are the document's own frame timestamps.
    pub fn is_timestamp_heading(&self) -> bool {
        match self {
            Self::Heading {
                text,
                seconds: Some(seconds),
                ..
            } => course2md::render::fmt_ts(*seconds) == text.as_ref(),
            _ => false,
        }
    }
    pub fn section_index(&self) -> Option<usize> {
        let Self::Heading { anchor, .. } = self else {
            return None;
        };
        anchor.strip_prefix("section-")?.parse().ok()
    }
}

#[derive(Clone)]
pub enum ProcessingStage {
    Transcript,
    Screenshots,
    Proofreading,
    Translation,
    Summary,
}
impl ProcessingStage {
    pub fn label(&self) -> &'static str {
        match self {
            Self::Transcript => course2md::tr!("文字转写"),
            Self::Screenshots => course2md::tr!("截图"),
            Self::Proofreading => course2md::tr!("AI 校对"),
            Self::Translation => course2md::tr!("AI 翻译"),
            Self::Summary => course2md::tr!("摘要"),
        }
    }
}
#[derive(Clone)]
pub struct ProcessingIssue {
    pub stage: ProcessingStage,
    pub outcome: artifact::Outcome,
}
impl ProcessingIssue {
    /// The recorded cause as published (engine messages already use the UI language).
    pub fn detail(&self) -> Option<&str> {
        self.outcome
            .message
            .as_deref()
            .map(str::trim)
            .filter(|detail| !detail.is_empty())
    }
}
fn processing_issues(manifest: &artifact::Manifest) -> Vec<ProcessingIssue> {
    [
        (ProcessingStage::Transcript, &manifest.outcomes.transcript),
        (ProcessingStage::Screenshots, &manifest.outcomes.screenshots),
        (
            ProcessingStage::Proofreading,
            &manifest.outcomes.proofreading,
        ),
        (ProcessingStage::Translation, &manifest.outcomes.translation),
        (ProcessingStage::Summary, &manifest.outcomes.summary),
    ]
    .into_iter()
    .filter(|(_, outcome)| {
        matches!(
            outcome.status,
            artifact::Status::Failed | artifact::Status::Partial
        )
    })
    .map(|(stage, outcome)| ProcessingIssue {
        stage,
        outcome: outcome.clone(),
    })
    .collect()
}
#[derive(Clone)]
pub struct PageReview {
    /// Heading block in this exact preview; never a PDF page index.
    pub block: usize,
    pub seconds: f64,
}

#[derive(Clone)]
pub struct Preview {
    pub course: Course,
    pub blocks: Vec<PreviewBlock>,
    pub outputs: Vec<String>,
    pub plain_text: String,
    pub markdown_text: String,
    pub document: artifact::Document,
    pub issues: Vec<String>,
    /// Incomplete processing is recoverable through its task, never by re-reading files.
    pub processing_issues: Vec<ProcessingIssue>,
    /// Saved PDF matches requiring comparison with the original video.
    pub page_reviews: Vec<PageReview>,
}

#[derive(serde::Deserialize)]
struct AlignmentReport {
    schema: u32,
    pages: Vec<AlignmentPage>,
}

#[derive(serde::Deserialize)]
struct AlignmentPage {
    t: f64,
    image: String,
    uncertain: bool,
}

/// Called by `read_preview` on its worker while holding the version read guard.
/// The report is optional for older notes; invalid reports cannot hide the body.
fn read_page_reviews(
    dir: &Path,
    document: &artifact::Document,
    blocks: &[PreviewBlock],
) -> Result<Vec<PageReview>> {
    use std::io::Read;
    const MAX_REPORT_BYTES: u64 = 16 * 1024 * 1024;
    match std::fs::symlink_metadata(dir.join("alignment.json")) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        result => {
            result?;
        }
    }
    let path = artifact::safe_asset_path(dir, "alignment.json")?;
    anyhow::ensure!(
        std::fs::metadata(&path)?.is_file(),
        "invalid alignment file"
    );
    let file = std::fs::File::open(path)?;
    let mut bytes = Vec::new();
    file.take(MAX_REPORT_BYTES + 1).read_to_end(&mut bytes)?;
    anyhow::ensure!(
        bytes.len() as u64 <= MAX_REPORT_BYTES,
        "alignment file too large"
    );
    let report: AlignmentReport = serde_json::from_slice(&bytes)?;
    anyhow::ensure!(report.schema == 1, "unsupported alignment schema");
    let mut headings = std::collections::HashMap::new();
    for (block, value) in blocks.iter().enumerate() {
        if let PreviewBlock::Heading { anchor, .. } = value
            && let Some(section) = anchor
                .strip_prefix("section-")
                .and_then(|value| value.parse::<usize>().ok())
            && let Some(section) = document.sections.get(section)
        {
            headings
                .entry((section.t.to_bits(), section.image.as_str()))
                .or_insert_with(Vec::new)
                .push(block);
        }
    }
    let mut reviews = std::collections::BTreeMap::new();
    for page in report.pages {
        anyhow::ensure!(page.t.is_finite() && page.t >= 0., "invalid alignment time");
        let matches = headings.get(&(page.t.to_bits(), page.image.as_str()));
        let Some([block]) = matches.map(Vec::as_slice) else {
            anyhow::bail!("alignment does not match note sections");
        };
        if page.uncertain {
            reviews.insert(
                *block,
                PageReview {
                    block: *block,
                    seconds: page.t,
                },
            );
        }
    }
    Ok(reviews.into_values().collect())
}

pub fn read_preview(mut course: Course) -> Result<Preview> {
    let _read_guard = course2md::note_edit::read_guard(&course.dir)?;
    // A scan's manifest can predate an edit or crash recovery. Always open the
    // current snapshot instead of pairing cached hashes with a different body.
    let manifest =
        artifact::read_manifest(&course.dir.join("manifest.json")).context(body_unreadable())?;
    let document: artifact::Document = serde_json::from_slice(&std::fs::read(
        artifact::safe_asset_path(&course.dir, &manifest.document)?,
    )?)?;
    anyhow::ensure!(
        document.schema == 1 && artifact::has_readable_body(&document.sections),
        body_unreadable()
    );
    let markdown = artifact::safe_asset_path(&course.dir, &manifest.markdown)
        .and_then(|path| Ok(std::fs::read_to_string(path)?))
        .unwrap_or_else(|_| course2md::render::render_markdown(&document.meta, &document.sections));
    let mut plain_text = format!("{}\n\n", manifest.title);
    let mut blocks = Vec::new();
    if let Some(summary) = &document.summary {
        blocks.push(PreviewBlock::Heading {
            text: course2md::tr!("摘要").into(),
            anchor: "summary".into(),
            seconds: None,
        });
        blocks.push(PreviewBlock::Paragraph {
            text: summary.tldr.clone().into(),
            anchor: "summary-tldr".into(),
        });
        plain_text.push_str(&summary.tldr);
        plain_text.push_str("\n\n");
        for (i, point) in summary.key_points.iter().enumerate() {
            blocks.push(PreviewBlock::Paragraph {
                text: format!("• {point}").into(),
                anchor: format!("key-point-{i}"),
            });
            plain_text.push_str(point);
            plain_text.push('\n');
        }
    }
    let mut issues = course.warning.clone().into_iter().collect::<Vec<_>>();
    if artifact::validate_version(&course.dir, &manifest).is_err() {
        issues.push(course2md::tr!("部分笔记文件缺失或已改动，先显示能读到的内容。").into());
    }
    for frame in &manifest.frames {
        if artifact::safe_asset_path(&course.dir, &frame.image).is_err() {
            issues.push(course2md::tr!(
                "{} 的截图无法读取",
                course2md::render::fmt_ts(frame.t)
            ));
        }
    }
    for (section_index, section) in document.sections.iter().enumerate() {
        blocks.push(PreviewBlock::Heading {
            text: course2md::render::fmt_ts(section.t).into(),
            anchor: format!("section-{section_index}"),
            seconds: Some(section.t),
        });
        if !section.image.is_empty()
            && manifest.frames.iter().any(|f| f.image == section.image)
            && let Ok(path) = artifact::safe_asset_path(&course.dir, &section.image)
        {
            blocks.push(PreviewBlock::Image(path));
        }
        for (paragraph_index, paragraph) in section.speech.iter().enumerate() {
            if !paragraph.text.trim().is_empty() {
                blocks.push(PreviewBlock::Paragraph {
                    text: paragraph.text.clone().into(),
                    anchor: format!("paragraph-{section_index}-{paragraph_index}"),
                });
                plain_text.push_str(&paragraph.text);
                plain_text.push_str("\n\n");
                if let Some(translation) = paragraph
                    .translation
                    .as_ref()
                    .filter(|text| !text.trim().is_empty())
                {
                    blocks.push(PreviewBlock::Translation {
                        text: translation.clone().into(),
                        anchor: format!("translation-{section_index}-{paragraph_index}"),
                    });
                    plain_text.push_str(translation);
                    plain_text.push_str("\n\n");
                }
            }
        }
    }
    let page_reviews = read_page_reviews(&course.dir, &document, &blocks).unwrap_or_else(|_| {
        issues.push(course2md::tr!("PDF 页面的核对信息暂时无法读取，笔记正文仍可阅读。").into());
        Vec::new()
    });
    let processing_issues = processing_issues(&manifest);
    course.title = manifest.title.clone();
    course.slides = manifest.frames.len();
    let outputs = manifest.outputs.clone();
    course.manifest = Some(manifest);
    Ok(Preview {
        course,
        blocks,
        outputs,
        plain_text,
        markdown_text: without_image_references(&markdown),
        document,
        issues,
        processing_issues,
        page_reviews,
    })
}

/// Copy-friendly markdown: strips image spans and their reference definitions while
/// leaving authored prose, inline code, lists, tables and links untouched.
pub fn without_image_references(markdown: &str) -> String {
    use pulldown_cmark::{Event, Tag};
    let mut ranges = Vec::new();
    let mut definitions = Vec::new();
    for (event, range) in
        pulldown_cmark::Parser::new_ext(markdown, pulldown_cmark::Options::all()).into_offset_iter()
    {
        if let Event::Start(Tag::Image { id, .. }) = event {
            ranges.push(range);
            if !id.is_empty() {
                definitions.push(id.into_string());
            }
        }
    }
    let mut result = markdown.to_owned();
    for range in ranges.into_iter().rev() {
        result.replace_range(range, "");
    }
    result
        .lines()
        .filter(|line| {
            !definitions
                .iter()
                .any(|id| line.trim_start().starts_with(&format!("[{id}]:")))
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Published notes for tests: one frame and one paragraph per time.
#[cfg(test)]
pub(crate) mod fixtures {
    use super::Course;
    use course2md::{
        artifact,
        fetch::VideoMeta,
        timeline::{Section, TranscriptEvent},
    };
    use std::path::Path;

    pub(crate) fn published_note(
        root: &Path,
        revision: u64,
        times: &[f64],
        outcomes: artifact::Outcomes,
    ) -> Course {
        published_note_translated(root, revision, times, outcomes, None)
    }

    /// A published note whose paragraphs carry `translation`.
    pub(crate) fn published_note_translated(
        root: &Path,
        revision: u64,
        times: &[f64],
        outcomes: artifact::Outcomes,
        translation: Option<&str>,
    ) -> Course {
        let work = root.join(format!("work-{revision}"));
        std::fs::create_dir_all(work.join("frames")).unwrap();
        let sections = times
            .iter()
            .enumerate()
            .map(|(index, time)| {
                let relative = format!("frames/frame-{index}.png");
                image::RgbImage::new(4 + revision as u32, 3)
                    .save(work.join(&relative))
                    .unwrap();
                Section {
                    t: *time,
                    end: *time + 10.,
                    image: relative,
                    speech: vec![TranscriptEvent {
                        start: *time,
                        end: *time + 10.,
                        text: format!("第 {revision} 版，{time} 秒的正文"),
                        raw: None,
                        translation: translation.map(str::to_owned),
                    }],
                }
            })
            .collect::<Vec<_>>();
        let target = artifact::Target {
            task_id: format!("task-{revision}"),
            course_id: "fixture-course".into(),
            source_id: "fixture-source".into(),
            version_id: format!("v{revision}"),
            course_dir: root.join("course"),
        };
        let meta = VideoMeta {
            title: "测试课程".into(),
            uploader: "作者".into(),
            duration: 90.,
            webpage_url: "https://www.bilibili.com/video/BVfixture?p=2".into(),
            extractor: "bilibili".into(),
            id: "fixture".into(),
        };
        let manifest =
            artifact::publish_blocking(&target, &work, &meta, &sections, None, &[], outcomes)
                .unwrap();
        Course {
            dir: target.version_dir(),
            title: meta.title,
            modified: std::time::SystemTime::now(),
            slides: times.len(),
            thumbnail: None,
            manifest: Some(manifest),
            warning: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{fixtures::published_note, *};

    fn saved_alignment(preview: &Preview, uncertain: &[usize]) -> serde_json::Value {
        serde_json::json!({
            "schema": 1,
            "slides": ["private-input.pdf"],
            "threshold": 0.35,
            "pages": preview.document.sections.iter().enumerate().map(|(index, section)| {
                serde_json::json!({
                    "t": section.t, "image": section.image, "uncertain": uncertain.contains(&index),
                    // PDF page numbers are deliberately unrelated to paragraph indices.
                    "page": 50 + index, "deck": 1, "page_number": 51 + index,
                    "visual": 0.1, "text": 0.1, "keywords": [],
                })
            }).collect::<Vec<_>>(),
        })
    }

    #[test]
    fn pdf_review_is_optional_and_only_marks_the_uncertain_sections() {
        let root = tempfile::tempdir().unwrap();
        let course = published_note(root.path(), 1, &[10., 20., 30.], Default::default());
        let initial = read_preview(course.clone()).unwrap();
        assert!(initial.page_reviews.is_empty());
        assert!(initial.issues.is_empty());
        let report_path = course.dir.join("alignment.json");
        std::fs::write(&report_path, saved_alignment(&initial, &[]).to_string()).unwrap();
        let confident = read_preview(course.clone()).unwrap();
        assert!(confident.page_reviews.is_empty());
        assert!(confident.issues.is_empty());

        let mut report = saved_alignment(&initial, &[1, 2]);
        report["pages"].as_array_mut().unwrap().reverse();
        // Duplicate reports for the same match must not duplicate the warning.
        let duplicate = report["pages"][0].clone();
        report["pages"].as_array_mut().unwrap().push(duplicate);
        std::fs::write(&report_path, report.to_string()).unwrap();
        let preview = read_preview(course).unwrap();
        assert!(preview.issues.is_empty());
        assert_eq!(preview.page_reviews.len(), 2);
        for (review, section) in preview.page_reviews.iter().zip([1, 2]) {
            assert_eq!(review.seconds, preview.document.sections[section].t);
            assert!(
                matches!(&preview.blocks[review.block], PreviewBlock::Heading { anchor, seconds, .. }
                if anchor == &format!("section-{section}") && *seconds == Some(review.seconds))
            );
        }
        assert_eq!(preview.plain_text, initial.plain_text);
        assert_eq!(preview.markdown_text, initial.markdown_text);
    }

    #[test]
    fn repeated_pdf_page_images_use_their_own_video_time() {
        let root = tempfile::tempdir().unwrap();
        let course = published_note(root.path(), 1, &[10., 20.], Default::default());
        let mut preview = read_preview(course).unwrap();
        preview.document.sections[1].image = preview.document.sections[0].image.clone();
        std::fs::write(
            preview.course.dir.join("alignment.json"),
            saved_alignment(&preview, &[1]).to_string(),
        )
        .unwrap();
        let _guard = course2md::note_edit::read_guard(&preview.course.dir).unwrap();
        let reviews =
            read_page_reviews(&preview.course.dir, &preview.document, &preview.blocks).unwrap();
        assert_eq!(reviews.len(), 1);
        assert_eq!(reviews[0].seconds, 20.);
        assert!(
            matches!(&preview.blocks[reviews[0].block], PreviewBlock::Heading { anchor, .. } if anchor == "section-1")
        );
    }

    #[test]
    fn malformed_or_stale_pdf_review_information_keeps_the_note_readable() {
        let root = tempfile::tempdir().unwrap();
        let course = published_note(root.path(), 1, &[10.], Default::default());
        let initial = read_preview(course.clone()).unwrap();
        let mut stale = saved_alignment(&initial, &[0]);
        stale["pages"][0]["t"] = 11.into();
        let mut invalid_time = saved_alignment(&initial, &[0]);
        invalid_time["pages"][0]["t"] = (-1).into();
        let mut wrong_image = saved_alignment(&initial, &[0]);
        wrong_image["pages"][0]["image"] = "../other-note/image.png".into();
        for contents in [
            "{".to_owned(),
            r#"{"schema":2,"pages":[]}"#.into(),
            r#"{"schema":1}"#.into(),
            stale.to_string(),
            invalid_time.to_string(),
            wrong_image.to_string(),
        ] {
            std::fs::write(course.dir.join("alignment.json"), contents).unwrap();
            let preview = read_preview(course.clone()).unwrap();
            assert!(preview.page_reviews.is_empty());
            assert_eq!(preview.plain_text, initial.plain_text);
            assert_eq!(
                preview.issues,
                [course2md::tr!(
                    "PDF 页面的核对信息暂时无法读取，笔记正文仍可阅读。"
                )]
            );
        }
        let file = std::fs::File::create(course.dir.join("alignment.json")).unwrap();
        file.set_len(16 * 1024 * 1024 + 1).unwrap();
        let preview = read_preview(course).unwrap();
        assert_eq!(preview.plain_text, initial.plain_text);
        assert_eq!(preview.issues.len(), 1);
    }

    #[cfg(unix)]
    #[test]
    fn pdf_review_file_cannot_escape_the_note_directory() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let course = published_note(root.path(), 1, &[10.], Default::default());
        let initial = read_preview(course.clone()).unwrap();
        let report = outside.path().join("alignment.json");
        std::fs::write(&report, saved_alignment(&initial, &[0]).to_string()).unwrap();
        std::os::unix::fs::symlink(report, course.dir.join("alignment.json")).unwrap();
        let preview = read_preview(course).unwrap();
        assert!(preview.page_reviews.is_empty());
        assert_eq!(preview.plain_text, initial.plain_text);
        assert_eq!(preview.issues.len(), 1);
    }

    #[test]
    fn translations_follow_their_paragraphs_in_the_reader() {
        let root = tempfile::tempdir().unwrap();
        let course = super::fixtures::published_note_translated(
            root.path(),
            1,
            &[10.],
            Default::default(),
            Some("Body of version 1"),
        );
        let preview = read_preview(course).unwrap();
        let position = preview
            .blocks
            .iter()
            .position(|block| matches!(block, PreviewBlock::Paragraph { .. }))
            .unwrap();
        assert!(matches!(
            &preview.blocks[position + 1],
            PreviewBlock::Translation { text, .. } if text.as_ref() == "Body of version 1"
        ));
        assert!(preview.plain_text.contains("Body of version 1"));
        assert!(preview.markdown_text.contains("> Body of version 1"));
    }

    #[test]
    fn optional_export_failure_does_not_describe_readable_content_as_incomplete() {
        let root = tempfile::tempdir().unwrap();
        published_note(root.path(), 1, &[10.], Default::default());
        let mut course = scan_library(root.path()).unwrap().courses.remove(0);
        let manifest = course.manifest.as_mut().unwrap();
        manifest.partial = true;
        manifest.outcomes.exports.insert(
            "html".into(),
            artifact::Outcome::failed("destination unavailable"),
        );
        assert!(!course.incomplete());
        course.manifest.as_mut().unwrap().outcomes.summary =
            artifact::Outcome::failed("service unavailable");
        assert!(course.incomplete());
    }

    #[test]
    fn library_discovers_published_notes_and_ignores_task_leftovers() {
        let root = tempfile::tempdir().unwrap();
        let note = published_note(root.path(), 1, &[10.], Default::default());
        let leftover = root.path().join("failed");
        std::fs::create_dir_all(&leftover).unwrap();
        std::fs::write(leftover.join("run.json"), r#"{"success":false}"#).unwrap();
        let scan = scan_library(root.path()).unwrap();
        assert_eq!(scan.courses.len(), 1);
        assert!(scan.issues.is_empty());
        assert_eq!(scan.courses[0].dir, note.dir);
        assert_eq!(
            scan.courses[0].storage_dir().canonicalize().unwrap(),
            root.path().join("course").canonicalize().unwrap()
        );
        let preview = read_preview(scan.courses[0].clone()).unwrap();
        assert!(preview.plain_text.contains("10 秒的正文"));
        assert!(preview.issues.is_empty());
        assert!(
            preview
                .blocks
                .iter()
                .any(|b| matches!(b,PreviewBlock::Heading{seconds:Some(t),..} if *t==10.))
        );
    }

    #[test]
    fn a_damaged_latest_version_falls_back_to_the_last_intact_one() {
        let root = tempfile::tempdir().unwrap();
        let old = published_note(root.path(), 1, &[10.], Default::default());
        let new = published_note(root.path(), 2, &[20.], Default::default());
        std::fs::remove_file(new.dir.join("document.json")).unwrap();
        let course = scan_library(root.path()).unwrap().courses.remove(0);
        assert_eq!(course.dir, old.dir);
        assert!(course.warning.is_some());
        // A completed task's course reads its manifest on open.
        let opened = read_preview(Course::from_completed(&Done {
            out_dir: old.dir.clone(),
            ..Default::default()
        }))
        .unwrap();
        assert_eq!(opened.course.storage_dir(), old.storage_dir());
        assert!(
            read_preview(Course::from_completed(&Done {
                out_dir: root.path().join("missing"),
                ..Default::default()
            }))
            .is_err()
        );
    }

    #[test]
    fn opening_reports_changed_files_the_listing_did_not_hash() {
        let root = tempfile::tempdir().unwrap();
        let note = published_note(root.path(), 1, &[10.], Default::default());
        let frame = note.dir.join("frames/frame-0.png");
        let mut bytes = std::fs::read(&frame).unwrap();
        let last = bytes.len() - 1;
        bytes[last] ^= 1;
        std::fs::write(&frame, bytes).unwrap();
        let course = scan_library(root.path()).unwrap().courses.remove(0);
        assert!(course.warning.is_none());
        assert_eq!(read_preview(course).unwrap().issues.len(), 1);
    }

    #[test]
    fn processing_details_show_the_published_message() {
        let issue = ProcessingIssue {
            stage: ProcessingStage::Summary,
            outcome: artifact::Outcome::failed("摘要尚未完成，已保留正文"),
        };
        assert_eq!(issue.detail(), Some("摘要尚未完成，已保留正文"));
    }

    #[test]
    fn copy_markdown_removes_real_images_but_keeps_inline_code_and_surrounding_words() {
        let value = "before ![inline](frames/a.png) after\n\n![ref][pic]\n\n[pic]: frames/a.png\n\n`![example](literal)`";
        let copied = without_image_references(value);
        assert!(copied.contains("before  after"));
        assert!(!copied.contains("frames/a.png"));
        assert!(copied.contains("`![example](literal)`"));
    }
}
