//! Exports that retain their contents when moved away from the course library.

use crate::{
    artifact::{self, Document},
    config::OutputFormat,
    dispatch::TaskControl,
    render,
};
use anyhow::{Context, Result};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, HashSet},
    fs::File,
    io::{self, BufReader, BufWriter, Read, Write},
    path::{Path, PathBuf},
    sync::{Mutex, OnceLock},
    time::SystemTime,
};

pub fn file_name(format: OutputFormat) -> &'static str {
    match format {
        OutputFormat::Md => "course.zip",
        OutputFormat::Html => "course.html",
        OutputFormat::Json => "structured.json",
    }
}

/// Exact destination, with no overwrite. UI may explicitly confirm replacing a separate
/// exported file, but must never mutate the immutable note version itself.
pub fn export(version_dir: &Path, format: OutputFormat, destination: &Path) -> Result<PathBuf> {
    export_in_task(
        version_dir,
        format,
        destination,
        &crate::dispatch::TaskControl::default(),
    )
}

/// [`export`] that stops at the task's safe boundaries.
pub(crate) fn export_in_task(
    version_dir: &Path,
    format: OutputFormat,
    destination: &Path,
    control: &crate::dispatch::TaskControl,
) -> Result<PathBuf> {
    control.check()?;
    // Keep the same snapshot protected through validation and streaming output.
    let _guard = crate::note_edit::read_guard(version_dir)?;
    let manifest = artifact::read_manifest(&version_dir.join("manifest.json"))?;
    artifact::validate_version_in_task(version_dir, &manifest, control)?;
    let document: Document = serde_json::from_reader(BufReader::new(ControlledIo::new(
        File::open(artifact::safe_asset_path(version_dir, &manifest.document)?)?,
        control,
    )))?;
    write_document_in_task(version_dir, &document, format, destination, control)
}

#[derive(serde::Serialize)]
struct PortableJson<'a> {
    schema: u32,
    title: &'a str,
    author: &'a str,
    duration: Option<f64>,
    source: PortableSource,
    summary: Option<&'a crate::summarize::Summary>,
    sections: Vec<PortableSection<'a>>,
    images: Vec<PortableImage<'a>>,
}

#[derive(serde::Serialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
enum PortableSource {
    Local { file_name: String },
    Online { url: String },
    Unknown,
}

#[derive(serde::Serialize)]
struct PortableSection<'a> {
    t: f64,
    end: f64,
    image_id: Option<&'a str>,
    speech: &'a [crate::timeline::TranscriptEvent],
    #[serde(skip_serializing_if = "Option::is_none")]
    needs_review: Option<bool>,
}

/// Only portable review information is retained. The source report also contains
/// local PDF paths and diagnostic scores, which must not leak into shared exports.
#[derive(serde::Deserialize, serde::Serialize)]
struct AlignmentReport {
    schema: u32,
    pages: Vec<AlignmentPage>,
}

#[derive(serde::Deserialize, serde::Serialize)]
struct AlignmentPage {
    t: f64,
    image: String,
    uncertain: bool,
}

struct PageReviews {
    alignment: AlignmentReport,
    sections: Vec<bool>,
}

fn page_reviews(
    root: &Path,
    document: &Document,
    images: &BTreeMap<String, ImageAsset>,
    control: &TaskControl,
) -> Result<Option<PageReviews>> {
    control.check()?;
    match std::fs::symlink_metadata(root.join("alignment.json")) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        result => {
            result?;
        }
    }
    let read = || -> Result<PageReviews> {
        const MAX_REPORT_BYTES: u64 = 16 * 1024 * 1024;
        control.check()?;
        let path = artifact::safe_asset_path(root, "alignment.json")?.canonicalize()?;
        anyhow::ensure!(
            std::fs::metadata(&path)?.is_file(),
            "invalid alignment file"
        );
        let file = ControlledIo::new(File::open(path)?, control);
        let mut bytes = Vec::new();
        file.take(MAX_REPORT_BYTES + 1).read_to_end(&mut bytes)?;
        anyhow::ensure!(
            bytes.len() as u64 <= MAX_REPORT_BYTES,
            "alignment file too large"
        );
        let mut alignment: AlignmentReport = serde_json::from_slice(&bytes)?;
        anyhow::ensure!(alignment.schema == 1, "unsupported alignment schema");
        let mut positions = BTreeMap::<_, Vec<_>>::new();
        for (index, section) in document.sections.iter().enumerate() {
            positions
                .entry((section.t.to_bits(), section.image.as_str()))
                .or_default()
                .push(index);
        }
        let mut sections = vec![false; document.sections.len()];
        for page in &mut alignment.pages {
            control.check()?;
            anyhow::ensure!(
                page.t.is_finite() && page.t >= 0.0,
                "invalid alignment time"
            );
            let matches = positions.get(&(page.t.to_bits(), page.image.as_str()));
            let Some([index]) = matches.map(Vec::as_slice) else {
                anyhow::bail!("alignment does not match note sections");
            };
            sections[*index] |= page.uncertain;
            if !page.image.is_empty() {
                page.image = images
                    .get(&page.image)
                    .context("alignment image missing")?
                    .path
                    .clone();
            }
        }
        Ok(PageReviews {
            alignment,
            sections,
        })
    };
    let result = read();
    // Surface pause/cancel directly, including cancellation during ControlledIo
    // reads, before adding the explanation for an unreadable report.
    control.check()?;
    result.map(Some).context(crate::tr!(
        "导出失败：PDF 页面核对信息无效或已过期，原笔记未修改。"
    ))
}

#[derive(serde::Serialize)]
struct PortableImage<'a> {
    id: &'a str,
    mime_type: &'static str,
    sha256: &'a str,
}

struct ImageAsset {
    id: String,
    path: String,
    mime: &'static str,
    sha256: String,
    fingerprint: ImageFingerprint,
}

/// Cache only content digests, never image bytes. File identity and change time
/// on Unix also detect replacements/restored mtimes at the same path and size.
#[derive(Clone, PartialEq, Eq)]
struct ImageFingerprint {
    len: u64,
    modified: SystemTime,
    #[cfg(unix)]
    identity: (u64, u64, i64, i64),
}

impl ImageFingerprint {
    fn of(file: &File) -> io::Result<Self> {
        let meta = file.metadata()?;
        #[cfg(unix)]
        use std::os::unix::fs::MetadataExt;
        Ok(Self {
            len: meta.len(),
            modified: meta.modified()?,
            #[cfg(unix)]
            identity: (meta.dev(), meta.ino(), meta.ctime(), meta.ctime_nsec()),
        })
    }
}

type DigestCache = BTreeMap<PathBuf, (ImageFingerprint, String)>;

fn digest_cache() -> &'static Mutex<DigestCache> {
    static CACHE: OnceLock<Mutex<DigestCache>> = OnceLock::new();
    CACHE.get_or_init(Default::default)
}

fn image_digest(
    path: &Path,
    file: &mut ControlledIo<'_, File>,
    header: &[u8],
    fingerprint: &ImageFingerprint,
) -> Result<String> {
    if let Some((_, digest)) = crate::runtime::lock_recover(digest_cache())
        .get(path)
        .filter(|(cached, _)| cached == fingerprint)
    {
        return Ok(digest.clone());
    }
    let mut hash = Sha256::new();
    hash.update(header);
    let mut buffer = [0; CHUNK_SIZE];
    loop {
        let n = file.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        hash.update(&buffer[..n]);
    }
    anyhow::ensure!(
        ImageFingerprint::of(&file.inner)? == *fingerprint,
        crate::tr!("文件校验不符 / Checksum mismatch")
    );
    let digest = format!("{:x}", hash.finalize());
    let mut cache = crate::runtime::lock_recover(digest_cache());
    if cache.len() >= 16_384 {
        cache.clear();
    }
    cache.insert(path.to_owned(), (fingerprint.clone(), digest.clone()));
    Ok(digest)
}

fn image_mime(bytes: &[u8]) -> Option<(&'static str, &'static str)> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some(("image/png", "png"))
    } else if bytes.starts_with(&[0xff, 0xd8, 0xff]) {
        Some(("image/jpeg", "jpg"))
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        Some(("image/gif", "gif"))
    } else if bytes.starts_with(b"RIFF") && bytes.get(8..12) == Some(b"WEBP") {
        Some(("image/webp", "webp"))
    } else {
        None
    }
}

// A bounded buffer also bounds cancellation latency for large individual assets.
const CHUNK_SIZE: usize = 64 * 1024;

struct ControlledIo<'a, T> {
    inner: T,
    control: &'a TaskControl,
    remaining: usize,
}

impl<'a, T> ControlledIo<'a, T> {
    fn new(inner: T, control: &'a TaskControl) -> Self {
        Self {
            inner,
            control,
            remaining: 0,
        }
    }

    fn check(&mut self) -> io::Result<()> {
        if self.remaining == 0 {
            self.control.check().map_err(io::Error::other)?;
            self.remaining = CHUNK_SIZE;
        }
        Ok(())
    }
}

impl<T: Read> Read for ControlledIo<'_, T> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.check()?;
        let limit = buf.len().min(self.remaining);
        let read = self.inner.read(&mut buf[..limit])?;
        self.remaining -= read;
        Ok(read)
    }
}

impl<T: Write> Write for ControlledIo<'_, T> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.check()?;
        let written = self.inner.write(&buf[..buf.len().min(self.remaining)])?;
        self.remaining -= written;
        Ok(written)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.control.check().map_err(io::Error::other)?;
        self.inner.flush()
    }
}

fn open_image(root: &Path, relative: &str) -> Result<(PathBuf, File)> {
    // Reject Windows separators and drive/stream names even on Unix. Export
    // destinations are content-addressed, never derived from these names.
    anyhow::ensure!(
        !relative.contains(['\\', ':']),
        crate::tr!("笔记包含无效资源路径 / Invalid note asset path")
    );
    let path = artifact::safe_asset_path(root, relative)?.canonicalize()?;
    anyhow::ensure!(
        path.starts_with(root.canonicalize()?),
        crate::tr!("资源位置超出此笔记 / Asset escapes note directory")
    );
    let file = File::open(&path)?;
    anyhow::ensure!(
        file.metadata()?.is_file(),
        crate::tr!("笔记包含无效资源路径 / Invalid note asset path")
    );
    Ok((path, file))
}

fn copy_image(
    root: &Path,
    original: &str,
    asset: &ImageAsset,
    out: &mut impl Write,
    control: &TaskControl,
) -> Result<()> {
    let (_, file) = open_image(root, original)?;
    anyhow::ensure!(
        ImageFingerprint::of(&file)? == asset.fingerprint,
        crate::tr!("文件校验不符 / Checksum mismatch")
    );
    let mut input = ControlledIo::new(file, control);
    io::copy(&mut input, out)?;
    anyhow::ensure!(
        ImageFingerprint::of(&input.inner)? == asset.fingerprint,
        crate::tr!("文件校验不符 / Checksum mismatch")
    );
    Ok(())
}

fn images(
    root: &Path,
    document: &Document,
    control: &TaskControl,
) -> Result<BTreeMap<String, ImageAsset>> {
    let mut images = BTreeMap::new();
    for section in document.sections.iter().filter(|s| !s.image.is_empty()) {
        control.check()?;
        if images.contains_key(&section.image) {
            continue;
        }
        let (path, file) = open_image(root, &section.image)?;
        let fingerprint = ImageFingerprint::of(&file)?;
        let mut file = ControlledIo::new(file, control);
        let mut header = Vec::with_capacity(12);
        Read::by_ref(&mut file).take(12).read_to_end(&mut header)?;
        let (mime, ext) = image_mime(&header).context(crate::tr!("截图不是可识别的图片，原文件已保留 / Screenshot is not a recognized image; original file kept"))?;
        let sha256 = image_digest(&path, &mut file, &header, &fingerprint)?;
        // Full hashes avoid truncated-ID collisions and also deduplicate aliases.
        let id = format!("image-{sha256}");
        images.insert(
            section.image.clone(),
            ImageAsset {
                path: format!("images/{id}.{ext}"),
                id,
                mime,
                sha256,
                fingerprint,
            },
        );
    }
    Ok(images)
}

/// Export an in-memory document to exact, non-existing destinations. Image
/// metadata/hashes are collected once for the whole batch; image contents are
/// streamed separately into each output. No manifest validation is performed.
///
/// Each file is published atomically, but the batch is not a transaction: on
/// failure earlier files may exist. Callers updating a note should use a staging
/// directory and commit its outputs only after this function succeeds.
pub fn export_document(
    root: &Path,
    document: &Document,
    destinations: &[(OutputFormat, PathBuf)],
    control: &TaskControl,
) -> Result<Vec<PathBuf>> {
    control.check()?;
    let mut seen = HashSet::new();
    for (_, destination) in destinations {
        ensure_destination(destination)?;
        anyhow::ensure!(
            seen.insert(destination),
            crate::tr!("导出位置已有不同文件，未覆盖 / Existing export differs; not overwritten")
        );
    }
    if destinations.is_empty() {
        return Ok(Vec::new());
    }
    let images = images(root, document, control)?;
    let reviews = page_reviews(root, document, &images, control)?;
    destinations
        .iter()
        .map(|(format, destination)| {
            write_indexed_document(
                root,
                document,
                *format,
                destination,
                control,
                &images,
                reviews.as_ref(),
            )
        })
        .collect()
}

#[cfg(test)]
fn write_document(
    root: &Path,
    document: &Document,
    format: OutputFormat,
    destination: &Path,
) -> Result<PathBuf> {
    write_document_in_task(root, document, format, destination, &TaskControl::default())
}

pub(crate) fn write_document_in_task(
    root: &Path,
    document: &Document,
    format: OutputFormat,
    destination: &Path,
    control: &TaskControl,
) -> Result<PathBuf> {
    export_document(root, document, &[(format, destination.to_owned())], control)?;
    Ok(destination.to_owned())
}

fn ensure_destination(destination: &Path) -> Result<()> {
    // symlink_metadata catches dangling symlinks as well as existing files.
    match std::fs::symlink_metadata(destination) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
        Ok(_) => anyhow::bail!(
            "{}: {}",
            crate::tr!("导出位置已有不同文件，未覆盖 / Existing export differs; not overwritten"),
            destination.display()
        ),
    }
}

fn write_indexed_document(
    root: &Path,
    document: &Document,
    format: OutputFormat,
    destination: &Path,
    control: &TaskControl,
    images: &BTreeMap<String, ImageAsset>,
    reviews: Option<&PageReviews>,
) -> Result<PathBuf> {
    control.check()?;
    ensure_destination(destination)?;
    let parent = destination
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    std::fs::create_dir_all(parent)?;
    let mut file = tempfile::NamedTempFile::new_in(parent)?;
    crate::checkpoint::make_shared(file.as_file())?;
    // Clone metadata only, not the entire transcript/summary.
    let mut meta = document.meta.clone();
    let local_file = if meta.extractor == "local" {
        let name = local_file_name(&meta.webpage_url);
        meta.webpage_url.clear();
        name
    } else {
        None
    };
    match format {
        OutputFormat::Md => {
            let mut zip = zip::ZipWriter::new(file.as_file_mut());
            let options = zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Stored)
                .large_file(true);
            zip.start_file("course.md", options)?;
            {
                let mut out = ControlledIo::new(BufWriter::new(&mut zip), control);
                render::write_markdown(
                    &meta,
                    &document.sections,
                    document.summary.as_ref(),
                    &mut out,
                    |out, original| {
                        out.write_all(images[original].path.as_bytes())?;
                        Ok(())
                    },
                )?;
                if let Some(name) = &local_file {
                    write!(
                        out,
                        "{}",
                        crate::tr!("\n源文件：{}\n", name.replace(['\r', '\n'], " "))
                    )?;
                }
                out.flush()?;
            }
            let mut written = HashSet::new();
            for (original, image) in images {
                control.check()?;
                if written.insert(&image.path) {
                    zip.start_file(&image.path, options)?;
                    copy_image(root, original, image, &mut zip, control)?;
                }
            }
            if let Some(reviews) = reviews {
                zip.start_file("alignment.json", options)?;
                let mut out = ControlledIo::new(BufWriter::new(&mut zip), control);
                serde_json::to_writer_pretty(&mut out, &reviews.alignment)?;
                out.flush()?;
            }
            control.check()?;
            zip.finish()?;
        }
        OutputFormat::Html => {
            let mut out = ControlledIo::new(BufWriter::new(file.as_file_mut()), control);
            render::write_html_with_reviews(
                &meta,
                &document.sections,
                document.summary.as_ref(),
                &mut out,
                |out, original| {
                    let image = &images[original];
                    write!(out, "data:{};base64,", image.mime)?;
                    // EncoderWriter may return WriteZero after a short delegate
                    // write. Buffer its small encoded chunks so each is accepted
                    // in full, even across ControlledIo's cancellation boundaries.
                    let mut encoder = base64::write::EncoderWriter::new(
                        BufWriter::new(out),
                        &base64::engine::general_purpose::STANDARD,
                    );
                    copy_image(root, original, image, &mut encoder, control)?;
                    encoder.finish()?.flush()?;
                    Ok(())
                },
                reviews.map_or(&[], |reviews| reviews.sections.as_slice()),
            )?;
            out.flush()?;
        }
        OutputFormat::Json => {
            let source = if document.meta.extractor == "local" {
                local_file.map_or(PortableSource::Unknown, |file_name| PortableSource::Local {
                    file_name,
                })
            } else if document.meta.webpage_url.is_empty() {
                PortableSource::Unknown
            } else {
                PortableSource::Online {
                    url: document.meta.webpage_url.clone(),
                }
            };
            let mut seen = HashSet::new();
            let export = PortableJson {
                schema: 1,
                title: &document.meta.title,
                author: &document.meta.uploader,
                duration: (document.meta.duration > 0.).then_some(document.meta.duration),
                source,
                summary: document.summary.as_ref(),
                sections: document
                    .sections
                    .iter()
                    .enumerate()
                    .map(|(index, section)| PortableSection {
                        t: section.t,
                        end: section.end,
                        image_id: images.get(&section.image).map(|image| image.id.as_str()),
                        speech: &section.speech,
                        needs_review: reviews
                            .and_then(|reviews| reviews.sections[index].then_some(true)),
                    })
                    .collect(),
                images: images
                    .values()
                    .filter(|image| seen.insert(&image.id))
                    .map(|image| PortableImage {
                        id: &image.id,
                        mime_type: image.mime,
                        sha256: &image.sha256,
                    })
                    .collect(),
            };
            let mut out = ControlledIo::new(BufWriter::new(file.as_file_mut()), control);
            serde_json::to_writer_pretty(&mut out, &export)?;
            out.flush()?;
        }
    }
    control.check()?;
    file.as_file().sync_all()?;
    control.check()?;
    file.persist_noclobber(destination)
        .with_context(|| destination.display().to_string())?;
    artifact::sync_dir(parent)?;
    Ok(destination.to_path_buf())
}

/// The file name of a local source path. The note may have been made on
/// another system, so both separators split it, whatever this platform's
/// path rules are.
fn local_file_name(path: &str) -> Option<String> {
    path.rsplit(['/', '\\'])
        .next()
        .filter(|name| !name.is_empty() && *name != "." && *name != "..")
        .map(str::to_owned)
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::{
        dispatch::{Control, ControlSource, Intent},
        fetch::VideoMeta,
        summarize::{OutlineItem, Summary},
        timeline::{Section, TranscriptEvent},
    };
    use base64::Engine as _;

    fn fixture(root: &Path) -> Document {
        std::fs::create_dir_all(root.join("slides")).unwrap();
        std::fs::create_dir_all(root.join("frames")).unwrap();
        image::RgbImage::from_pixel(2, 2, image::Rgb([20, 80, 180]))
            .save(root.join("frames/a.png"))
            .unwrap();
        // MIME comes from the file signature, not the slide's .jpg extension.
        std::fs::copy(root.join("frames/a.png"), root.join("slides/d1-0038.jpg")).unwrap();
        std::fs::copy(root.join("frames/a.png"), root.join("slides/d2-0007.jpg")).unwrap();
        Document {
            schema: 1,
            meta: VideoMeta {
                title: "Lecture <title>".into(),
                uploader: "An author".into(),
                duration: 30.,
                webpage_url: r"C:\private\course\lecture.mp4".into(),
                extractor: "local".into(),
                id: "source".into(),
            },
            sections: ["slides/d1-0038.jpg", "slides/d2-0007.jpg", "frames/a.png"]
                .iter()
                .enumerate()
                .map(|(i, image)| Section {
                    t: i as f64 * 10.,
                    end: (i + 1) as f64 * 10.,
                    image: (*image).into(),
                    speech: vec![TranscriptEvent {
                        start: i as f64 * 10.,
                        end: (i + 1) as f64 * 10.,
                        text: "Actual note $x^2$; keep ](slides/d1-0038.jpg)".into(),
                        translation: Some("Translation <with> & symbols".into()),
                        raw: None,
                    }],
                })
                .collect(),
            summary: Some(Summary {
                tldr: "Complete overview".into(),
                key_points: vec!["First point".into(), "Last point".into()],
                outline: (0..100)
                    .map(|i| OutlineItem {
                        t: i as f64,
                        title: format!("Topic {i}"),
                        detail: format!("Complete detail {i}"),
                    })
                    .collect(),
            }),
        }
    }

    fn task(dir: &Path) -> (TaskControl, PathBuf) {
        let path = dir.join("control.json");
        let control = TaskControl::open(
            dir,
            ControlSource {
                control_path: Some(path.clone()),
                ..Default::default()
            },
        )
        .unwrap();
        (control, path)
    }

    fn cancel(path: &Path) {
        crate::checkpoint::atomic_write(
            path,
            &serde_json::to_vec(&Control {
                intent: Intent::Cancel,
                ..Default::default()
            })
            .unwrap(),
        )
        .unwrap();
    }

    fn alignment_report(document: &Document) -> serde_json::Value {
        serde_json::json!({
            "schema": 1,
            "slides": ["/private/teacher/deck.pdf"],
            "pages": document.sections.iter().enumerate().map(|(index, section)| {
                serde_json::json!({
                    "t": section.t, "image": section.image, "uncertain": index == 1,
                    "visual": 0.41, "keywords": ["private diagnostic"]
                })
            }).collect::<Vec<_>>()
        })
    }

    #[test]
    fn review_exports_match_time_and_image_without_leaking_local_paths() {
        let root = tempfile::tempdir().unwrap();
        let mut document = fixture(root.path());
        document.sections[1].image = document.sections[0].image.clone();
        let original = serde_json::to_vec(&document).unwrap();
        let baseline = root.path().join("baseline.zip");
        write_document(root.path(), &document, OutputFormat::Md, &baseline).unwrap();
        let mut baseline_zip = zip::ZipArchive::new(File::open(baseline).unwrap()).unwrap();
        let mut baseline_md = String::new();
        baseline_zip
            .by_name("course.md")
            .unwrap()
            .read_to_string(&mut baseline_md)
            .unwrap();
        let report = serde_json::to_vec(&alignment_report(&document)).unwrap();
        std::fs::write(root.path().join("alignment.json"), &report).unwrap();
        let destinations: Vec<_> = [OutputFormat::Md, OutputFormat::Html, OutputFormat::Json]
            .into_iter()
            .map(|format| (format, root.path().join("out").join(file_name(format))))
            .collect();
        export_document(
            root.path(),
            &document,
            &destinations,
            &TaskControl::default(),
        )
        .unwrap();
        let mut zip = zip::ZipArchive::new(File::open(&destinations[0].1).unwrap()).unwrap();
        let mut markdown = String::new();
        zip.by_name("course.md")
            .unwrap()
            .read_to_string(&mut markdown)
            .unwrap();
        assert_eq!(markdown, baseline_md);
        let mut exported = String::new();
        zip.by_name("alignment.json")
            .unwrap()
            .read_to_string(&mut exported)
            .unwrap();
        for private in ["private", "slides", "visual", "keywords", "deck.pdf"] {
            assert!(!exported.contains(private), "leaked {private}");
        }
        let alignment: serde_json::Value = serde_json::from_str(&exported).unwrap();
        assert_eq!(alignment["schema"], 1);
        for (index, page) in alignment["pages"].as_array().unwrap().iter().enumerate() {
            assert_eq!(page["t"], document.sections[index].t);
            assert_eq!(page["uncertain"], index == 1);
            let image = page["image"].as_str().unwrap();
            assert!(image.starts_with("images/image-"));
            assert!(zip.by_name(image).is_ok());
        }
        let html = std::fs::read_to_string(&destinations[1].1).unwrap();
        assert_eq!(html.matches("class=\"page-review\"").count(), 1);
        let marker = html.find("class=\"page-review\"").unwrap();
        assert!(marker > html.find("<section id=\"t10\"").unwrap());
        assert!(marker < html.find("<section id=\"t20\"").unwrap());
        let json_text = std::fs::read_to_string(&destinations[2].1).unwrap();
        for text in [&html, &json_text, &markdown] {
            assert!(!text.contains("private"));
            assert!(!text.contains("deck.pdf"));
        }
        let json: serde_json::Value = serde_json::from_str(&json_text).unwrap();
        assert_eq!(json["schema"], 1);
        assert!(json["sections"][0].get("needs_review").is_none());
        assert_eq!(json["sections"][1]["needs_review"], true);
        assert!(json["sections"][2].get("needs_review").is_none());
        assert_eq!(
            std::fs::read(root.path().join("alignment.json")).unwrap(),
            report
        );
        assert_eq!(serde_json::to_vec(&document).unwrap(), original);
        // Note editing changes speech but keeps section coordinates and reads
        // the report from the version root, independently of the export stage.
        document.sections[1].speech[0].text = "Edited note".into();
        let edited = root.path().join("stage/edited.json");
        write_document(root.path(), &document, OutputFormat::Json, &edited).unwrap();
        let edited: serde_json::Value =
            serde_json::from_slice(&std::fs::read(edited).unwrap()).unwrap();
        assert_eq!(edited["sections"][1]["needs_review"], true);
        assert_eq!(edited["sections"][1]["speech"][0]["text"], "Edited note");
    }

    #[test]
    fn malformed_or_stale_review_reports_do_not_publish_exports() {
        let root = tempfile::tempdir().unwrap();
        let document = fixture(root.path());
        let mut stale = alignment_report(&document);
        stale["pages"][1]["t"] = 10.5.into();
        let mut invalid_time = alignment_report(&document);
        invalid_time["pages"][1]["t"] = (-1).into();
        let mut wrong_image = alignment_report(&document);
        wrong_image["pages"][1]["image"] = "../other.png".into();
        let mut missing_status = alignment_report(&document);
        missing_status["pages"][1]
            .as_object_mut()
            .unwrap()
            .remove("uncertain");
        for report in [
            "broken".into(),
            r#"{"schema":2,"pages":[]}"#.into(),
            stale.to_string(),
            invalid_time.to_string(),
            wrong_image.to_string(),
            missing_status.to_string(),
        ] {
            std::fs::write(root.path().join("alignment.json"), report).unwrap();
            for format in [OutputFormat::Md, OutputFormat::Html, OutputFormat::Json] {
                let destination = root.path().join(file_name(format));
                assert!(write_document(root.path(), &document, format, &destination).is_err());
                assert!(!destination.exists());
                std::fs::write(&destination, b"previous export").unwrap();
                assert!(write_document(root.path(), &document, format, &destination).is_err());
                assert_eq!(std::fs::read(&destination).unwrap(), b"previous export");
                std::fs::remove_file(destination).unwrap();
            }
        }
        File::create(root.path().join("alignment.json"))
            .unwrap()
            .set_len(16 * 1024 * 1024 + 1)
            .unwrap();
        assert!(
            write_document(
                root.path(),
                &document,
                OutputFormat::Html,
                &root.path().join("large.html")
            )
            .is_err()
        );
    }

    #[test]
    fn review_report_cancellation_retains_control_cause() {
        let root = tempfile::tempdir().unwrap();
        let document = fixture(root.path());
        std::fs::write(root.path().join("alignment.json"), "broken").unwrap();
        let images = images(root.path(), &document, &TaskControl::default()).unwrap();
        let (control, path) = task(root.path());
        cancel(&path);
        let error = page_reviews(root.path(), &document, &images, &control)
            .err()
            .unwrap();
        assert!(
            error
                .chain()
                .any(|cause| cause.downcast_ref::<crate::dispatch::Failure>().is_some())
        );
        assert_eq!(error.to_string(), control.check().unwrap_err().to_string());
    }

    #[test]
    #[cfg(unix)]
    fn review_report_cannot_escape_version_directory() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let document = fixture(root.path());
        let report = outside.path().join("alignment.json");
        std::fs::write(&report, alignment_report(&document).to_string()).unwrap();
        std::os::unix::fs::symlink(report, root.path().join("alignment.json")).unwrap();
        let destination = root.path().join("course.html");
        assert!(write_document(root.path(), &document, OutputFormat::Html, &destination).is_err());
        assert!(!destination.exists());
    }

    #[test]
    fn batch_preserves_summary_page_labels_translation_and_deduplicates_assets() {
        let root = tempfile::tempdir().unwrap();
        let document = fixture(root.path());
        let destinations: Vec<_> = [OutputFormat::Md, OutputFormat::Html, OutputFormat::Json]
            .into_iter()
            .map(|format| (format, root.path().join("out").join(file_name(format))))
            .collect();
        let exported = export_document(
            root.path(),
            &document,
            &destinations,
            &TaskControl::default(),
        )
        .unwrap();
        assert_eq!(
            exported,
            destinations
                .iter()
                .map(|(_, path)| path.clone())
                .collect::<Vec<_>>()
        );
        let bytes = std::fs::read(root.path().join("frames/a.png")).unwrap();
        let hash = crate::execution::digest(&bytes);
        let image_path = format!("images/image-{hash}.png");
        let mut zip = zip::ZipArchive::new(File::open(&exported[0]).unwrap()).unwrap();
        assert_eq!(zip.len(), 2, "identical bytes have just one ZIP entry");
        let mut markdown = String::new();
        zip.by_name("course.md")
            .unwrap()
            .read_to_string(&mut markdown)
            .unwrap();
        let mut zipped_image = Vec::new();
        zip.by_name(&image_path)
            .unwrap()
            .read_to_end(&mut zipped_image)
            .unwrap();
        assert_eq!(zipped_image, bytes);
        assert_eq!(markdown.matches(&format!("]({image_path})")).count(), 3);
        assert!(markdown.contains(&crate::tr!("第 {} 页", 38)));
        assert!(markdown.contains(&crate::tr!("第 {} 套 · 第 {} 页", 2, 7)));
        assert!(markdown.contains("> Translation <with> & symbols"));
        assert!(
            markdown.contains("keep ](slides/d1-0038.jpg)"),
            "prose must not be rewritten as an image target"
        );
        let summary = document.summary.as_ref().unwrap();
        assert!(markdown.contains(&crate::summarize::render_md_block(summary)));
        assert!(
            markdown.find("Complete overview").unwrap() < markdown.find("Actual note").unwrap()
        );
        assert!(!markdown.contains("private"));

        let html = std::fs::read_to_string(&exported[1]).unwrap();
        let mut meta = document.meta.clone();
        meta.webpage_url.clear();
        let mut expected = render::render_html(&meta, &document.sections);
        let data_uri = format!(
            "data:image/png;base64,{}",
            base64::engine::general_purpose::STANDARD.encode(&bytes)
        );
        for section in &document.sections {
            expected = expected.replace(
                &format!("src=\"{}\"", render::esc(&section.image)),
                &format!("src=\"{data_uri}\""),
            );
        }
        expected = expected.replacen(
            "</header>\n",
            &format!(
                "</header>\n{}\n",
                crate::summarize::render_html_block(summary)
            ),
            1,
        );
        assert_eq!(
            html, expected,
            "portable HTML retains the shared renderer's layout and complete summary"
        );
        assert_eq!(html.matches(&data_uri).count(), 3);
        assert!(html.contains("<svg"), "formula layout is retained");
        assert!(!html.contains("private"));
        let json: serde_json::Value =
            serde_json::from_reader(File::open(&exported[2]).unwrap()).unwrap();
        assert_eq!(json["summary"], serde_json::to_value(summary).unwrap());
        assert_eq!(json["images"].as_array().unwrap().len(), 1);
        assert_eq!(json["images"][0]["sha256"], hash);
        assert_eq!(json["source"]["file_name"], "lecture.mp4");
        assert_eq!(
            json["sections"][0]["speech"][0]["translation"],
            "Translation <with> & symbols"
        );
        for section in json["sections"].as_array().unwrap() {
            assert_eq!(section["image_id"], json["images"][0]["id"]);
        }
    }

    #[test]
    fn empty_batch_never_reads_assets_or_creates_directories() {
        let root = tempfile::tempdir().unwrap();
        let mut document = fixture(root.path());
        document.sections[0].image = "../missing.png".into();
        let missing = root.path().join("does-not-exist");
        assert!(
            export_document(&missing, &document, &[], &TaskControl::default())
                .unwrap()
                .is_empty()
        );
        assert!(!missing.exists());
    }

    #[test]
    fn published_export_respects_editor_lock_without_a_recovery_journal() {
        let root = tempfile::tempdir().unwrap();
        let document = fixture(root.path());
        std::fs::write(
            root.path().join("document.json"),
            serde_json::to_vec(&document).unwrap(),
        )
        .unwrap();
        let assets: Vec<_> = std::iter::once("document.json")
            .chain(document.sections.iter().map(|s| s.image.as_str()))
            .map(|relative| {
                let path = root.path().join(relative);
                artifact::Asset {
                    path: relative.into(),
                    bytes: std::fs::metadata(&path).unwrap().len(),
                    sha256: crate::execution::file_digest(&path).unwrap(),
                }
            })
            .collect();
        std::fs::write(
            root.path().join("manifest.json"),
            serde_json::to_vec(&serde_json::json!({
                "schema": 1, "task_id": "task", "course_id": "course",
                "source_id": "source", "version_id": "version", "title": "Lecture",
                "created_at_ms": 0, "revision": 1, "document": "document.json",
                "markdown": "course.md", "frames": [], "assets": assets,
                "outputs": [], "outcomes": {}, "partial": false,
            }))
            .unwrap(),
        )
        .unwrap();
        let lock_path = root.path().join(".ai-edit.lock");
        let editor = crate::runtime::lock_file(&lock_path).unwrap();
        let out = root.path().join("published.html");
        assert!(!root.path().join(".ai-edit-transaction").exists());
        assert!(export(root.path(), OutputFormat::Html, &out).is_err());
        assert!(
            !out.exists(),
            "an active editor must prevent snapshot reads"
        );

        // Transactions already own the editor lock; the in-memory batch entry
        // point must remain usable without trying to acquire a second lock.
        export_document(
            root.path(),
            &document,
            &[(OutputFormat::Html, root.path().join("staged.html"))],
            &TaskControl::default(),
        )
        .unwrap();
        drop(editor);
        export(root.path(), OutputFormat::Html, &out).unwrap();
        let released = crate::runtime::lock_file(&lock_path).unwrap();
        drop(released);
        assert!(export(root.path(), OutputFormat::Html, &out).is_err());
        // An export failure also releases its read guard.
        crate::runtime::lock_file(&lock_path).unwrap();
    }

    #[test]
    fn cancellation_and_read_failure_leave_no_partial_export() {
        let root = tempfile::tempdir().unwrap();
        let document = fixture(root.path());
        let (control, path) = task(root.path());
        cancel(&path);
        let out_dir = root.path().join("out");
        std::fs::create_dir(&out_dir).unwrap();
        for format in [OutputFormat::Md, OutputFormat::Html, OutputFormat::Json] {
            assert!(
                write_document_in_task(
                    root.path(),
                    &document,
                    format,
                    &out_dir.join(file_name(format)),
                    &control
                )
                .is_err()
            );
        }
        let images = images(root.path(), &document, &TaskControl::default()).unwrap();
        // Failure after indexing exercises temporary-file cleanup during writing.
        std::fs::remove_file(root.path().join("slides/d1-0038.jpg")).unwrap();
        std::fs::remove_file(root.path().join("frames/a.png")).unwrap();
        for format in [OutputFormat::Md, OutputFormat::Html] {
            assert!(
                write_indexed_document(
                    root.path(),
                    &document,
                    format,
                    &out_dir.join(file_name(format)),
                    &TaskControl::default(),
                    &images,
                    None
                )
                .is_err()
            );
        }
        assert_eq!(std::fs::read_dir(out_dir).unwrap().count(), 0);
    }

    #[test]
    fn large_single_writes_and_image_reads_observe_midstream_cancellation() {
        struct CancelAfterWrite {
            path: PathBuf,
            written: usize,
        }
        impl Write for CancelAfterWrite {
            fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
                self.written += buf.len();
                cancel(&self.path);
                Ok(buf.len())
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let root = tempfile::tempdir().unwrap();
        let (control, path) = task(root.path());
        let mut writer = ControlledIo::new(CancelAfterWrite { path, written: 0 }, &control);
        assert!(writer.write_all(&vec![b'x'; CHUNK_SIZE * 3]).is_err());
        assert_eq!(writer.inner.written, CHUNK_SIZE);
        let root = tempfile::tempdir().unwrap();
        let (control, path) = task(root.path());
        let mut source =
            ControlledIo::new(io::repeat(b'x').take((CHUNK_SIZE * 3) as u64), &control);
        let mut sink = CancelAfterWrite { path, written: 0 };
        assert!(io::copy(&mut source, &mut sink).is_err());
        assert!(sink.written > 0 && sink.written <= CHUNK_SIZE);
    }

    #[test]
    fn portable_assets_reject_traversal_and_cross_platform_paths() {
        let root = tempfile::tempdir().unwrap();
        let mut document = fixture(root.path());
        document.sections.truncate(1);
        for invalid in [
            "../outside.png",
            "/tmp/outside.png",
            r"..\outside.png",
            "C:/outside.png",
            "frames/a.png:stream",
        ] {
            document.sections[0].image = invalid.into();
            let out = root.path().join("out.html");
            assert!(
                write_document(root.path(), &document, OutputFormat::Html, &out).is_err(),
                "accepted {invalid}"
            );
            assert!(!out.exists());
        }
    }

    #[cfg(unix)]
    #[test]
    fn symlink_escape_and_dangling_destination_are_rejected() {
        use std::os::unix::fs::symlink;
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let mut document = fixture(root.path());
        std::fs::copy(
            root.path().join("frames/a.png"),
            outside.path().join("a.png"),
        )
        .unwrap();
        symlink(outside.path().join("a.png"), root.path().join("escape.png")).unwrap();
        document.sections[0].image = "escape.png".into();
        let out = root.path().join("out.html");
        assert!(write_document(root.path(), &document, OutputFormat::Html, &out).is_err());
        assert!(!out.exists());
        symlink(outside.path().join("missing.html"), &out).unwrap();
        assert!(write_document(root.path(), &document, OutputFormat::Html, &out).is_err());
        assert!(
            std::fs::symlink_metadata(out)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert!(!outside.path().join("missing.html").exists());
    }

    #[test]
    fn unchanged_images_reuse_hashes_and_modified_images_invalidate_them() {
        use std::io::Seek;
        let root = tempfile::tempdir().unwrap();
        let mut document = fixture(root.path());
        document.sections.truncate(1);
        let control = TaskControl::default();
        let original = &document.sections[0].image;
        let first = images(root.path(), &document, &control).unwrap();
        let (path, file) = open_image(root.path(), original).unwrap();
        let fingerprint = ImageFingerprint::of(&file).unwrap();
        let mut reader = ControlledIo::new(file, &control);
        let mut header = [0; 12];
        reader.read_exact(&mut header).unwrap();
        assert_eq!(
            image_digest(&path, &mut reader, &header, &fingerprint).unwrap(),
            first[original].sha256
        );
        assert_eq!(
            reader.inner.stream_position().unwrap(),
            12,
            "a cache hit must not reread the image body"
        );
        drop(reader);

        let mut bytes = std::fs::read(&path).unwrap();
        let last = bytes.len() - 1;
        bytes[last] ^= 1;
        std::fs::write(&path, &bytes).unwrap();
        File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_times(
                std::fs::FileTimes::new()
                    .set_modified(fingerprint.modified + std::time::Duration::from_secs(2)),
            )
            .unwrap();
        // Already-indexed outputs must reject changed assets rather than pair
        // new bytes with an old content-addressed name.
        let out = root.path().join("out.html");
        assert!(
            write_indexed_document(
                root.path(),
                &document,
                OutputFormat::Html,
                &out,
                &control,
                &first,
                None
            )
            .is_err()
        );
        assert!(!out.exists());
        let second = images(root.path(), &document, &control).unwrap();
        assert_ne!(first[original].sha256, second[original].sha256);
        assert_eq!(second[original].sha256, crate::execution::digest(&bytes));
    }

    #[test]
    fn html_embeds_images_with_attribute_special_characters_in_their_names() {
        let root = tempfile::tempdir().unwrap();
        let mut document = fixture(root.path());
        document.sections.truncate(1);
        // Quotes aren't valid in Windows filenames, but ampersands are and
        // still used to break raw-string HTML image replacement.
        let name = if cfg!(windows) {
            "frames/a&b.png"
        } else {
            "frames/a&\"b.png"
        };
        std::fs::copy(root.path().join("frames/a.png"), root.path().join(name)).unwrap();
        document.sections[0].image = name.into();
        let out = root.path().join("special.html");
        write_document(root.path(), &document, OutputFormat::Html, &out).unwrap();
        let html = std::fs::read_to_string(out).unwrap();
        assert!(html.contains("src=\"data:image/png;base64,"));
        assert!(!html.contains(&format!("src=\"{}\"", render::esc(name))));
    }

    #[test]
    fn long_note_streams_large_images_and_all_sections() {
        let root = tempfile::tempdir().unwrap();
        let mut document = fixture(root.path());
        document.summary = None;
        document.sections.truncate(1);
        document.sections[0].speech[0].text = "Long transcript".repeat(50);
        let mut bytes = b"\x89PNG\r\n\x1a\n".to_vec();
        bytes.resize(CHUNK_SIZE * 3 + 1, 42);
        let original = document.sections[0].clone();
        document.sections = (0..128)
            .map(|i| {
                let mut section = original.clone();
                section.t = i as f64;
                section.end = section.t + 1.;
                section.image = format!("frames/{i}.png");
                std::fs::write(root.path().join(&section.image), &bytes).unwrap();
                section
            })
            .collect();
        let path = root.path().join("long.html");
        write_document(root.path(), &document, OutputFormat::Html, &path).unwrap();
        // Read linewise so the regression itself does not retain the 32 MiB output.
        use std::io::BufRead;
        let mut count = 0;
        let expected = base64::engine::general_purpose::STANDARD.encode(&bytes);
        for line in BufReader::new(File::open(path).unwrap()).lines() {
            let line = line.unwrap();
            if let Some(image) = line.split_once("data:image/png;base64,") {
                assert_eq!(image.1.split('"').next().unwrap(), expected);
                count += 1;
            }
        }
        assert_eq!(count, document.sections.len());
    }

    #[test]
    fn local_file_names_come_from_either_platform() {
        assert_eq!(
            local_file_name(r"C:\课程\a b.mp4").as_deref(),
            Some("a b.mp4")
        );
        assert_eq!(local_file_name("/Users/me/a.mp4").as_deref(), Some("a.mp4"));
        assert_eq!(local_file_name("dir/"), None);
    }
    #[test]
    fn all_exports_survive_without_the_original_library_and_never_overwrite() {
        use crate::{
            fetch::VideoMeta,
            timeline::{Section, TranscriptEvent},
        };
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("source");
        std::fs::create_dir_all(source.join("frames")).unwrap();
        image::RgbImage::from_pixel(2, 2, image::Rgb([20, 80, 180]))
            .save(source.join("frames/a.png"))
            .unwrap();
        let document = Document {
            schema: 1,
            meta: VideoMeta {
                title: "title".into(),
                uploader: String::new(),
                duration: 0.,
                webpage_url: "/private/machine/movie.mp4".into(),
                extractor: "local".into(),
                id: "source".into(),
            },
            sections: vec![Section {
                t: 0.,
                end: 1.,
                image: "frames/a.png".into(),
                speech: vec![TranscriptEvent {
                    start: 0.,
                    end: 1.,
                    text: "Actual note".into(),
                    raw: None,
                    translation: None,
                }],
            }],
            summary: None,
        };
        let destination = root.path().join("moved");
        for format in [OutputFormat::Md, OutputFormat::Html, OutputFormat::Json] {
            write_document(
                &source,
                &document,
                format,
                &destination.join(file_name(format)),
            )
            .unwrap();
        }
        std::fs::remove_dir_all(source).unwrap();
        let mut zip =
            zip::ZipArchive::new(std::fs::File::open(destination.join("course.zip")).unwrap())
                .unwrap();
        assert_eq!(zip.len(), 2);
        assert!(zip.by_name("course.md").is_ok());
        let html = std::fs::read_to_string(destination.join("course.html")).unwrap();
        assert!(html.contains("data:image/png;base64,"));
        assert!(!html.contains("src=\"images/"));
        assert!(!html.contains("时长 00:00"));
        let json = std::fs::read_to_string(destination.join("structured.json")).unwrap();
        assert!(json.contains("image_id"));
        assert!(!json.contains("/private/machine"));
        assert!(!json.contains("frames/a.png"));
        let existing = destination.join("keep.html");
        std::fs::write(&existing, "user content").unwrap();
        assert!(write_document(root.path(), &document, OutputFormat::Html, &existing).is_err());
        assert_eq!(std::fs::read_to_string(existing).unwrap(), "user content");
    }
}
