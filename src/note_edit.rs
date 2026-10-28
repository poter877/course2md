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
