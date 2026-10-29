//! In-place AI edits use one version lock and a recoverable rollback journal.
//! All replacements are prepared before touching the published files. The legacy
//! `document.json.before-ai` remains the one-level undo format.

use anyhow::{Context, Result};
use std::{
    io::ErrorKind,
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

const BACKUP: &str = "document.json.before-ai";
const JOURNAL: &str = ".ai-edit-transaction";
const LOCK: &str = ".ai-edit.lock";

pub struct EditReport {
    pub attempted: usize,
    pub succeeded: usize,
    pub failed: usize,
    pub note: Option<String>,
}

fn parse_document(bytes: &[u8]) -> Result<crate::artifact::Document> {
    let document: crate::artifact::Document =
        serde_json::from_slice(bytes).context(crate::tr!("笔记正文无法读取，原文件已保留"))?;
    anyhow::ensure!(
        document.schema == 1,
        crate::tr!("此笔记版本需要其他版本的软件 / Unsupported manifest schema")
    );
    Ok(document)
}

#[cfg(test)]
fn load_document(version_dir: &Path) -> Result<crate::artifact::Document> {
    parse_document(&std::fs::read(version_dir.join("document.json"))?)
}

fn write_json(path: &Path, value: &impl serde::Serialize) -> Result<()> {
    crate::checkpoint::atomic_write_shared(path, &serde_json::to_vec_pretty(value)?)
}

// Reject directories and links, including an exports directory that points outside
// the version. NotFound is the only error that means an optional file is absent.
fn regular_file(path: &Path) -> Result<bool> {
    match std::fs::symlink_metadata(path) {
        Ok(meta) => {
            anyhow::ensure!(
                meta.file_type().is_file(),
                crate::tr!("目标出现非预期文件，原文件已保留：{}", path.display())
            );
            Ok(true)
        }
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error.into()),
    }
}

fn export_directory(version_dir: &Path) -> Result<bool> {
    let path = version_dir.join("exports");
    match std::fs::symlink_metadata(&path) {
        Ok(meta) => {
            anyhow::ensure!(
                meta.file_type().is_dir(),
                crate::tr!("目标出现非预期文件，原文件已保留：{}", path.display())
            );
            Ok(true)
        }
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error.into()),
    }
}

fn copy_atomic(from: &Path, to: &Path) -> Result<()> {
    let parent = to.parent().unwrap();
    let mut file = tempfile::NamedTempFile::new_in(parent)?;
    crate::checkpoint::make_shared(file.as_file())?;
    std::io::copy(&mut std::fs::File::open(from)?, &mut file)?;
    file.as_file().sync_all()?;
    file.persist(to)?;
    crate::artifact::sync_dir(parent)
}

#[derive(serde::Serialize, serde::Deserialize)]
struct Journal {
    schema: u32,
    committed: bool,
    entries: Vec<JournalEntry>,
}

#[derive(serde::Serialize, serde::Deserialize)]
struct JournalEntry {
    path: String,
    // A missing digest records that the file did not exist before this operation.
    original_digest: Option<String>,
    remove: bool,
}

fn editable_path(path: &str) -> bool {
    matches!(
        path,
        "document.json"
            | "course.md"
            | "manifest.json"
            | BACKUP
            | "exports/course.zip"
            | "exports/course.html"
            | "exports/structured.json"
    )
}

// Rename the entire journal out of the recovery namespace before deleting it.
// Interrupted cleanup can never turn a complete journal into an incomplete one.
fn retire_journal(version_dir: &Path) -> Result<()> {
    let garbage = tempfile::Builder::new()
        .prefix(".ai-edit-gc-")
        .tempdir_in(version_dir)?;
    std::fs::rename(version_dir.join(JOURNAL), garbage.path().join("retired"))?;
    crate::artifact::sync_dir(version_dir)?;
    Ok(())
}

fn recover(version_dir: &Path) -> Result<()> {
    let dir = version_dir.join(JOURNAL);
    match std::fs::symlink_metadata(&dir) {
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
        Ok(meta) => anyhow::ensure!(
            meta.file_type().is_dir(),
            crate::tr!("笔记编辑恢复记录无效，原文件已保留")
        ),
    }
    let journal: Journal = serde_json::from_slice(&std::fs::read(dir.join("journal.json"))?)?;
    anyhow::ensure!(
        journal.schema == 1 && !journal.entries.is_empty(),
        crate::tr!("笔记编辑恢复记录无效，原文件已保留")
    );
    let mut unique = std::collections::HashSet::new();
    for entry in &journal.entries {
        anyhow::ensure!(
            editable_path(&entry.path) && unique.insert(&entry.path),
            crate::tr!("笔记编辑恢复记录无效，原文件已保留")
        );
    }
    if !journal.committed {
        // Check every backup before restoring even the first file. Keep originals
        // in the journal throughout rollback so a second interruption is retryable.
        for (index, entry) in journal.entries.iter().enumerate() {
            if let Some(digest) = &entry.original_digest {
                let original = dir.join(format!("old-{index}"));
                anyhow::ensure!(
                    regular_file(&original)?
                        && crate::execution::file_digest(&original)? == *digest,
                    crate::tr!("笔记编辑恢复记录无效，原文件已保留")
                );
            }
            if entry.path.starts_with("exports/") {
                anyhow::ensure!(
                    export_directory(version_dir)?,
                    crate::tr!("笔记编辑恢复记录无效，原文件已保留")
                );
            }
            regular_file(&version_dir.join(&entry.path))?;
        }
        for (index, entry) in journal.entries.iter().enumerate() {
            let target = version_dir.join(&entry.path);
            if entry.original_digest.is_some() {
                copy_atomic(&dir.join(format!("old-{index}")), &target)?;
            } else if regular_file(&target)? {
                std::fs::remove_file(&target)?;
                crate::artifact::sync_dir(target.parent().unwrap())?;
            }
        }
    }
    retire_journal(version_dir)
}

/// Recover an interrupted edit before a reader validates the manifest. Normal
/// read-only notes incur only an existence check and never acquire/create a lock.
/// If an active editor owns the journal, this returns the existing lock error so
/// readers do not validate a partially published version.
pub fn recover_pending(version_dir: &Path) -> Result<()> {
    match std::fs::symlink_metadata(version_dir.join(JOURNAL)) {
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
        Ok(_) => {}
    }
    let _lock = crate::runtime::lock_file(&version_dir.join(LOCK))?;
    recover(version_dir)
}

/// Keeps a readable version stable against in-place editors until dropped.
/// Legacy notes on read-only storage may have no lock file. Only that case,
/// with no pending journal, falls back to reading without creating a lock.
pub struct ReadGuard {
    file: Option<std::fs::File>,
}

impl Drop for ReadGuard {
    fn drop(&mut self) {
        if let Some(file) = &self.file {
            let _ = file.unlock();
        }
    }
}

fn journal_pending(version_dir: &Path) -> Result<bool> {
    match std::fs::symlink_metadata(version_dir.join(JOURNAL)) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error.into()),
    }
}

fn open_reader_lock(
    version_dir: &Path,
    create: impl FnOnce() -> std::io::Result<std::fs::File>,
) -> Result<Option<std::fs::File>> {
    match create() {
        Ok(file) => Ok(Some(file)),
        Err(error)
            if matches!(
                error.kind(),
                ErrorKind::PermissionDenied | ErrorKind::ReadOnlyFilesystem
            ) =>
        {
            // An existing lock can be shared even when the file or volume is
            // read-only. Never mistake contention/other I/O failures for this case.
            match std::fs::File::open(version_dir.join(LOCK)) {
                Ok(file) => Ok(Some(file)),
                Err(open_error) if open_error.kind() == ErrorKind::NotFound => {
                    anyhow::ensure!(
                        version_dir.is_dir() && !journal_pending(version_dir)?,
                        crate::tr!("笔记编辑尚未恢复，请重试；恢复副本已保留")
                    );
                    Ok(None)
                }
                Err(open_error) => Err(open_error.into()),
            }
        }
        Err(error) => Err(error.into()),
    }
}

/// Acquire before reading manifest/document/assets and hold through validation or
/// export. Recovery runs under an exclusive lock; the subsequent shared lock is
/// checked again for a journal so a writer cannot slip a partial commit between
/// recovery and reading. Writers and readers report contention instead of blocking.
pub fn read_guard(version_dir: &Path) -> Result<ReadGuard> {
    let path = version_dir.join(LOCK);
    let guard = ReadGuard {
        file: open_reader_lock(version_dir, || {
            std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .open(&path)
        })?,
    };
    if let Some(file) = &guard.file {
        loop {
            file.try_lock_shared().with_context(|| {
                crate::tr!("目录正被另一任务使用：{}。请等待任务结束", path.display())
            })?;
            if !journal_pending(version_dir)? {
                break;
            }
            file.unlock()?;
            file.try_lock().with_context(|| {
                crate::tr!("目录正被另一任务使用：{}。请等待任务结束", path.display())
            })?;
            recover(version_dir)?;
            file.unlock()?;
        }
    }
    Ok(guard)
}

fn begin(version_dir: &Path) -> Result<crate::runtime::FileLock> {
    let lock = crate::runtime::lock_file(&version_dir.join(LOCK))?;
    recover(version_dir)?;
    Ok(lock)
}

/// Capture this before sending an AI request. Checked edits compare it under the
/// same OS lock used for recovery and publication, including legacy versions.
pub fn document_revision(version_dir: &Path) -> Result<String> {
    let _lock = read_guard(version_dir)?;
    crate::execution::file_digest(&version_dir.join("document.json"))
}

/// Capture the on-disk identity only if it still matches the document displayed
/// in the reader. Capturing a fresh hash alone could authorize a stale AI prompt.
pub fn document_revision_for(
    version_dir: &Path,
    expected: &crate::artifact::Document,
) -> Result<String> {
    let _lock = read_guard(version_dir)?;
    let bytes = std::fs::read(version_dir.join("document.json"))?;
    let document = parse_document(&bytes)?;
    anyhow::ensure!(
        serde_json::to_value(&document)? == serde_json::to_value(expected)?,
        crate::tr!("笔记已发生变化，请重新选择文字后再试")
    );
    Ok(crate::execution::digest(&bytes))
}

fn check_cancelled(cancelled: &AtomicBool) -> Result<()> {
    anyhow::ensure!(
        !cancelled.load(Ordering::Acquire),
        crate::tr!("已取消 AI 编辑")
    );
    Ok(())
}

fn check_revision(original: &[u8], expected: Option<&str>) -> Result<()> {
    anyhow::ensure!(
        expected.is_none_or(|expected| crate::execution::digest(original) == expected),
        crate::tr!("笔记已发生变化，请重新选择文字后再试")
    );
    Ok(())
}

fn render_markdown(document: &crate::artifact::Document) -> String {
    let mut markdown = crate::render::render_markdown(&document.meta, &document.sections);
    if let Some(summary) = &document.summary {
        markdown = crate::summarize::insert_into_md(&markdown, summary);
    }
    markdown
}

// Only changed files are hashed. In particular, images/timeline/meta are never
// copied or rehashed here. Unknown manifest fields are retained for older clients.
fn stage_manifest(version_dir: &Path, stage: &Path, changed: &[String]) -> Result<bool> {
    let path = version_dir.join("manifest.json");
    if !regular_file(&path)? {
        return Ok(false);
    }
    let mut value: serde_json::Value = serde_json::from_slice(&std::fs::read(&path)?)?;
    let manifest: crate::artifact::Manifest = serde_json::from_value(value.clone())?;
    anyhow::ensure!(
        manifest.schema == 1
            && manifest.document == "document.json"
            && manifest.markdown == "course.md",
        crate::tr!("此笔记版本需要其他版本的软件 / Unsupported manifest schema")
    );
    value["revision"] = manifest.revision.saturating_add(1).into();
    let assets = value["assets"]
        .as_array_mut()
        .context(crate::tr!("笔记版本信息已损坏"))?;
    for name in changed.iter().filter(|name| name.as_str() != BACKUP) {
        let full = stage.join(name);
        let bytes = std::fs::metadata(&full)?.len();
        let digest = crate::execution::file_digest(&full)?;
        let mut found = false;
        for asset in assets
            .iter_mut()
            .filter(|asset| asset["path"].as_str() == Some(name))
        {
            asset["bytes"] = bytes.into();
            asset["sha256"] = digest.clone().into();
            found = true;
        }
        if !found {
            assets.push(serde_json::json!({"path": name, "bytes": bytes, "sha256": digest}));
        }
    }
    write_json(&stage.join("manifest.json"), &value)?;
    Ok(true)
}

fn prepare(
    version_dir: &Path,
    document_bytes: &[u8],
    document: &crate::artifact::Document,
    previous: Option<&[u8]>,
    control: &crate::dispatch::TaskControl,
) -> Result<tempfile::TempDir> {
    anyhow::ensure!(
        crate::artifact::has_readable_body(&document.sections),
        crate::tr!("笔记没有可读正文 / Note has no readable body")
    );
    control.check()?;
    let stage = tempfile::Builder::new()
        .prefix(".ai-edit-prepare-")
        .tempdir_in(version_dir)?;
    crate::checkpoint::atomic_write_shared(&stage.path().join("document.json"), document_bytes)?;
    crate::checkpoint::atomic_write_shared(
        &stage.path().join("course.md"),
        render_markdown(document).as_bytes(),
    )?;
    let mut changed = vec!["document.json".to_owned(), "course.md".to_owned()];
    let mut exports = Vec::new();
    if export_directory(version_dir)? {
        for format in [
            crate::config::OutputFormat::Md,
            crate::config::OutputFormat::Html,
            crate::config::OutputFormat::Json,
        ] {
            let name = format!("exports/{}", crate::portable::file_name(format));
            if regular_file(&version_dir.join(&name))? {
                exports.push((format, stage.path().join(&name)));
                changed.push(name);
            }
        }
    }
    if !exports.is_empty() {
        crate::portable::export_document(version_dir, document, &exports, control)?;
    }
    control.check()?;
    if stage_manifest(version_dir, stage.path(), &changed)? {
        changed.push("manifest.json".to_owned());
    }
    if let Some(bytes) = previous {
        crate::checkpoint::atomic_write_shared(&stage.path().join(BACKUP), bytes)?;
    }
    changed.push(BACKUP.to_owned());
    let mut journal = Journal {
        schema: 1,
        committed: false,
        entries: Vec::new(),
    };
    for (index, name) in changed.into_iter().enumerate() {
        control.check()?;
        let path = version_dir.join(&name);
        let original_digest = if regular_file(&path)? {
            let original = stage.path().join(format!("old-{index}"));
            copy_atomic(&path, &original)?;
            Some(crate::execution::file_digest(&original)?)
        } else {
            None
        };
        journal.entries.push(JournalEntry {
            remove: name == BACKUP && previous.is_none(),
            path: name,
            original_digest,
        });
    }
    control.check()?;
    write_json(&stage.path().join("journal.json"), &journal)?;
    crate::artifact::sync_dir(stage.path())?;
    Ok(stage)
}

fn commit(version_dir: &Path, stage: tempfile::TempDir) -> Result<()> {
    commit_with_hook(version_dir, stage, |_| Ok(()))
}

// The hook provides deterministic I/O failure coverage without process-global
// fault switches, sleeps, or permission tests that accidentally pass as root.
fn commit_with_hook(
    version_dir: &Path,
    stage: tempfile::TempDir,
    mut after_write: impl FnMut(usize) -> Result<()>,
) -> Result<()> {
    let dir = version_dir.join(JOURNAL);
    std::fs::rename(stage.path(), &dir)?;
    let result = (|| -> Result<()> {
        crate::artifact::sync_dir(version_dir)?;
        let mut journal: Journal =
            serde_json::from_slice(&std::fs::read(dir.join("journal.json"))?)?;
        for (index, entry) in journal.entries.iter().enumerate() {
            let target = version_dir.join(&entry.path);
            if entry.remove {
                std::fs::remove_file(&target)?;
                crate::artifact::sync_dir(target.parent().unwrap())?;
            } else {
                copy_atomic(&dir.join(&entry.path), &target)?;
            }
            after_write(index)?;
        }
        journal.committed = true;
        write_json(&dir.join("journal.json"), &journal)?;
        crate::artifact::sync_dir(&dir)?;
        Ok(())
    })();
    if let Err(error) = result {
        // A failed commit marker sync must still roll back in this process.
        let marker = dir.join("journal.json");
        let mut journal: Journal = serde_json::from_slice(&std::fs::read(&marker)?)?;
        if journal.committed {
            journal.committed = false;
            write_json(&marker, &journal)?;
            crate::artifact::sync_dir(&dir)?;
        }
        recover(version_dir).context(crate::tr!("笔记编辑尚未恢复，请重试；恢复副本已保留"))?;
        return Err(error);
    }
    // All data and the commit marker are durable. Cleanup failures are harmless:
    // the next locked operation sees the committed marker and finishes cleanup.
    let _ = retire_journal(version_dir);
    Ok(())
}

fn save_edit(
    version_dir: &Path,
    original: &[u8],
    document: &crate::artifact::Document,
    cancelled: &Arc<AtomicBool>,
    control: &crate::dispatch::TaskControl,
) -> Result<()> {
    check_cancelled(cancelled)?;
    let before = parse_document(original)?;
    if serde_json::to_value(&before)? == serde_json::to_value(document)? {
        return Ok(());
    }
    // Patch only speech fields touched by AI; preserve unknown fields and summary
    // in the original document instead of round-tripping them through the model.
    let mut value: serde_json::Value = serde_json::from_slice(original)?;
    for (i, section) in document.sections.iter().enumerate() {
        for (j, speech) in section.speech.iter().enumerate() {
            let old = &before.sections[i].speech[j];
            let target = &mut value["sections"][i]["speech"][j];
            if old.text != speech.text || old.translation != speech.translation {
                target
                    .as_object_mut()
                    .unwrap()
                    .remove("selection_translations");
            }
            if old.text != speech.text {
                target["text"] = speech.text.clone().into();
            }
            if old.translation != speech.translation {
                target["translation"] = serde_json::to_value(&speech.translation)?;
            }
        }
    }
    let bytes = serde_json::to_vec_pretty(&value)?;
    let stage = prepare(version_dir, &bytes, document, Some(original), control)?;
    check_cancelled(cancelled)?;
    commit(version_dir, stage)
}

pub fn translate_in_place(
    version_dir: &Path,
    settings: &crate::llm::LlmSettings,
    target: &str,
) -> Result<EditReport> {
    translate(
        version_dir,
        settings,
        target,
        None,
        &Arc::new(AtomicBool::new(false)),
    )
}

pub fn translate_in_place_checked(
    version_dir: &Path,
    settings: &crate::llm::LlmSettings,
    target: &str,
    expected_document_sha256: &str,
    cancelled: &Arc<AtomicBool>,
) -> Result<EditReport> {
    translate(
        version_dir,
        settings,
        target,
        Some(expected_document_sha256),
        cancelled,
    )
}

fn translate(
    version_dir: &Path,
    settings: &crate::llm::LlmSettings,
    target: &str,
    expected: Option<&str>,
    cancelled: &Arc<AtomicBool>,
) -> Result<EditReport> {
    let _lock = begin(version_dir)?;
    let original = std::fs::read(version_dir.join("document.json"))?;
    check_cancelled(cancelled)?;
    check_revision(&original, expected)?;
    let mut document = parse_document(&original)?;
    let control = crate::dispatch::TaskControl::with_cancel_flag(cancelled.clone());
    let report =
        crate::llm::translate_sections_report(&mut document.sections, settings, target, &control)?;
    save_edit(version_dir, &original, &document, cancelled, &control)?;
    Ok(EditReport {
        attempted: report.attempted,
        succeeded: report.succeeded,
        failed: report.failed,
        note: report.note,
    })
}

pub fn apply_region_edit(
    version_dir: &Path,
    section: usize,
    speech: usize,
    text: &str,
    translation: Option<&str>,
) -> Result<()> {
    apply_region(
        version_dir,
        section,
        speech,
        text,
        translation,
        None,
        &Arc::new(AtomicBool::new(false)),
    )
}

pub fn apply_region_edit_checked(
    version_dir: &Path,
    section: usize,
    speech: usize,
    text: &str,
    translation: Option<&str>,
    expected_document_sha256: &str,
    cancelled: &Arc<AtomicBool>,
) -> Result<()> {
    apply_region(
        version_dir,
        section,
        speech,
        text,
        translation,
        Some(expected_document_sha256),
        cancelled,
    )
}

/// A replacement of original text within one speech passage. Offsets are UTF-8
/// byte offsets (`start..end`, end exclusive) in the expected document, before
/// any of the batch's replacements are applied. Split cross-passage selections
/// into one edit per passage; never use offsets into rendered text or translation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelectionEdit {
    pub section: usize,
    pub speech: usize,
    pub start: usize,
    pub end: usize,
    pub text: String,
}

/// Atomically replace selected source ranges, preserving all unselected text.
///
/// Capture `expected_document_sha256` with `document_revision_for` against the
/// displayed document before requesting AI results. Callers must map each model
/// result to its original input range and reject missing or duplicate result IDs.
/// Edits may arrive in any order; empty, overlapping (including duplicate),
/// out-of-bounds, and non-character-boundary ranges are rejected as a whole.
/// Adjacent ranges and empty replacement strings are allowed, provided the note
/// retains readable text. All coordinates refer to the same original snapshot.
///
/// Only passages whose final source text changes lose their old translation.
/// A no-op batch preserves the previous undo and revision. A changed batch uses
/// one undo and the existing recoverable transaction for document, markdown,
/// manifest and existing exports. Cancellation is honored until publication;
/// once publication starts, the transaction completes or rolls back as a unit.
pub fn apply_selection_edits_checked(
    version_dir: &Path,
    edits: &[SelectionEdit],
    expected_document_sha256: &str,
    cancelled: &Arc<AtomicBool>,
) -> Result<()> {
    let _lock = begin(version_dir)?;
    let original = std::fs::read(version_dir.join("document.json"))?;
    check_cancelled(cancelled)?;
    check_revision(&original, Some(expected_document_sha256))?;
    let mut document = parse_document(&original)?;
    let ordered = checked_selection_edits(&document, edits, cancelled)?;

    // Work backwards within each passage so length changes never move any
    // remaining range. Clear translation only after comparing the final passage,
    // including batches whose individual replacements cancel each other out.
    let mut remaining = ordered.as_slice();
    while let Some(first) = remaining.first() {
        check_cancelled(cancelled)?;
        let count = remaining
            .partition_point(|edit| (edit.section, edit.speech) == (first.section, first.speech));
        let event = &mut document.sections[first.section].speech[first.speech];
        let mut text = event.text.clone();
        for edit in remaining[..count].iter().rev() {
            check_cancelled(cancelled)?;
            text.replace_range(edit.start..edit.end, &edit.text);
        }
        if text != event.text {
            event.text = text;
            event.translation = None;
        }
        remaining = &remaining[count..];
    }

    let control = crate::dispatch::TaskControl::with_cancel_flag(cancelled.clone());
    save_edit(version_dir, &original, &document, cancelled, &control)
}

fn checked_selection_edits<'a>(
    document: &crate::artifact::Document,
    edits: &'a [SelectionEdit],
    cancelled: &AtomicBool,
) -> Result<Vec<&'a SelectionEdit>> {
    anyhow::ensure!(!edits.is_empty(), crate::tr!("请先在原文中选择文字"));

    let mut ordered: Vec<_> = edits.iter().collect();
    ordered.sort_by_key(|edit| (edit.section, edit.speech, edit.start, edit.end));
    // Validate the entire batch against the untouched snapshot. In particular,
    // a later invalid range must never leave an earlier replacement published.
    let mut previous: Option<&SelectionEdit> = None;
    for edit in &ordered {
        check_cancelled(cancelled)?;
        let event = document
            .sections
            .get(edit.section)
            .and_then(|section| section.speech.get(edit.speech))
            .context(crate::tr!("所选原文已失效，请重新选择"))?;
        anyhow::ensure!(
            edit.start < edit.end && event.text.get(edit.start..edit.end).is_some(),
            crate::tr!("所选原文已失效，请重新选择")
        );
        if let Some(previous) = previous {
            anyhow::ensure!(
                (edit.section, edit.speech) != (previous.section, previous.speech)
                    || edit.start >= previous.end,
                crate::tr!("所选原文已失效，请重新选择")
            );
        }
        previous = Some(edit);
    }

    Ok(ordered)
}

#[derive(serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct TranslationPart {
    start: usize,
    end: usize,
    text: String,
}

#[derive(serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct SelectionTranslations {
    schema: u8,
    source_sha256: String,
    base: Option<String>,
    parts: Vec<TranslationPart>,
    rendered: String,
}

/// Translate exact selected source ranges without replacing source text.
/// `SelectionEdit.text` contains the translated text. All passages commit and
/// undo together. Full-passage results use the existing bilingual field. Partial
/// results retain any existing full translation and carry explicit source quotes.
/// The auxiliary ranges let a repeat translation replace its own previous result;
/// `translation` remains readable by existing readers and every export format.
pub fn apply_selection_translations_checked(
    version_dir: &Path,
    edits: &[SelectionEdit],
    expected_document_sha256: &str,
    cancelled: &Arc<AtomicBool>,
) -> Result<()> {
    let _lock = begin(version_dir)?;
    let original = std::fs::read(version_dir.join("document.json"))?;
    check_cancelled(cancelled)?;
    check_revision(&original, Some(expected_document_sha256))?;
    let document = parse_document(&original)?;
    let ordered = checked_selection_edits(&document, edits, cancelled)?;
    anyhow::ensure!(
        edits.iter().all(|edit| !edit.text.trim().is_empty()),
        crate::tr!("服务返回的编辑结构无效 / Invalid edit structure")
    );
    let mut value: serde_json::Value = serde_json::from_slice(&original)?;
    let before = value.clone();
    let mut remaining = ordered.as_slice();
    while let Some(first) = remaining.first() {
        check_cancelled(cancelled)?;
        let count = remaining
            .partition_point(|edit| (edit.section, edit.speech) == (first.section, first.speech));
        let event = &document.sections[first.section].speech[first.speech];
        let target = &mut value["sections"][first.section]["speech"][first.speech];
        if count == 1 && first.start == 0 && first.end == event.text.len() {
            target["translation"] = first.text.trim().into();
            target
                .as_object_mut()
                .unwrap()
                .remove("selection_translations");
        } else {
            let digest = crate::execution::digest(event.text.as_bytes());
            let mut saved = if let Some(saved) = target.get("selection_translations") {
                let saved: SelectionTranslations = serde_json::from_value(saved.clone())
                    .context(crate::tr!("笔记正文无法读取，原文件已保留"))?;
                anyhow::ensure!(
                    saved.schema == 1
                        && saved.source_sha256 == digest
                        && event.translation.as_deref() == Some(saved.rendered.as_str())
                        && saved.parts.iter().all(|part| part.start < part.end
                            && event.text.get(part.start..part.end).is_some()
                            && !part.text.trim().is_empty())
                        && saved
                            .parts
                            .windows(2)
                            .all(|pair| pair[0].end <= pair[1].start),
                    crate::tr!("笔记正文无法读取，原文件已保留")
                );
                saved
            } else {
                SelectionTranslations {
                    schema: 1,
                    source_sha256: digest,
                    base: event.translation.clone(),
                    parts: Vec::new(),
                    rendered: String::new(),
                }
            };
            for edit in &remaining[..count] {
                saved
                    .parts
                    .retain(|part| part.end <= edit.start || part.start >= edit.end);
                saved.parts.push(TranslationPart {
                    start: edit.start,
                    end: edit.end,
                    text: edit.text.trim().to_owned(),
                });
            }
            saved.parts.sort_by_key(|part| part.start);
            let mut rendered: Vec<String> = saved.base.iter().cloned().collect();
            for part in &saved.parts {
                rendered.push(format!(
                    "{}\n{}",
                    crate::tr!("选区译文（原文：{}）", &event.text[part.start..part.end]),
                    part.text
                ));
            }
            saved.rendered = rendered.join("\n\n");
            target["translation"] = saved.rendered.clone().into();
            target["selection_translations"] = serde_json::to_value(saved)?;
        }
        remaining = &remaining[count..];
    }
    if value == before {
        return Ok(());
    }
    let bytes = serde_json::to_vec_pretty(&value)?;
    let document = parse_document(&bytes)?;
    let control = crate::dispatch::TaskControl::with_cancel_flag(cancelled.clone());
    let stage = prepare(version_dir, &bytes, &document, Some(&original), &control)?;
    check_cancelled(cancelled)?;
    commit(version_dir, stage)
}

fn apply_region(
    version_dir: &Path,
    section: usize,
    speech: usize,
    text: &str,
    translation: Option<&str>,
    expected: Option<&str>,
    cancelled: &Arc<AtomicBool>,
) -> Result<()> {
    let _lock = begin(version_dir)?;
    let original = std::fs::read(version_dir.join("document.json"))?;
    check_cancelled(cancelled)?;
    check_revision(&original, expected)?;
    let mut document = parse_document(&original)?;
    let event = document
        .sections
        .get_mut(section)
        .and_then(|section| section.speech.get_mut(speech))
        .with_context(|| {
            format!(
                "{}: {section}/{speech}",
                crate::tr!("笔记里找不到这段文字 / Note passage not found")
            )
        })?;
    event.text = text.to_owned();
    event.translation = translation
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned);
    let control = crate::dispatch::TaskControl::with_cancel_flag(cancelled.clone());
    save_edit(version_dir, &original, &document, cancelled, &control)
}

/// Restores the immediately previous successful edit; old single-file backups
/// are supported. The backup is removed only as part of a successful transaction.
pub fn undo_in_place(version_dir: &Path) -> Result<()> {
    undo(version_dir, None, &Arc::new(AtomicBool::new(false)))
}

pub fn undo_in_place_checked(
    version_dir: &Path,
    expected_document_sha256: &str,
    cancelled: &Arc<AtomicBool>,
) -> Result<()> {
    undo(version_dir, Some(expected_document_sha256), cancelled)
}

fn undo(version_dir: &Path, expected: Option<&str>, cancelled: &Arc<AtomicBool>) -> Result<()> {
    let _lock = begin(version_dir)?;
    check_cancelled(cancelled)?;
    check_revision(&std::fs::read(version_dir.join("document.json"))?, expected)?;
    let backup = version_dir.join(BACKUP);
    anyhow::ensure!(
        regular_file(&backup)?,
        crate::tr!("这份笔记没有可撤销的 AI 编辑 / No AI edit to undo")
    );
    let bytes = std::fs::read(backup)?;
    let document = parse_document(&bytes)?;
    let control = crate::dispatch::TaskControl::with_cancel_flag(cancelled.clone());
    let stage = prepare(version_dir, &bytes, &document, None, &control)?;
    check_cancelled(cancelled)?;
    commit(version_dir, stage)
}
#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;

    fn prepare(
        version_dir: &Path,
        bytes: &[u8],
        document: &crate::artifact::Document,
        previous: Option<&[u8]>,
    ) -> Result<tempfile::TempDir> {
        super::prepare(
            version_dir,
            bytes,
            document,
            previous,
            &crate::dispatch::TaskControl::default(),
        )
    }

    fn serve_once(reply: serde_json::Value) -> u16 {
        serve_once_with(reply, || {})
    }

    fn serve_once_with(
        reply: serde_json::Value,
        before_response: impl FnOnce() + Send + 'static,
    ) -> u16 {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut buf = vec![0u8; 65536];
            let _ = stream.read(&mut buf);
            before_response();
            let payload = reply.to_string();
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                payload.len(),
                payload
            );
            let _ = stream.write_all(response.as_bytes());
        });
        port
    }

    fn test_settings(port: u16) -> crate::llm::LlmSettings {
        crate::llm::LlmSettings {
            base_url: format!("http://127.0.0.1:{port}/v1"),
            api_key: "test".into(),
            model: "test".into(),
            ..crate::llm::LlmSettings::default()
        }
    }

    #[test]
    fn translate_in_place_writes_translation_and_backup() {
        let dir = tempfile::tempdir().unwrap();
        fixture(dir.path(), false);
        let content = r#"{"segments":[{"id":0,"text":"你好"}]}"#;
        let reply = serde_json::json!({
            "choices": [{"message": {"content": content}}]
        });
        let port = serve_once(reply);
        let report = translate_in_place(dir.path(), &test_settings(port), "zh-Hans").unwrap();
        assert_eq!(report.attempted, 1);
        assert_eq!(report.succeeded, 1);
        let document: serde_json::Value =
            serde_json::from_slice(&std::fs::read(dir.path().join("document.json")).unwrap())
                .unwrap();
        assert_eq!(document["sections"][0]["speech"][0]["translation"], "你好");
        assert!(dir.path().join("document.json.before-ai").is_file());
        let markdown = std::fs::read_to_string(dir.path().join("course.md")).unwrap();
        assert!(markdown.contains("你好"));
    }

    fn fixture(dir: &Path, with_translation: bool) {
        let translation = if with_translation { "你好" } else { "" };
        let speech = if with_translation {
            format!(r#"{{"start":0.0,"end":1.0,"text":"hello","translation":"{translation}"}}"#)
        } else {
            r#"{"start":0.0,"end":1.0,"text":"hello"}"#.to_owned()
        };
        let document = format!(
            r#"{{"schema":1,"meta":{{"title":"t","uploader":"","duration":1.0,"webpage_url":"/tmp/x.mp4","extractor":"local","id":"x"}},"sections":[{{"t":0.0,"end":1.0,"image":"","speech":[{speech}]}}],"summary":null}}"#
        );
        std::fs::write(dir.join("document.json"), document).unwrap();
    }

    #[test]
    fn undo_restores_backup_and_removes_it() {
        let dir = tempfile::tempdir().unwrap();
        fixture(dir.path(), true);
        std::fs::copy(
            dir.path().join("document.json"),
            dir.path().join("document.json.before-ai"),
        )
        .unwrap();
        undo_in_place(dir.path()).unwrap();
        let restored: serde_json::Value =
            serde_json::from_slice(&std::fs::read(dir.path().join("document.json")).unwrap())
                .unwrap();
        assert_eq!(restored["sections"][0]["speech"][0]["translation"], "你好");
        assert!(!dir.path().join("document.json.before-ai").exists());
        assert!(dir.path().join("course.md").is_file());
    }

    #[test]
    fn apply_region_edit_replaces_text_and_translation() {
        let dir = tempfile::tempdir().unwrap();
        fixture(dir.path(), false);
        apply_region_edit(dir.path(), 0, 0, "hello there", Some("你好")).unwrap();
        let document: serde_json::Value =
            serde_json::from_slice(&std::fs::read(dir.path().join("document.json")).unwrap())
                .unwrap();
        assert_eq!(document["sections"][0]["speech"][0]["text"], "hello there");
        assert_eq!(document["sections"][0]["speech"][0]["translation"], "你好");
        assert!(dir.path().join("document.json.before-ai").is_file());
    }

    #[test]
    fn undo_without_backup_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        fixture(dir.path(), true);
        assert!(undo_in_place(dir.path()).is_err());
        assert!(dir.path().join("document.json").is_file());
    }

    fn rich_fixture(dir: &Path) {
        fixture(dir, false);
        let mut document = load_document(dir).unwrap();
        document.summary = Some(crate::summarize::Summary {
            tldr: "Persistent summary".into(),
            key_points: vec!["Important point".into()],
            outline: vec![],
        });
        write_json(&dir.join("document.json"), &document).unwrap();
        std::fs::write(dir.join("course.md"), render_markdown(&document)).unwrap();
        let destinations: Vec<_> = [
            crate::config::OutputFormat::Md,
            crate::config::OutputFormat::Html,
            crate::config::OutputFormat::Json,
        ]
        .into_iter()
        .map(|format| {
            (
                format,
                dir.join("exports").join(crate::portable::file_name(format)),
            )
        })
        .collect();
        crate::portable::export_document(
            dir,
            &document,
            &destinations,
            &crate::dispatch::TaskControl::default(),
        )
        .unwrap();
        std::fs::write(dir.join("untouched.bin"), "untouched asset").unwrap();
        let names = [
            "document.json",
            "course.md",
            "untouched.bin",
            "exports/course.zip",
            "exports/course.html",
            "exports/structured.json",
        ];
        let assets: Vec<_> = names
            .iter()
            .map(|name| crate::artifact::Asset {
                path: name.to_string(),
                bytes: std::fs::metadata(dir.join(name)).unwrap().len(),
                sha256: crate::execution::file_digest(&dir.join(name)).unwrap(),
            })
            .collect();
        let manifest = crate::artifact::Manifest {
            schema: 1,
            task_id: "task".into(),
            course_id: "course".into(),
            source_id: "source".into(),
            version_id: "version".into(),
            title: "t".into(),
            created_at_ms: 0,
            revision: 1,
            document: "document.json".into(),
            markdown: "course.md".into(),
            frames: vec![],
            assets,
            outputs: names[3..].iter().map(|name| name.to_string()).collect(),
            outcomes: Default::default(),
            partial: false,
        };
        write_json(&dir.join("manifest.json"), &manifest).unwrap();
    }

    fn snapshot(dir: &Path) -> Vec<(String, Vec<u8>)> {
        [
            "document.json",
            "course.md",
            "manifest.json",
            BACKUP,
            "exports/course.zip",
            "exports/course.html",
            "exports/structured.json",
        ]
        .into_iter()
        .filter_map(|name| {
            std::fs::read(dir.join(name))
                .ok()
                .map(|bytes| (name.into(), bytes))
        })
        .collect()
    }

    fn check_consistent(dir: &Path, expected_text: &str, expected_revision: u64) {
        let document = load_document(dir).unwrap();
        assert_eq!(document.sections[0].speech[0].text, expected_text);
        assert_eq!(
            document.summary.as_ref().unwrap().tldr,
            "Persistent summary"
        );
        let markdown = std::fs::read_to_string(dir.join("course.md")).unwrap();
        let html = std::fs::read_to_string(dir.join("exports/course.html")).unwrap();
        let mut zip =
            zip::ZipArchive::new(std::fs::File::open(dir.join("exports/course.zip")).unwrap())
                .unwrap();
        let mut zipped = String::new();
        zip.by_name("course.md")
            .unwrap()
            .read_to_string(&mut zipped)
            .unwrap();
        for text in [&markdown, &html, &zipped] {
            assert!(text.contains(expected_text));
            assert!(text.contains("Persistent summary"));
        }
        let json: serde_json::Value =
            serde_json::from_slice(&std::fs::read(dir.join("exports/structured.json")).unwrap())
                .unwrap();
        assert_eq!(json["sections"][0]["speech"][0]["text"], expected_text);
        assert_eq!(json["summary"]["tldr"], "Persistent summary");
        let manifest = crate::artifact::read_manifest(&dir.join("manifest.json")).unwrap();
        assert_eq!(manifest.revision, expected_revision);
        // Unlike validate_version this also verifies every optional export.
        for asset in &manifest.assets {
            assert_eq!(
                std::fs::metadata(dir.join(&asset.path)).unwrap().len(),
                asset.bytes
            );
            assert_eq!(
                crate::execution::file_digest(&dir.join(&asset.path)).unwrap(),
                asset.sha256,
                "{}",
                asset.path
            );
        }
    }

    fn selection_edit(
        section: usize,
        speech: usize,
        start: usize,
        end: usize,
        text: &str,
    ) -> SelectionEdit {
        SelectionEdit {
            section,
            speech,
            start,
            end,
            text: text.into(),
        }
    }

    #[test]
    fn selection_batch_preserves_unselected_unicode_and_metadata_and_undoes_as_one() {
        let dir = tempfile::tempdir().unwrap();
        fixture(dir.path(), true);
        let path = dir.path().join("document.json");
        let mut value: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        value["extension"] = serde_json::json!({"future": true});
        value["sections"][0]["speech"] = serde_json::json!([
            {"start":0.0,"end":1.0,"text":"前🙂中🙂后","translation":"old first","extra":7},
            {"start":1.0,"end":2.0,"text":"第二段原文","translation":"old second"},
            {"start":2.0,"end":3.0,"text":"未选段","translation":"untouched"}
        ]);
        let mut section = value["sections"][0].clone();
        section["speech"] = serde_json::json!([
            {"start":3.0,"end":4.0,"text":"跨节文本","translation":"old third"}
        ]);
        value["sections"].as_array_mut().unwrap().push(section);
        write_json(&path, &value).unwrap();
        let original = std::fs::read(&path).unwrap();
        let revision =
            document_revision_for(dir.path(), &load_document(dir.path()).unwrap()).unwrap();
        let cancelled = Arc::new(AtomicBool::new(false));
        // Unsorted inputs, repeated source words, variable replacement lengths,
        // multiple ranges in one passage, multiple passages and multiple sections.
        let edits = [
            selection_edit(1, 0, 0, 6, "另一节"),
            selection_edit(0, 0, 10, 14, "好"),
            selection_edit(0, 1, 3, 6, "三"),
            selection_edit(0, 0, 3, 7, "更好🙂"),
        ];
        apply_selection_edits_checked(dir.path(), &edits, &revision, &cancelled).unwrap();
        let actual: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        let mut expected = value;
        for (section, speech, text) in [
            (0, 0, "前更好🙂中好后"),
            (0, 1, "第三段原文"),
            (1, 0, "另一节文本"),
        ] {
            expected["sections"][section]["speech"][speech]["text"] = text.into();
            expected["sections"][section]["speech"][speech]["translation"] =
                serde_json::Value::Null;
        }
        assert_eq!(actual, expected);
        assert_eq!(std::fs::read(dir.path().join(BACKUP)).unwrap(), original);
        undo_in_place_checked(
            dir.path(),
            &document_revision(dir.path()).unwrap(),
            &cancelled,
        )
        .unwrap();
        assert_eq!(std::fs::read(path).unwrap(), original);
        assert!(!dir.path().join(BACKUP).exists());
    }

    #[test]
    fn selection_batch_rejects_every_invalid_range_without_changing_files_or_undo() {
        let dir = tempfile::tempdir().unwrap();
        rich_fixture(dir.path());
        apply_region_edit(dir.path(), 0, 0, "前🙂后", Some("translation")).unwrap();
        let before = snapshot(dir.path());
        let revision = document_revision(dir.path()).unwrap();
        let cancelled = Arc::new(AtomicBool::new(false));
        let valid = selection_edit(0, 0, 0, 3, "changed");
        let mut batches = vec![Vec::new()];
        for invalid in [
            selection_edit(1, 0, 0, 3, "bad section"),
            selection_edit(0, 1, 0, 3, "bad speech"),
            selection_edit(0, 0, 3, 3, "empty"),
            selection_edit(0, 0, 7, 3, "reversed"),
            selection_edit(0, 0, 3, usize::MAX, "out of bounds"),
            selection_edit(0, 0, usize::MAX, usize::MAX, "out of bounds"),
            selection_edit(0, 0, 4, 7, "split start"),
            selection_edit(0, 0, 3, 6, "split end"),
            selection_edit(0, 0, 0, 7, "overlap"),
            valid.clone(),
        ] {
            batches.push(vec![valid.clone(), invalid]);
        }
        for edits in batches {
            assert!(
                apply_selection_edits_checked(dir.path(), &edits, &revision, &cancelled).is_err(),
                "accepted invalid batch: {edits:?}"
            );
            assert_eq!(snapshot(dir.path()), before);
            assert!(!dir.path().join(JOURNAL).exists());
        }
    }

    #[test]
    fn selection_batch_refreshes_exports_once_and_preserves_no_op_translation_and_undo() {
        let dir = tempfile::tempdir().unwrap();
        rich_fixture(dir.path());
        apply_region_edit(dir.path(), 0, 0, "hello", Some("old translation")).unwrap();
        let before = snapshot(dir.path());
        let revision = document_revision(dir.path()).unwrap();
        let cancelled = Arc::new(AtomicBool::new(false));
        // Two adjacent replacements that together leave the passage unchanged.
        let no_op = [
            selection_edit(0, 0, 0, 1, "he"),
            selection_edit(0, 0, 1, 2, ""),
        ];
        apply_selection_edits_checked(dir.path(), &no_op, &revision, &cancelled).unwrap();
        assert_eq!(snapshot(dir.path()), before);
        let edits = [
            selection_edit(0, 0, 0, 2, "HE"),
            selection_edit(0, 0, 2, 3, "L"),
            selection_edit(0, 0, 4, 5, "O!"),
        ];
        apply_selection_edits_checked(dir.path(), &edits, &revision, &cancelled).unwrap();
        check_consistent(dir.path(), "HELlO!", 3);
        assert!(
            load_document(dir.path()).unwrap().sections[0].speech[0]
                .translation
                .is_none()
        );
        undo_in_place(dir.path()).unwrap();
        check_consistent(dir.path(), "hello", 4);
        assert_eq!(
            load_document(dir.path()).unwrap().sections[0].speech[0]
                .translation
                .as_deref(),
            Some("old translation")
        );
    }

    #[test]
    fn selection_batch_checks_lock_revision_cancellation_and_export_failure() {
        let dir = tempfile::tempdir().unwrap();
        rich_fixture(dir.path());
        let stale = document_revision(dir.path()).unwrap();
        apply_region_edit(dir.path(), 0, 0, "hello there", None).unwrap();
        let before = snapshot(dir.path());
        let revision = document_revision(dir.path()).unwrap();
        let edits = [selection_edit(0, 0, 0, 5, "goodbye")];
        let cancelled = Arc::new(AtomicBool::new(false));
        assert!(apply_selection_edits_checked(dir.path(), &edits, &stale, &cancelled).is_err());
        let lock = begin(dir.path()).unwrap();
        assert!(apply_selection_edits_checked(dir.path(), &edits, &revision, &cancelled).is_err());
        drop(lock);
        cancelled.store(true, Ordering::Release);
        assert!(apply_selection_edits_checked(dir.path(), &edits, &revision, &cancelled).is_err());
        assert_eq!(snapshot(dir.path()), before);
        cancelled.store(false, Ordering::Release);
        let mut document = load_document(dir.path()).unwrap();
        document.sections[0].image = "missing.png".into();
        write_json(&dir.path().join("document.json"), &document).unwrap();
        let before = snapshot(dir.path());
        let revision = document_revision(dir.path()).unwrap();
        assert!(apply_selection_edits_checked(dir.path(), &edits, &revision, &cancelled).is_err());
        assert_eq!(snapshot(dir.path()), before);
        assert!(!dir.path().join(JOURNAL).exists());
    }

    #[test]
    fn selection_batch_can_delete_selected_text_but_not_the_last_readable_body() {
        let dir = tempfile::tempdir().unwrap();
        rich_fixture(dir.path());
        let before = snapshot(dir.path());
        let revision = document_revision(dir.path()).unwrap();
        let cancelled = Arc::new(AtomicBool::new(false));
        let edits = [selection_edit(0, 0, 0, 5, "")];
        assert!(apply_selection_edits_checked(dir.path(), &edits, &revision, &cancelled).is_err());
        assert_eq!(snapshot(dir.path()), before);
        let edits = [selection_edit(0, 0, 1, 4, "")];
        apply_selection_edits_checked(dir.path(), &edits, &revision, &cancelled).unwrap();
        check_consistent(dir.path(), "ho", 2);
    }

    fn selection_translation_fixture(dir: &Path) -> serde_json::Value {
        rich_fixture(dir);
        let path = dir.join("document.json");
        let mut value: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        value["extension"] = serde_json::json!({"future": true});
        value["sections"][0]["speech"] = serde_json::json!([
            {"start":0.0,"end":1.0,"text":"前🙂中🙂后","raw":"original raw 🙂",
             "translation":"seed translation","extra":{"keep":7}},
            {"start":1.0,"end":2.0,"text":"Second full passage.","raw":"second raw",
             "translation":"old second translation"},
            {"start":2.0,"end":3.0,"text":"未选段","raw":"untouched raw",
             "translation":"untouched translation"}
        ]);
        let mut section = value["sections"][0].clone();
        section["t"] = 3.0.into();
        section["end"] = 4.0.into();
        section["speech"] = serde_json::json!([
            {"start":3.0,"end":4.0,"text":"跨节文本","raw":"cross-section raw"}
        ]);
        value["sections"].as_array_mut().unwrap().push(section);
        write_json(&path, &value).unwrap();
        // Publish a coherent baseline, including exports and a previous undo.
        apply_region_edit(dir, 0, 0, "前🙂中🙂后", Some("old full translation")).unwrap();
        serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap()
    }

    #[test]
    fn selection_translations_preserve_source_raw_and_full_translation_and_undo_as_one() {
        let dir = tempfile::tempdir().unwrap();
        let original = selection_translation_fixture(dir.path());
        let path = dir.path().join("document.json");
        let original_bytes = std::fs::read(&path).unwrap();
        let cancelled = Arc::new(AtomicBool::new(false));
        let edits = [
            selection_edit(1, 0, 0, 6, "cross-section translation"),
            selection_edit(0, 0, 10, 14, "second emoji translation"),
            selection_edit(0, 1, 0, "Second full passage.".len(), " full replacement "),
            selection_edit(0, 0, 3, 7, "first emoji translation"),
        ];
        apply_selection_translations_checked(
            dir.path(),
            &edits,
            &document_revision(dir.path()).unwrap(),
            &cancelled,
        )
        .unwrap();
        let actual: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        let first = &actual["sections"][0]["speech"][0];
        let translation = first["translation"].as_str().unwrap();
        assert!(translation.starts_with("old full translation\n\n"));
        assert!(
            translation.find("first emoji translation").unwrap()
                < translation.find("second emoji translation").unwrap()
        );
        assert_eq!(
            first["selection_translations"]["base"],
            "old full translation"
        );
        assert_eq!(
            first["selection_translations"]["parts"],
            serde_json::json!([
                {"start":3,"end":7,"text":"first emoji translation"},
                {"start":10,"end":14,"text":"second emoji translation"}
            ])
        );
        assert_eq!(
            actual["sections"][0]["speech"][1]["translation"],
            "full replacement"
        );
        assert!(
            actual["sections"][0]["speech"][1]
                .get("selection_translations")
                .is_none()
        );
        assert!(actual["sections"][1]["speech"][0]["selection_translations"]["base"].is_null());
        // Compare every source/raw/timing/extension field, including unselected passages.
        let mut restored_fields = actual.clone();
        for (section, speech) in [(0, 0), (0, 1), (1, 0)] {
            let target = restored_fields["sections"][section]["speech"][speech]
                .as_object_mut()
                .unwrap();
            target.remove("selection_translations");
            target.remove("translation");
            if let Some(old) = original["sections"][section]["speech"][speech].get("translation") {
                target.insert("translation".into(), old.clone());
            }
        }
        assert_eq!(restored_fields, original);
        check_consistent(dir.path(), "前🙂中🙂后", 3);
        for name in [
            "course.md",
            "exports/course.html",
            "exports/structured.json",
        ] {
            let export = std::fs::read_to_string(dir.path().join(name)).unwrap();
            for text in [
                "old full translation",
                "first emoji translation",
                "second emoji translation",
                "full replacement",
                "cross-section translation",
            ] {
                assert!(export.contains(text), "{name} omitted {text}");
            }
        }
        assert_eq!(
            std::fs::read(dir.path().join(BACKUP)).unwrap(),
            original_bytes
        );
        undo_in_place_checked(
            dir.path(),
            &document_revision(dir.path()).unwrap(),
            &cancelled,
        )
        .unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), original_bytes);
        check_consistent(dir.path(), "前🙂中🙂后", 4);
        assert!(!dir.path().join(BACKUP).exists());
        let restored = snapshot(dir.path());
        assert!(undo_in_place(dir.path()).is_err());
        assert_eq!(snapshot(dir.path()), restored);
    }

    #[test]
    fn selection_translations_replace_repeated_ranges_and_full_passage_supersedes_parts() {
        let dir = tempfile::tempdir().unwrap();
        selection_translation_fixture(dir.path());
        let path = dir.path().join("document.json");
        let cancelled = Arc::new(AtomicBool::new(false));
        let initial = [
            selection_edit(0, 0, 3, 7, "old partial"),
            selection_edit(0, 0, 10, 14, "keep partial"),
        ];
        apply_selection_translations_checked(
            dir.path(),
            &initial,
            &document_revision(dir.path()).unwrap(),
            &cancelled,
        )
        .unwrap();
        let replacement = [selection_edit(0, 0, 3, 7, "replacement partial")];
        apply_selection_translations_checked(
            dir.path(),
            &replacement,
            &document_revision(dir.path()).unwrap(),
            &cancelled,
        )
        .unwrap();
        let partial_bytes = std::fs::read(&path).unwrap();
        let value: serde_json::Value = serde_json::from_slice(&partial_bytes).unwrap();
        let event = &value["sections"][0]["speech"][0];
        assert_eq!(
            event["selection_translations"]["parts"],
            serde_json::json!([
                {"start":3,"end":7,"text":"replacement partial"},
                {"start":10,"end":14,"text":"keep partial"}
            ])
        );
        let rendered = event["translation"].as_str().unwrap();
        assert!(!rendered.contains("old partial"));
        for text in [
            "old full translation",
            "replacement partial",
            "keep partial",
        ] {
            assert_eq!(rendered.matches(text).count(), 1);
        }
        let before = snapshot(dir.path());
        apply_selection_translations_checked(
            dir.path(),
            &replacement,
            &document_revision(dir.path()).unwrap(),
            &cancelled,
        )
        .unwrap();
        assert_eq!(
            snapshot(dir.path()),
            before,
            "identical translation must retain revision and undo"
        );
        let full = [selection_edit(
            0,
            0,
            0,
            "前🙂中🙂后".len(),
            "new full translation",
        )];
        apply_selection_translations_checked(
            dir.path(),
            &full,
            &document_revision(dir.path()).unwrap(),
            &cancelled,
        )
        .unwrap();
        let value: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        let event = &value["sections"][0]["speech"][0];
        assert_eq!(event["translation"], "new full translation");
        assert_eq!(event["text"], "前🙂中🙂后");
        assert_eq!(event["raw"], "original raw 🙂");
        assert!(event.get("selection_translations").is_none());
        undo_in_place(dir.path()).unwrap();
        assert_eq!(std::fs::read(path).unwrap(), partial_bytes);
    }

    #[test]
    fn selection_translations_reject_invalid_metadata_without_publishing_earlier_passages() {
        let dir = tempfile::tempdir().unwrap();
        selection_translation_fixture(dir.path());
        let cancelled = Arc::new(AtomicBool::new(false));
        apply_selection_translations_checked(
            dir.path(),
            &[selection_edit(1, 0, 0, 3, "prior partial")],
            &document_revision(dir.path()).unwrap(),
            &cancelled,
        )
        .unwrap();
        let path = dir.path().join("document.json");
        let valid: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        let metadata = valid["sections"][1]["speech"][0]["selection_translations"].clone();
        let mut corruptions = vec![
            ("null metadata", serde_json::Value::Null),
            ("missing fields", serde_json::json!({})),
        ];
        for (key, replacement) in [
            ("schema", serde_json::json!(2)),
            ("source_sha256", serde_json::json!("stale source digest")),
            (
                "rendered",
                serde_json::json!("different visible translation"),
            ),
            ("base", serde_json::json!(42)),
            ("unknown_field", serde_json::json!(true)),
        ] {
            let mut invalid = metadata.clone();
            invalid[key] = replacement;
            corruptions.push((key, invalid));
        }
        for (label, parts) in [
            (
                "split UTF-8 start",
                serde_json::json!([{"start":1,"end":3,"text":"x"}]),
            ),
            (
                "split UTF-8 end",
                serde_json::json!([{"start":0,"end":2,"text":"x"}]),
            ),
            (
                "out of bounds",
                serde_json::json!([{"start":0,"end":99,"text":"x"}]),
            ),
            (
                "empty range",
                serde_json::json!([{"start":3,"end":3,"text":"x"}]),
            ),
            (
                "reversed range",
                serde_json::json!([{"start":6,"end":3,"text":"x"}]),
            ),
            (
                "empty translation",
                serde_json::json!([{"start":0,"end":3,"text":" \n\t"}]),
            ),
            (
                "overlap",
                serde_json::json!([{"start":0,"end":6,"text":"x"},{"start":3,"end":9,"text":"y"}]),
            ),
            (
                "duplicate range",
                serde_json::json!([{"start":0,"end":3,"text":"x"},{"start":0,"end":3,"text":"y"}]),
            ),
            (
                "reversed order",
                serde_json::json!([{"start":6,"end":9,"text":"x"},{"start":0,"end":3,"text":"y"}]),
            ),
        ] {
            let mut invalid = metadata.clone();
            invalid["parts"] = parts;
            corruptions.push((label, invalid));
        }
        for (label, invalid) in corruptions {
            let mut document = valid.clone();
            document["sections"][1]["speech"][0]["selection_translations"] = invalid;
            write_json(&path, &document).unwrap();
            let before = snapshot(dir.path());
            let edits = [
                selection_edit(0, 0, 3, 7, "must not publish"),
                selection_edit(1, 0, 6, 9, "also not published"),
            ];
            assert!(
                apply_selection_translations_checked(
                    dir.path(),
                    &edits,
                    &document_revision(dir.path()).unwrap(),
                    &cancelled
                )
                .is_err(),
                "accepted {label}"
            );
            assert_eq!(snapshot(dir.path()), before, "wrote files for {label}");
            assert!(!dir.path().join(JOURNAL).exists());
        }
    }

    #[test]
    fn selection_translations_reject_invalid_ranges_empty_results_and_cancel_without_writes() {
        let dir = tempfile::tempdir().unwrap();
        selection_translation_fixture(dir.path());
        let before = snapshot(dir.path());
        let revision = document_revision(dir.path()).unwrap();
        let cancelled = Arc::new(AtomicBool::new(false));
        let valid = selection_edit(0, 0, 0, 3, "valid translation");
        let mut batches = vec![vec![]];
        for invalid in [
            selection_edit(99, 0, 0, 3, "bad section"),
            selection_edit(0, 99, 0, 3, "bad passage"),
            selection_edit(0, 0, 4, 7, "split start"),
            selection_edit(0, 0, 3, 6, "split end"),
            selection_edit(0, 0, 3, 3, "empty range"),
            selection_edit(0, 0, 7, 3, "reversed range"),
            selection_edit(0, 0, 3, usize::MAX, "out of bounds"),
            selection_edit(0, 0, 0, 7, "overlap"),
            selection_edit(1, 0, 0, 6, ""),
            selection_edit(1, 0, 0, 6, " \n\t　"),
            valid.clone(),
        ] {
            batches.push(vec![valid.clone(), invalid]);
        }
        for edits in batches {
            assert!(
                apply_selection_translations_checked(dir.path(), &edits, &revision, &cancelled)
                    .is_err(),
                "accepted {edits:?}"
            );
            assert_eq!(snapshot(dir.path()), before);
            assert!(!dir.path().join(JOURNAL).exists());
        }
        cancelled.store(true, Ordering::Release);
        assert!(
            apply_selection_translations_checked(
                dir.path(),
                std::slice::from_ref(&valid),
                &revision,
                &cancelled
            )
            .is_err()
        );
        assert_eq!(snapshot(dir.path()), before);
        cancelled.store(false, Ordering::Release);
        assert!(
            apply_selection_translations_checked(
                dir.path(),
                &[valid],
                "stale revision",
                &cancelled
            )
            .is_err()
        );
        assert_eq!(snapshot(dir.path()), before);
        assert!(!dir.path().join(JOURNAL).exists());
    }

    #[test]
    fn selection_translations_source_edits_clear_only_changed_passage_metadata() {
        for use_selection_edit in [true, false] {
            let dir = tempfile::tempdir().unwrap();
            selection_translation_fixture(dir.path());
            let cancelled = Arc::new(AtomicBool::new(false));
            let parts = [
                selection_edit(0, 0, 3, 7, "first partial"),
                selection_edit(1, 0, 0, 6, "cross partial"),
            ];
            apply_selection_translations_checked(
                dir.path(),
                &parts,
                &document_revision(dir.path()).unwrap(),
                &cancelled,
            )
            .unwrap();
            let path = dir.path().join("document.json");
            let original_bytes = std::fs::read(&path).unwrap();
            let original: serde_json::Value = serde_json::from_slice(&original_bytes).unwrap();
            let before = snapshot(dir.path());
            apply_selection_edits_checked(
                dir.path(),
                &[selection_edit(0, 0, 3, 7, "🙂")],
                &document_revision(dir.path()).unwrap(),
                &cancelled,
            )
            .unwrap();
            assert_eq!(
                snapshot(dir.path()),
                before,
                "no-op source edits must retain translation metadata"
            );
            if use_selection_edit {
                apply_selection_edits_checked(
                    dir.path(),
                    &[selection_edit(0, 0, 3, 7, "新")],
                    &document_revision(dir.path()).unwrap(),
                    &cancelled,
                )
                .unwrap();
            } else {
                apply_region_edit(dir.path(), 0, 0, "前新中🙂后", None).unwrap();
            }
            let value: serde_json::Value =
                serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
            let first = &value["sections"][0]["speech"][0];
            assert_eq!(first["text"], "前新中🙂后");
            assert_eq!(first["raw"], "original raw 🙂");
            assert!(first["translation"].is_null());
            assert!(first.get("selection_translations").is_none());
            assert_eq!(value["sections"][1], original["sections"][1]);
            undo_in_place(dir.path()).unwrap();
            assert_eq!(std::fs::read(&path).unwrap(), original_bytes);
        }
    }

    #[test]
    fn selection_batch_deletes_whole_passages_and_cross_passage_ranges_then_undoes() {
        let dir = tempfile::tempdir().unwrap();
        let original = selection_translation_fixture(dir.path());
        let path = dir.path().join("document.json");
        let original_bytes = std::fs::read(&path).unwrap();
        let cancelled = Arc::new(AtomicBool::new(false));
        let edits = [
            selection_edit(1, 0, 0, "跨节文本".len(), ""),
            selection_edit(0, 1, 0, "Second".len(), ""),
            selection_edit(0, 0, 0, "前🙂中🙂后".len(), ""),
        ];
        apply_selection_edits_checked(
            dir.path(),
            &edits,
            &document_revision(dir.path()).unwrap(),
            &cancelled,
        )
        .unwrap();
        let actual: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        let mut expected = original;
        for (section, speech, text) in [(0, 0, ""), (0, 1, " full passage."), (1, 0, "")] {
            expected["sections"][section]["speech"][speech]["text"] = text.into();
            if expected["sections"][section]["speech"][speech]
                .get("translation")
                .is_some()
            {
                expected["sections"][section]["speech"][speech]["translation"] =
                    serde_json::Value::Null;
            }
        }
        assert_eq!(actual, expected);
        assert_eq!(
            load_document(dir.path()).unwrap().sections[0].speech[2].text,
            "未选段"
        );
        assert_eq!(
            std::fs::read(dir.path().join(BACKUP)).unwrap(),
            original_bytes
        );
        undo_in_place_checked(
            dir.path(),
            &document_revision(dir.path()).unwrap(),
            &cancelled,
        )
        .unwrap();
        assert_eq!(std::fs::read(path).unwrap(), original_bytes);
        check_consistent(dir.path(), "前🙂中🙂后", 4);
        assert!(!dir.path().join(BACKUP).exists());
    }

    #[test]
    fn successive_edits_undo_only_the_latest_and_keep_all_exports_and_summary() {
        let dir = tempfile::tempdir().unwrap();
        rich_fixture(dir.path());
        apply_region_edit(dir.path(), 0, 0, "first edit", Some("first translation")).unwrap();
        let first = std::fs::read(dir.path().join("document.json")).unwrap();
        apply_region_edit(dir.path(), 0, 0, "second edit", None).unwrap();
        check_consistent(dir.path(), "second edit", 3);
        assert_eq!(std::fs::read(dir.path().join(BACKUP)).unwrap(), first);
        undo_in_place(dir.path()).unwrap();
        check_consistent(dir.path(), "first edit", 4);
        assert_eq!(
            std::fs::read(dir.path().join("document.json")).unwrap(),
            first
        );
        assert!(undo_in_place(dir.path()).is_err());
        apply_region_edit(dir.path(), 0, 0, "third edit", None).unwrap();
        undo_in_place(dir.path()).unwrap();
        check_consistent(dir.path(), "first edit", 6);
    }

    #[test]
    fn every_commit_boundary_rolls_back_all_files_and_previous_undo() {
        for undo in [false, true] {
            for fail_after in 0..7 {
                let dir = tempfile::tempdir().unwrap();
                rich_fixture(dir.path());
                apply_region_edit(dir.path(), 0, 0, "prior edit", None).unwrap();
                let before = snapshot(dir.path());
                let original = std::fs::read(dir.path().join("document.json")).unwrap();
                let mut document = load_document(dir.path()).unwrap();
                document.sections[0].speech[0].text = "next edit".into();
                let bytes = if undo {
                    std::fs::read(dir.path().join(BACKUP)).unwrap()
                } else {
                    serde_json::to_vec_pretty(&document).unwrap()
                };
                let document = parse_document(&bytes).unwrap();
                let stage = prepare(
                    dir.path(),
                    &bytes,
                    &document,
                    (!undo).then_some(original.as_slice()),
                )
                .unwrap();
                let result = commit_with_hook(dir.path(), stage, |index| {
                    if index == fail_after {
                        Err(std::io::Error::other("injected I/O failure").into())
                    } else {
                        Ok(())
                    }
                });
                assert!(result.is_err(), "boundary {fail_after}, undo {undo}");
                assert_eq!(
                    snapshot(dir.path()),
                    before,
                    "boundary {fail_after}, undo {undo}"
                );
                assert!(!dir.path().join(JOURNAL).exists());
            }
        }
    }

    #[test]
    fn interrupted_commit_recovers_before_next_operation() {
        let dir = tempfile::tempdir().unwrap();
        rich_fixture(dir.path());
        let before = snapshot(dir.path());
        let original = std::fs::read(dir.path().join("document.json")).unwrap();
        let mut document = load_document(dir.path()).unwrap();
        document.sections[0].speech[0].text = "interrupted".into();
        let bytes = serde_json::to_vec_pretty(&document).unwrap();
        let stage = prepare(dir.path(), &bytes, &document, Some(&original)).unwrap();
        std::fs::rename(stage.path(), dir.path().join(JOURNAL)).unwrap();
        for name in ["document.json", "course.md", "exports/course.zip"] {
            copy_atomic(&dir.path().join(JOURNAL).join(name), &dir.path().join(name)).unwrap();
        }
        // Simulates a restart: the checked operation itself performs recovery.
        recover_pending(dir.path()).unwrap();
        crate::artifact::validate_version(
            dir.path(),
            &crate::artifact::read_manifest(&dir.path().join("manifest.json")).unwrap(),
        )
        .unwrap();
        let revision = document_revision(dir.path()).unwrap();
        assert_eq!(revision, crate::execution::digest(&original));
        assert_eq!(snapshot(dir.path()), before);
        assert!(!dir.path().join(JOURNAL).exists());
    }

    #[test]
    fn damaged_recovery_copy_does_not_partially_restore_or_discard_journal() {
        let dir = tempfile::tempdir().unwrap();
        rich_fixture(dir.path());
        let original = std::fs::read(dir.path().join("document.json")).unwrap();
        let mut document = load_document(dir.path()).unwrap();
        document.sections[0].speech[0].text = "interrupted".into();
        let bytes = serde_json::to_vec_pretty(&document).unwrap();
        let stage = prepare(dir.path(), &bytes, &document, Some(&original)).unwrap();
        std::fs::rename(stage.path(), dir.path().join(JOURNAL)).unwrap();
        copy_atomic(
            &dir.path().join(JOURNAL).join("document.json"),
            &dir.path().join("document.json"),
        )
        .unwrap();
        let current = snapshot(dir.path());
        let damaged = dir.path().join(JOURNAL).join("old-1");
        let good = std::fs::read(&damaged).unwrap();
        std::fs::write(&damaged, "damaged").unwrap();
        assert!(document_revision(dir.path()).is_err());
        assert_eq!(snapshot(dir.path()), current);
        assert!(dir.path().join(JOURNAL).is_dir());
        std::fs::write(damaged, good).unwrap();
        document_revision(dir.path()).unwrap();
        assert_eq!(
            std::fs::read(dir.path().join("document.json")).unwrap(),
            original
        );
    }

    #[test]
    fn preparation_failure_and_invalid_legacy_backup_preserve_originals() {
        let dir = tempfile::tempdir().unwrap();
        rich_fixture(dir.path());
        apply_region_edit(dir.path(), 0, 0, "first edit", None).unwrap();
        // Invalid image causes the public exporter to fail before publication.
        let mut document = load_document(dir.path()).unwrap();
        document.sections[0].image = "missing.png".into();
        write_json(&dir.path().join("document.json"), &document).unwrap();
        let before = snapshot(dir.path());
        assert!(apply_region_edit(dir.path(), 0, 0, "must not land", None).is_err());
        assert_eq!(snapshot(dir.path()), before);
        std::fs::write(dir.path().join(BACKUP), "broken legacy backup").unwrap();
        let before = snapshot(dir.path());
        assert!(undo_in_place(dir.path()).is_err());
        assert_eq!(snapshot(dir.path()), before);
    }

    #[test]
    fn no_op_and_failed_translation_keep_prior_undo_and_revision() {
        let dir = tempfile::tempdir().unwrap();
        rich_fixture(dir.path());
        apply_region_edit(dir.path(), 0, 0, "first edit", None).unwrap();
        let before = snapshot(dir.path());
        apply_region_edit(dir.path(), 0, 0, "first edit", None).unwrap();
        let settings = crate::llm::LlmSettings {
            model: String::new(),
            ..Default::default()
        };
        let _ = translate_in_place(dir.path(), &settings, "zh-Hans");
        assert_eq!(snapshot(dir.path()), before);
    }

    #[test]
    fn edits_do_not_rehash_unrelated_assets_and_preserve_unknown_fields() {
        let dir = tempfile::tempdir().unwrap();
        rich_fixture(dir.path());
        let path = dir.path().join("manifest.json");
        let mut value: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        value["extension"] = "keep manifest extension".into();
        write_json(&path, &value).unwrap();
        let old_assets = value["assets"].clone();
        let path = dir.path().join("document.json");
        let mut value: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        value["extension"] = "keep document extension".into();
        value["sections"][0]["speech"][0]["extension"] = "keep speech extension".into();
        write_json(&path, &value).unwrap();
        // This would fail or alter its digest if all manifest assets were read.
        std::fs::remove_file(dir.path().join("untouched.bin")).unwrap();
        apply_region_edit(dir.path(), 0, 0, "edit", None).unwrap();
        let manifest: serde_json::Value =
            serde_json::from_slice(&std::fs::read(dir.path().join("manifest.json")).unwrap())
                .unwrap();
        assert_eq!(manifest["assets"][2], old_assets[2]);
        assert_eq!(manifest["extension"], "keep manifest extension");
        let value: serde_json::Value =
            serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
        assert_eq!(value["extension"], "keep document extension");
        assert_eq!(
            value["sections"][0]["speech"][0]["extension"],
            "keep speech extension"
        );
    }

    #[test]
    fn lock_stale_ui_revision_and_cancellation_prevent_writes() {
        let dir = tempfile::tempdir().unwrap();
        rich_fixture(dir.path());
        let ui = load_document(dir.path()).unwrap();
        let revision = document_revision_for(dir.path(), &ui).unwrap();
        let lock = begin(dir.path()).unwrap();
        let before = snapshot(dir.path());
        assert!(apply_region_edit(dir.path(), 0, 0, "blocked", None).is_err());
        assert_eq!(snapshot(dir.path()), before);
        drop(lock);
        apply_region_edit(dir.path(), 0, 0, "new version", None).unwrap();
        assert!(document_revision_for(dir.path(), &ui).is_err());
        let before = snapshot(dir.path());
        let cancelled = Arc::new(AtomicBool::new(false));
        assert!(
            apply_region_edit_checked(dir.path(), 0, 0, "stale", None, &revision, &cancelled)
                .is_err()
        );
        assert!(undo_in_place_checked(dir.path(), &revision, &cancelled).is_err());
        let revision = document_revision(dir.path()).unwrap();
        cancelled.store(true, Ordering::Release);
        assert!(
            apply_region_edit_checked(dir.path(), 0, 0, "cancelled", None, &revision, &cancelled)
                .is_err()
        );
        assert!(undo_in_place_checked(dir.path(), &revision, &cancelled).is_err());
        assert!(
            translate_in_place_checked(
                dir.path(),
                &test_settings(1),
                "zh-Hans",
                &revision,
                &cancelled
            )
            .is_err()
        );
        assert_eq!(snapshot(dir.path()), before);
    }

    #[test]
    fn cancellation_during_translation_prevents_publication() {
        let dir = tempfile::tempdir().unwrap();
        rich_fixture(dir.path());
        apply_region_edit(dir.path(), 0, 0, "prior edit", None).unwrap();
        let before = snapshot(dir.path());
        let revision = document_revision(dir.path()).unwrap();
        let cancelled = std::sync::Arc::new(AtomicBool::new(false));
        let flag = cancelled.clone();
        let port = serve_once_with(
            serde_json::json!({
                "choices": [{"message": {"content": r#"{"segments":[{"id":0,"text":"translated"}]}"#}}]
            }),
            move || flag.store(true, Ordering::Release),
        );
        assert!(
            translate_in_place_checked(
                dir.path(),
                &test_settings(port),
                "zh-Hans",
                &revision,
                &cancelled
            )
            .is_err()
        );
        assert!(cancelled.load(Ordering::Acquire));
        assert_eq!(snapshot(dir.path()), before);
        assert!(!dir.path().join(JOURNAL).exists());
    }

    #[test]
    fn translation_keeps_summary_and_refreshes_every_export() {
        let dir = tempfile::tempdir().unwrap();
        rich_fixture(dir.path());
        let port = serve_once(serde_json::json!({
            "choices": [{"message": {"content": r#"{"segments":[{"id":0,"text":"translated passage"}]}"#}}]
        }));
        let ui = load_document(dir.path()).unwrap();
        let revision = document_revision_for(dir.path(), &ui).unwrap();
        let report = translate_in_place_checked(
            dir.path(),
            &test_settings(port),
            "zh-Hans",
            &revision,
            &Arc::new(AtomicBool::new(false)),
        )
        .unwrap();
        assert_eq!(report.succeeded, 1);
        check_consistent(dir.path(), "hello", 2);
        for name in [
            "course.md",
            "exports/course.html",
            "exports/structured.json",
        ] {
            assert!(
                std::fs::read_to_string(dir.path().join(name))
                    .unwrap()
                    .contains("translated passage")
            );
        }
        let revision = document_revision(dir.path()).unwrap();
        undo_in_place_checked(dir.path(), &revision, &Arc::new(AtomicBool::new(false))).unwrap();
        check_consistent(dir.path(), "hello", 3);
        assert!(
            load_document(dir.path()).unwrap().sections[0].speech[0]
                .translation
                .is_none()
        );
    }

    #[test]
    fn completed_journal_cleanup_never_reverts_committed_edit() {
        let dir = tempfile::tempdir().unwrap();
        rich_fixture(dir.path());
        let original = std::fs::read(dir.path().join("document.json")).unwrap();
        let mut document = load_document(dir.path()).unwrap();
        document.sections[0].speech[0].text = "committed".into();
        let bytes = serde_json::to_vec_pretty(&document).unwrap();
        let stage = prepare(dir.path(), &bytes, &document, Some(&original)).unwrap();
        let path = dir.path().join(JOURNAL);
        std::fs::rename(stage.path(), &path).unwrap();
        let mut journal: Journal =
            serde_json::from_slice(&std::fs::read(path.join("journal.json")).unwrap()).unwrap();
        for entry in &journal.entries {
            copy_atomic(&path.join(&entry.path), &dir.path().join(&entry.path)).unwrap();
        }
        journal.committed = true;
        write_json(&path.join("journal.json"), &journal).unwrap();
        let committed = snapshot(dir.path());
        document_revision(dir.path()).unwrap();
        assert_eq!(snapshot(dir.path()), committed);
        check_consistent(dir.path(), "committed", 2);
        assert!(!path.exists());
    }

    #[test]
    fn rollback_removes_new_files_for_legacy_document_only_versions() {
        let dir = tempfile::tempdir().unwrap();
        fixture(dir.path(), false);
        let before = snapshot(dir.path());
        let original = std::fs::read(dir.path().join("document.json")).unwrap();
        let mut document = load_document(dir.path()).unwrap();
        document.sections[0].speech[0].text = "new".into();
        let stage = prepare(
            dir.path(),
            &serde_json::to_vec_pretty(&document).unwrap(),
            &document,
            Some(&original),
        )
        .unwrap();
        assert!(
            commit_with_hook(dir.path(), stage, |index| {
                if index == 2 {
                    Err(std::io::Error::other("injected failure").into())
                } else {
                    Ok(())
                }
            })
            .is_err()
        );
        assert_eq!(snapshot(dir.path()), before);
        assert!(!dir.path().join("course.md").exists());
        assert!(!dir.path().join(BACKUP).exists());
    }

    #[test]
    fn unsupported_schema_and_corrupt_manifest_are_not_overwritten() {
        let dir = tempfile::tempdir().unwrap();
        rich_fixture(dir.path());
        let path = dir.path().join("document.json");
        let original = std::fs::read(&path).unwrap();
        let mut document = load_document(dir.path()).unwrap();
        document.schema = 2;
        write_json(&path, &document).unwrap();
        let before = snapshot(dir.path());
        assert!(apply_region_edit(dir.path(), 0, 0, "new", None).is_err());
        assert_eq!(snapshot(dir.path()), before);
        std::fs::write(path, original).unwrap();
        std::fs::write(dir.path().join("manifest.json"), "broken manifest").unwrap();
        let before = snapshot(dir.path());
        assert!(apply_region_edit(dir.path(), 0, 0, "new", None).is_err());
        assert_eq!(snapshot(dir.path()), before);
    }

    #[cfg(unix)]
    #[test]
    fn exports_symlinks_do_not_write_outside_the_version() {
        let dir = tempfile::tempdir().unwrap();
        fixture(dir.path(), false);
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("course.html"), "external file").unwrap();
        std::os::unix::fs::symlink(outside.path(), dir.path().join("exports")).unwrap();
        let original = std::fs::read(dir.path().join("document.json")).unwrap();
        assert!(apply_region_edit(dir.path(), 0, 0, "new", None).is_err());
        assert_eq!(
            std::fs::read(dir.path().join("document.json")).unwrap(),
            original
        );
        assert_eq!(
            std::fs::read_to_string(outside.path().join("course.html")).unwrap(),
            "external file"
        );
    }

    #[test]
    fn normal_read_recovery_does_not_create_lock_or_touch_files() {
        let dir = tempfile::tempdir().unwrap();
        rich_fixture(dir.path());
        let before = snapshot(dir.path());
        recover_pending(dir.path()).unwrap();
        assert!(!dir.path().join(LOCK).exists());
        assert_eq!(snapshot(dir.path()), before);
    }

    #[test]
    fn empty_edit_and_empty_undo_cannot_remove_the_last_readable_passage() {
        let dir = tempfile::tempdir().unwrap();
        rich_fixture(dir.path());
        apply_region_edit(dir.path(), 0, 0, "prior edit", None).unwrap();
        let before = snapshot(dir.path());
        for text in ["", " \n\t"] {
            assert!(apply_region_edit(dir.path(), 0, 0, text, Some("translation alone")).is_err());
            assert_eq!(snapshot(dir.path()), before);
        }
        let mut empty_backup = load_document(dir.path()).unwrap();
        empty_backup.sections[0].speech.clear();
        write_json(&dir.path().join(BACKUP), &empty_backup).unwrap();
        let before = snapshot(dir.path());
        assert!(undo_in_place(dir.path()).is_err());
        assert_eq!(snapshot(dir.path()), before);
        assert!(!dir.path().join(JOURNAL).exists());
    }

    #[test]
    fn empty_edit_is_allowed_when_other_passages_remain_readable() {
        let dir = tempfile::tempdir().unwrap();
        fixture(dir.path(), false);
        let mut document = load_document(dir.path()).unwrap();
        let other = document.sections[0].speech[0].clone();
        document.sections[0].speech.push(other);
        write_json(&dir.path().join("document.json"), &document).unwrap();
        apply_region_edit(dir.path(), 0, 0, "", None).unwrap();
        assert!(crate::artifact::has_readable_body(
            &load_document(dir.path()).unwrap().sections
        ));
    }

    #[test]
    fn shared_read_guards_hold_a_consistent_snapshot_and_exclude_edits() {
        let dir = tempfile::tempdir().unwrap();
        rich_fixture(dir.path());
        let reader = read_guard(dir.path()).unwrap();
        let another_reader = read_guard(dir.path()).unwrap();
        let original = snapshot(dir.path());
        assert!(apply_region_edit(dir.path(), 0, 0, "blocked", None).is_err());
        assert!(undo_in_place(dir.path()).is_err());
        assert_eq!(snapshot(dir.path()), original);
        drop(reader);
        assert!(apply_region_edit(dir.path(), 0, 0, "still blocked", None).is_err());
        drop(another_reader);
        apply_region_edit(dir.path(), 0, 0, "allowed", None).unwrap();
        let writer = begin(dir.path()).unwrap();
        assert!(read_guard(dir.path()).is_err());
        drop(writer);
        assert!(read_guard(dir.path()).is_ok());
    }

    #[test]
    fn read_guard_recovers_then_keeps_the_restored_version_locked() {
        let dir = tempfile::tempdir().unwrap();
        rich_fixture(dir.path());
        let original = std::fs::read(dir.path().join("document.json")).unwrap();
        let mut document = load_document(dir.path()).unwrap();
        document.sections[0].speech[0].text = "interrupted".into();
        let stage = prepare(
            dir.path(),
            &serde_json::to_vec_pretty(&document).unwrap(),
            &document,
            Some(&original),
        )
        .unwrap();
        std::fs::rename(stage.path(), dir.path().join(JOURNAL)).unwrap();
        copy_atomic(
            &dir.path().join(JOURNAL).join("document.json"),
            &dir.path().join("document.json"),
        )
        .unwrap();
        let guard = read_guard(dir.path()).unwrap();
        check_consistent(dir.path(), "hello", 1);
        assert!(!dir.path().join(JOURNAL).exists());
        assert!(begin(dir.path()).is_err());
        drop(guard);
        assert!(begin(dir.path()).is_ok());
    }

    #[test]
    fn readonly_lock_fallback_is_limited_to_clean_legacy_versions() {
        let dir = tempfile::tempdir().unwrap();
        rich_fixture(dir.path());
        // Inject the OS open failure so this remains meaningful under root, too.
        for kind in [ErrorKind::PermissionDenied, ErrorKind::ReadOnlyFilesystem] {
            assert!(
                open_reader_lock(dir.path(), || Err(std::io::Error::from(kind)))
                    .unwrap()
                    .is_none()
            );
        }
        assert!(!dir.path().join(LOCK).exists());
        std::fs::create_dir(dir.path().join(JOURNAL)).unwrap();
        assert!(
            open_reader_lock(dir.path(), || Err(std::io::Error::from(
                ErrorKind::ReadOnlyFilesystem
            )))
            .is_err()
        );
        std::fs::remove_dir(dir.path().join(JOURNAL)).unwrap();
        std::fs::write(dir.path().join(LOCK), "").unwrap();
        let file = open_reader_lock(dir.path(), || {
            Err(std::io::Error::from(ErrorKind::PermissionDenied))
        })
        .unwrap()
        .unwrap();
        file.try_lock_shared().unwrap();
        assert!(begin(dir.path()).is_err());
        file.unlock().unwrap();
        assert!(
            open_reader_lock(dir.path(), || Err(std::io::Error::from(ErrorKind::Other))).is_err()
        );
    }

    #[test]
    fn cancelling_first_translation_batch_stops_queued_requests_and_all_writes() {
        let dir = tempfile::tempdir().unwrap();
        fixture(dir.path(), false);
        let mut document = load_document(dir.path()).unwrap();
        document.sections[0].speech = (0..60)
            .map(|index| {
                let mut speech = document.sections[0].speech[0].clone();
                speech.text = format!("passage {index}");
                speech
            })
            .collect();
        write_json(&dir.path().join("document.json"), &document).unwrap();
        let before = snapshot(dir.path());
        let revision = document_revision(dir.path()).unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let port = listener.local_addr().unwrap().port();
        let cancelled = Arc::new(AtomicBool::new(false));
        let server_cancelled = cancelled.clone();
        let stopped = Arc::new(AtomicBool::new(false));
        let server_stopped = stopped.clone();
        let server = std::thread::spawn(move || {
            let mut requests = 0;
            while !server_stopped.load(Ordering::Acquire) {
                let (mut stream, _) = match listener.accept() {
                    Ok(connection) => connection,
                    Err(error) if error.kind() == ErrorKind::WouldBlock => {
                        std::thread::sleep(std::time::Duration::from_millis(1));
                        continue;
                    }
                    Err(error) => panic!("{error}"),
                };
                stream
                    .set_read_timeout(Some(std::time::Duration::from_secs(2)))
                    .unwrap();
                let mut buffer = vec![0u8; 65536];
                let _ = stream.read(&mut buffer);
                requests += 1;
                server_cancelled.store(true, Ordering::Release);
                let content = serde_json::json!({"segments": (0..20).map(|id| serde_json::json!({"id": id, "text": "translated"})).collect::<Vec<_>>()});
                let body =
                    serde_json::json!({"choices": [{"message": {"content": content.to_string()}}]})
                        .to_string();
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = stream.write_all(response.as_bytes());
            }
            requests
        });
        let mut settings = test_settings(port);
        settings.concurrency = 1;
        let result =
            translate_in_place_checked(dir.path(), &settings, "zh-Hans", &revision, &cancelled);
        stopped.store(true, Ordering::Release);
        assert_eq!(
            server.join().unwrap(),
            1,
            "queued batches must not be sent after cancellation"
        );
        assert!(result.is_err());
        assert_eq!(snapshot(dir.path()), before);
        assert!(!dir.path().join(JOURNAL).exists());
    }

    #[test]
    fn alignment_review_survives_edit_and_rollback_and_invalid_reports_preserve_originals() {
        let dir = tempfile::tempdir().unwrap();
        rich_fixture(dir.path());
        let report = serde_json::json!({
            "schema": 1,
            "pages": [{"page": 0, "t": 0.0, "image": "", "uncertain": true}],
            "slides": ["/private/deck.pdf"]
        });
        let report_path = dir.path().join("alignment.json");
        write_json(&report_path, &report).unwrap();
        let original_report = std::fs::read(&report_path).unwrap();
        apply_region_edit(dir.path(), 0, 0, "reviewed edit", None).unwrap();
        let html = std::fs::read_to_string(dir.path().join("exports/course.html")).unwrap();
        assert!(html.contains("class=\"page-review\""));
        let json: serde_json::Value = serde_json::from_slice(
            &std::fs::read(dir.path().join("exports/structured.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(json["sections"][0]["needs_review"], true);
        let mut zip = zip::ZipArchive::new(
            std::fs::File::open(dir.path().join("exports/course.zip")).unwrap(),
        )
        .unwrap();
        let mut portable_report = String::new();
        zip.by_name("alignment.json")
            .unwrap()
            .read_to_string(&mut portable_report)
            .unwrap();
        assert!(!portable_report.contains("private"));
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&portable_report).unwrap()["pages"][0]["uncertain"],
            true
        );
        drop(zip);
        let before = snapshot(dir.path());
        let original = std::fs::read(dir.path().join("document.json")).unwrap();
        let mut document = load_document(dir.path()).unwrap();
        document.sections[0].speech[0].text = "must roll back".into();
        let stage = prepare(
            dir.path(),
            &serde_json::to_vec_pretty(&document).unwrap(),
            &document,
            Some(&original),
        )
        .unwrap();
        assert!(
            commit_with_hook(dir.path(), stage, |index| {
                if index == 4 {
                    anyhow::bail!("injected failure after exports");
                }
                Ok(())
            })
            .is_err()
        );
        assert_eq!(snapshot(dir.path()), before);
        assert_eq!(std::fs::read(&report_path).unwrap(), original_report);
        let mut stale = report;
        stale["pages"][0]["t"] = 0.5.into();
        for invalid in ["broken".to_owned(), stale.to_string()] {
            std::fs::write(&report_path, invalid.as_bytes()).unwrap();
            let error = apply_region_edit(dir.path(), 0, 0, "must not publish", None).unwrap_err();
            assert_eq!(
                error.to_string(),
                crate::tr!("导出失败：PDF 页面核对信息无效或已过期，原笔记未修改。")
            );
            assert_eq!(snapshot(dir.path()), before);
            assert_eq!(std::fs::read(&report_path).unwrap(), invalid.as_bytes());
            assert!(!dir.path().join(JOURNAL).exists());
        }
    }

    #[test]
    fn cancelled_preparation_does_not_publish_or_attempt_image_export() {
        let dir = tempfile::tempdir().unwrap();
        rich_fixture(dir.path());
        let original = std::fs::read(dir.path().join("document.json")).unwrap();
        let mut document = load_document(dir.path()).unwrap();
        document.sections[0].image = "missing.png".into();
        let cancelled = Arc::new(AtomicBool::new(true));
        let control = crate::dispatch::TaskControl::with_cancel_flag(cancelled);
        let before = snapshot(dir.path());
        let error = super::prepare(
            dir.path(),
            &serde_json::to_vec_pretty(&document).unwrap(),
            &document,
            Some(&original),
            &control,
        )
        .unwrap_err();
        assert!(format!("{error:#}").contains(crate::tr!("已取消 AI 编辑")));
        assert_eq!(snapshot(dir.path()), before);
    }
}
