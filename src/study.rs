//! A learning draft beside a published note. Its source snapshot never changes.
use crate::{artifact::Document, dispatch::TaskControl};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    io::{Read, Write},
    path::Path,
};

const FILE: &str = "study.json";
const MAX_BYTES: u64 = 64 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceSpan {
    pub section: usize,
    pub speech: usize,
    pub start: usize,
    pub end: usize,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct OrganizedSection {
    pub text: String,
    #[serde(default)]
    pub review: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StudyAnswer {
    pub id: String,
    pub spans: Vec<SourceSpan>,
    pub question: String,
    pub text: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StudyPoint {
    pub text: String,
    pub sections: Vec<usize>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct StudySummary {
    pub overview: String,
    pub points: Vec<StudyPoint>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StudyNote {
    pub schema: u32,
    pub revision: u64,
    pub source_digest: String,
    pub source: Document,
    #[serde(default)]
    pub organized: BTreeMap<usize, OrganizedSection>,
    #[serde(default)]
    pub answers: Vec<StudyAnswer>,
    #[serde(default)]
    pub summary: Option<StudySummary>,
}

impl StudyNote {
    pub fn from_document(source: Document) -> Result<Self> {
        let source_digest = document_digest(&source)?;
        let note = Self {
            schema: 1,
            revision: 0,
            source_digest,
            source,
            organized: BTreeMap::new(),
            answers: Vec::new(),
            summary: None,
        };
        validate(&note)?;
        Ok(note)
    }

    /// Verbatim quote; never search-and-replace ambiguous repeated words.
    pub fn quote(&self, spans: &[SourceSpan]) -> Result<String> {
        let mut text = String::new();
        self.visit_spans(spans, |quote| {
            if !text.is_empty() {
                text.push_str("\n\n");
            }
            text.push_str(quote);
            Ok(())
        })?;
        Ok(text)
    }

    fn visit_spans(
        &self,
        spans: &[SourceSpan],
        mut visit: impl FnMut(&str) -> Result<()>,
    ) -> Result<()> {
        ensure!(!spans.is_empty(), crate::tr!("请先在原文中选择文字"));
        let mut previous: Option<&SourceSpan> = None;
        for span in spans {
            let original = self
                .source
                .sections
                .get(span.section)
                .and_then(|section| section.speech.get(span.speech))
                .context(crate::tr!("所选原文已失效，请重新选择"))?;
            ensure!(
                span.start < span.end,
                crate::tr!("所选原文已失效，请重新选择")
            );
            let quote = original
                .text
                .get(span.start..span.end)
                .context(crate::tr!("所选原文已失效，请重新选择"))?;
            ensure!(!quote.trim().is_empty(), crate::tr!("请先在原文中选择文字"));
            if let Some(prev) = previous {
                ensure!(
                    (span.section, span.speech) > (prev.section, prev.speech)
                        || ((span.section, span.speech) == (prev.section, prev.speech)
                            && span.start >= prev.end),
                    crate::tr!("所选原文已失效，请重新选择")
                );
            }
            previous = Some(span);
            visit(quote)?;
        }
        Ok(())
    }
}

impl StudyAnswer {
    pub fn new(
        note: &StudyNote,
        spans: Vec<SourceSpan>,
        question: String,
        text: String,
    ) -> Result<Self> {
        note.quote(&spans)?;
        ensure!(
            !question.trim().is_empty() && !text.trim().is_empty(),
            crate::tr!("问题和回答不能为空")
        );
        Ok(Self {
            id: crate::random_b64url(18)?,
            spans,
            question,
            text,
        })
    }
}

fn document_digest(document: &Document) -> Result<String> {
    Ok(crate::execution::digest(&serde_json::to_vec(document)?))
}

fn read_bytes(root: &Path, name: &str) -> Result<Vec<u8>> {
    let path = crate::artifact::safe_asset_path(root, name)?;
    ensure!(path.is_file(), crate::tr!("学习笔记记录无效，原文件已保留"));
    let mut bytes = Vec::new();
    std::fs::File::open(path)?
        .take(MAX_BYTES + 1)
        .read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() as u64 <= MAX_BYTES,
        crate::tr!("学习笔记记录过大，原文件已保留")
    );
    Ok(bytes)
}

fn read_existing(root: &Path) -> Result<Option<StudyNote>> {
    match std::fs::symlink_metadata(root.join(FILE)) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        result => {
            ensure!(
                result?.file_type().is_file(),
                crate::tr!("学习笔记记录无效，原文件已保留")
            );
        }
    }
    let note: StudyNote = serde_json::from_slice(&read_bytes(root, FILE)?)
        .context(crate::tr!("学习笔记记录无效，原文件已保留"))?;
    validate(&note)?;
    Ok(Some(note))
}

/// Loading alone never creates a learning draft or calls an AI service.
pub fn load(version_dir: &Path) -> Result<StudyNote> {
    let _guard = crate::note_edit::read_guard(version_dir)?;
    if let Some(note) = read_existing(version_dir)? {
        return Ok(note);
    }
    StudyNote::from_document(serde_json::from_slice(&read_bytes(
        version_dir,
        "document.json",
    )?)?)
}

/// Compare-and-swap under an OS lock; one atomic file contains both the immutable
/// source and all derived content. No original note or export is overwritten.
pub fn save(
    version_dir: &Path,
    expected_revision: u64,
    updated: &StudyNote,
    control: &TaskControl,
) -> Result<StudyNote> {
    control.check()?;
    validate_controlled(updated, control)?;
    let _source_guard = crate::note_edit::read_guard(version_dir)?;
    let lock_path = version_dir.join(".study.lock");
    match std::fs::symlink_metadata(&lock_path) {
        Ok(metadata) => ensure!(
            metadata.file_type().is_file(),
            crate::tr!("学习笔记记录无效，原文件已保留")
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    let _lock = crate::runtime::lock_file(&lock_path)?;
    let current = match read_existing(version_dir)? {
        Some(note) => note,
        None => StudyNote::from_document(serde_json::from_slice(&read_bytes(
            version_dir,
            "document.json",
        )?)?)?,
    };
    ensure!(
        current.revision == expected_revision
            && updated.revision == expected_revision
            && current.source_digest == updated.source_digest,
        crate::tr!("学习笔记已变化，请重新打开后再试")
    );
    let mut next = updated.clone();
    next.revision = current
        .revision
        .checked_add(1)
        .context(crate::tr!("学习笔记版本无效"))?;
    let bytes = serde_json::to_vec_pretty(&next)?;
    ensure!(
        bytes.len() as u64 <= MAX_BYTES,
        crate::tr!("学习笔记记录过大，原文件已保留")
    );
    control.check()?;
    let staged = stage_draft(version_dir, &bytes, control)
        .context(crate::tr!("学习笔记尚未保存，请重试"))?;
    commit_draft(staged, &version_dir.join(FILE), control)
        .context(crate::tr!("学习笔记尚未保存，请重试"))?;
    Ok(next)
}

fn stage_draft(
    root: &Path,
    bytes: &[u8],
    control: &TaskControl,
) -> Result<tempfile::NamedTempFile> {
    control.check()?;
    let mut staged = tempfile::NamedTempFile::new_in(root)?;
    for chunk in bytes.chunks(64 * 1024) {
        control.check()?;
        staged.write_all(chunk)?;
    }
    crate::checkpoint::make_shared(staged.as_file())?;
    staged.as_file().sync_all()?;
    control.check()?;
    Ok(staged)
}

fn commit_draft(
    staged: tempfile::NamedTempFile,
    destination: &Path,
    control: &TaskControl,
) -> Result<()> {
    // This is the last cancellable boundary. A completed rename is a committed
    // save even if cancellation arrives immediately afterwards.
    control.check()?;
    staged.persist(destination)?;
    #[cfg(unix)]
    if let Some(parent) = destination.parent()
        && let Ok(directory) = std::fs::File::open(parent)
    {
        // Best effort durability for filesystems supporting directory fsync.
        // A sync failure after rename must not misreport the committed save as
        // unsaved or encourage a conflicting retry.
        let _ = directory.sync_all();
    }
    Ok(())
}

fn validate(note: &StudyNote) -> Result<()> {
    validate_controlled(note, &TaskControl::default())
}

fn validate_controlled(note: &StudyNote, control: &TaskControl) -> Result<()> {
    control.check()?;
    ensure!(
        note.schema == 1
            && note.source.schema == 1
            && document_digest(&note.source)? == note.source_digest,
        crate::tr!("学习笔记记录无效，原文件已保留")
    );
    ensure!(
        crate::artifact::has_readable_body(&note.source.sections),
        crate::tr!("笔记没有可读正文 / Note has no readable body")
    );
    for (index, section) in &note.organized {
        control.check()?;
        ensure!(
            *index < note.source.sections.len() && !section.text.trim().is_empty(),
            crate::tr!("学习笔记记录无效，原文件已保留")
        );
    }
    let mut ids = std::collections::HashSet::new();
    let mut referenced_bytes = 0u64;
    for answer in &note.answers {
        control.check()?;
        // Validate borrowed slices, never allocate a complete quote per answer.
        // Bound cumulative references as well as serialized file size.
        for span in &answer.spans {
            control.check()?;
            referenced_bytes =
                referenced_bytes.saturating_add(span.end.saturating_sub(span.start) as u64);
            ensure!(
                referenced_bytes <= MAX_BYTES,
                crate::tr!("学习笔记记录过大，原文件已保留")
            );
        }
        note.visit_spans(&answer.spans, |_| control.check())?;
        ensure!(
            !answer.id.is_empty()
                && ids.insert(&answer.id)
                && !answer.question.trim().is_empty()
                && !answer.text.trim().is_empty(),
            crate::tr!("学习笔记记录无效，原文件已保留")
        );
    }
    if let Some(summary) = &note.summary {
        ensure!(
            !summary.overview.trim().is_empty(),
            crate::tr!("学习笔记记录无效，原文件已保留")
        );
        for point in &summary.points {
            control.check()?;
            ensure!(
                !point.text.trim().is_empty()
                    && !point.sections.is_empty()
                    && point
                        .sections
                        .iter()
                        .all(|index| *index < note.source.sections.len()),
                crate::tr!("学习笔记记录无效，原文件已保留")
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, atomic::AtomicBool};

    fn document() -> Document {
        serde_json::from_value(serde_json::json!({
            "schema": 1,
            "meta": {"title":"Lesson", "webpage_url":"", "duration":20},
            "sections": [{"t":0,"end":20,"image":"images/one.png","speech":[
                {"start":1,"end":8,"text":"嗯，条件 x > 0 时，不删除关键部分。"},
                {"start":9,"end":18,"text":"repeat repeat"}
            ]}], "summary": null
        }))
        .unwrap()
    }

    fn fixture() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("document.json"),
            serde_json::to_vec(&document()).unwrap(),
        )
        .unwrap();
        std::fs::write(dir.path().join("course.md"), "original markdown").unwrap();
        std::fs::write(dir.path().join("manifest.json"), "original manifest").unwrap();
        dir
    }

    fn span(start: usize, end: usize) -> SourceSpan {
        SourceSpan {
            section: 0,
            speech: 1,
            start,
            end,
        }
    }

    fn organized(note: &StudyNote) -> StudyNote {
        let mut updated = note.clone();
        updated.organized.insert(
            0,
            OrganizedSection {
                text: "条件 x > 0 时，不删除关键部分。".into(),
                review: vec![],
            },
        );
        updated
    }

    #[test]
    fn loading_does_not_create_a_draft_and_saving_keeps_all_published_files() {
        let dir = fixture();
        let files = ["document.json", "course.md", "manifest.json"];
        let before: Vec<_> = files
            .iter()
            .map(|name| std::fs::read(dir.path().join(name)).unwrap())
            .collect();
        let note = load(dir.path()).unwrap();
        assert_eq!(note.revision, 0);
        assert!(!dir.path().join(FILE).exists());
        let saved = save(dir.path(), 0, &organized(&note), &TaskControl::default()).unwrap();
        assert_eq!(saved.revision, 1);
        assert_eq!(
            load(dir.path()).unwrap().organized[&0].text,
            saved.organized[&0].text
        );
        for (name, bytes) in files.iter().zip(before) {
            assert_eq!(std::fs::read(dir.path().join(name)).unwrap(), bytes);
        }
    }

    #[test]
    fn concurrent_first_save_and_stale_updates_are_rejected() {
        let dir = fixture();
        let a = load(dir.path()).unwrap();
        let b = load(dir.path()).unwrap();
        let saved = save(dir.path(), 0, &organized(&a), &TaskControl::default()).unwrap();
        let bytes = std::fs::read(dir.path().join(FILE)).unwrap();
        assert!(
            save(
                dir.path(),
                b.revision,
                &organized(&b),
                &TaskControl::default()
            )
            .is_err()
        );
        assert_eq!(std::fs::read(dir.path().join(FILE)).unwrap(), bytes);
        let saved_again =
            save(dir.path(), saved.revision, &saved, &TaskControl::default()).unwrap();
        assert_eq!(saved_again.revision, 2);
        assert!(save(dir.path(), saved.revision, &saved, &TaskControl::default()).is_err());
    }

    #[test]
    fn changed_source_before_first_save_is_rejected_but_persisted_snapshot_is_stable() {
        let dir = fixture();
        let original = load(dir.path()).unwrap();
        let mut changed = document();
        changed.sections[0].speech[0].text = "different lesson".into();
        std::fs::write(
            dir.path().join("document.json"),
            serde_json::to_vec(&changed).unwrap(),
        )
        .unwrap();
        assert!(
            save(
                dir.path(),
                0,
                &organized(&original),
                &TaskControl::default()
            )
            .is_err()
        );
        assert!(!dir.path().join(FILE).exists());
        let current = load(dir.path()).unwrap();
        save(dir.path(), 0, &organized(&current), &TaskControl::default()).unwrap();
        std::fs::write(dir.path().join("document.json"), b"unavailable").unwrap();
        assert_eq!(
            load(dir.path()).unwrap().source.sections[0].speech[0].text,
            "different lesson"
        );
    }

    #[test]
    fn source_cannot_be_replaced_even_with_a_recomputed_digest() {
        let dir = fixture();
        let note = save(
            dir.path(),
            0,
            &load(dir.path()).unwrap(),
            &TaskControl::default(),
        )
        .unwrap();
        let bytes = std::fs::read(dir.path().join(FILE)).unwrap();
        let mut altered = note.clone();
        altered.source.sections[0].speech[0].text = "changed".into();
        assert!(save(dir.path(), 1, &altered, &TaskControl::default()).is_err());
        altered.source_digest = document_digest(&altered.source).unwrap();
        assert!(save(dir.path(), 1, &altered, &TaskControl::default()).is_err());
        assert_eq!(std::fs::read(dir.path().join(FILE)).unwrap(), bytes);
    }

    #[test]
    fn citations_use_utf8_offsets_and_disambiguate_repeated_text() {
        let note = StudyNote::from_document(document()).unwrap();
        assert_eq!(note.quote(&[span(7, 13)]).unwrap(), "repeat");
        assert_eq!(
            note.quote(&[span(0, 6), span(7, 13)]).unwrap(),
            "repeat\n\nrepeat"
        );
        let valid = SourceSpan {
            section: 0,
            speech: 0,
            start: 0,
            end: 3,
        };
        assert_eq!(note.quote(std::slice::from_ref(&valid)).unwrap(), "嗯");
        assert!(note.quote(&[SourceSpan { start: 1, ..valid }]).is_err());
        assert!(note.quote(&[span(7, 13), span(0, 6)]).is_err());
        assert!(note.quote(&[span(0, 8), span(7, 13)]).is_err());
        assert!(note.quote(&[span(0, 99)]).is_err());
        assert!(note.quote(&[span(0, 0)]).is_err());
        assert!(note.quote(&[span(6, 7)]).is_err());
        assert!(note.quote(&[]).is_err());
    }

    #[test]
    fn reorganizing_keeps_saved_answers_and_their_exact_sources() {
        let dir = fixture();
        let mut note = load(dir.path()).unwrap();
        note.answers.push(
            StudyAnswer::new(
                &note,
                vec![span(7, 13)],
                "Why?".into(),
                "A grounded explanation".into(),
            )
            .unwrap(),
        );
        let saved = save(dir.path(), 0, &note, &TaskControl::default()).unwrap();
        let updated = organized(&saved);
        save(dir.path(), 1, &updated, &TaskControl::default()).unwrap();
        let loaded = load(dir.path()).unwrap();
        assert_eq!(loaded.answers.len(), 1);
        assert_eq!(loaded.answers[0].id, note.answers[0].id);
        assert_eq!(loaded.quote(&loaded.answers[0].spans).unwrap(), "repeat");
    }

    #[test]
    fn malformed_or_unknown_schema_drafts_are_never_overwritten() {
        for bytes in [b"{".as_slice(), b"{\"schema\":9}"] {
            let dir = fixture();
            let note = load(dir.path()).unwrap();
            std::fs::write(dir.path().join(FILE), bytes).unwrap();
            assert!(load(dir.path()).is_err());
            assert!(save(dir.path(), 0, &note, &TaskControl::default()).is_err());
            assert_eq!(std::fs::read(dir.path().join(FILE)).unwrap(), bytes);
        }
    }

    #[test]
    fn invalid_derived_content_cannot_replace_a_saved_draft() {
        let dir = fixture();
        let note = save(
            dir.path(),
            0,
            &load(dir.path()).unwrap(),
            &TaskControl::default(),
        )
        .unwrap();
        let before = std::fs::read(dir.path().join(FILE)).unwrap();
        let mut bad_index = note.clone();
        bad_index.organized.insert(
            1,
            OrganizedSection {
                text: "outside".into(),
                review: vec![],
            },
        );
        let mut bad_summary = note.clone();
        bad_summary.summary = Some(StudySummary {
            overview: "summary".into(),
            points: vec![StudyPoint {
                text: "claim".into(),
                sections: vec![2],
            }],
        });
        let mut duplicate_answer = note.clone();
        let answer =
            StudyAnswer::new(&note, vec![span(0, 6)], "Why?".into(), "Because".into()).unwrap();
        duplicate_answer.answers = vec![answer.clone(), answer];
        for invalid in [bad_index, bad_summary, duplicate_answer] {
            assert!(save(dir.path(), 1, &invalid, &TaskControl::default()).is_err());
            assert_eq!(std::fs::read(dir.path().join(FILE)).unwrap(), before);
        }
    }

    #[test]
    fn cancellation_prevents_any_draft_or_lock_creation() {
        let dir = fixture();
        let note = StudyNote::from_document(document()).unwrap();
        let control = TaskControl::with_cancel_flag(Arc::new(AtomicBool::new(true)));
        assert!(save(dir.path(), 0, &note, &control).is_err());
        for name in [FILE, ".study.lock", ".note-edit.lock"] {
            assert!(!dir.path().join(name).exists());
        }
    }

    #[test]
    fn an_existing_writer_blocks_a_save_without_changing_the_draft() {
        let dir = fixture();
        let note = load(dir.path()).unwrap();
        let lock = crate::runtime::lock_file(&dir.path().join(".study.lock")).unwrap();
        assert!(save(dir.path(), 0, &note, &TaskControl::default()).is_err());
        assert!(!dir.path().join(FILE).exists());
        drop(lock);
        save(dir.path(), 0, &note, &TaskControl::default()).unwrap();
    }

    #[test]
    fn current_revision_cannot_be_paired_with_a_stale_candidate() {
        let dir = fixture();
        let stale = load(dir.path()).unwrap();
        save(dir.path(), 0, &organized(&stale), &TaskControl::default()).unwrap();
        let before = std::fs::read(dir.path().join(FILE)).unwrap();
        assert!(save(dir.path(), 1, &stale, &TaskControl::default()).is_err());
        assert_eq!(std::fs::read(dir.path().join(FILE)).unwrap(), before);
    }

    #[test]
    fn cancelling_after_staging_preserves_previous_draft_and_removes_temporary_file() {
        let dir = fixture();
        let original = b"previous draft";
        std::fs::write(dir.path().join(FILE), original).unwrap();
        let cancelled = Arc::new(AtomicBool::new(false));
        let control = TaskControl::with_cancel_flag(cancelled.clone());
        let staged = stage_draft(dir.path(), &vec![b'x'; 128 * 1024], &control).unwrap();
        let temporary = staged.path().to_path_buf();
        assert!(temporary.exists());
        cancelled.store(true, std::sync::atomic::Ordering::Release);
        assert!(commit_draft(staged, &dir.path().join(FILE), &control).is_err());
        assert!(!temporary.exists());
        assert_eq!(std::fs::read(dir.path().join(FILE)).unwrap(), original);
    }

    #[test]
    fn cumulative_quote_references_are_bounded_without_expanding_each_answer() {
        let mut source = document();
        source.sections[0].speech[0].text = "x".repeat(1024 * 1024);
        let mut note = StudyNote::from_document(source).unwrap();
        for index in 0..65 {
            note.answers.push(StudyAnswer {
                id: index.to_string(),
                spans: vec![SourceSpan {
                    section: 0,
                    speech: 0,
                    start: 0,
                    end: 1024 * 1024,
                }],
                question: "Why?".into(),
                text: "An explanation".into(),
            });
        }
        assert!(serde_json::to_vec(&note).unwrap().len() < 2 * 1024 * 1024);
        assert!(validate(&note).is_err());
        note.answers.pop();
        validate(&note).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn read_only_version_can_be_read_and_failed_save_keeps_previous_content() {
        use std::os::unix::fs::PermissionsExt;
        let dir = fixture();
        let note = load(dir.path()).unwrap();
        let saved = save(dir.path(), 0, &organized(&note), &TaskControl::default()).unwrap();
        let before = std::fs::read(dir.path().join(FILE)).unwrap();
        let permissions = std::fs::metadata(dir.path()).unwrap().permissions();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o555)).unwrap();
        let read = load(dir.path());
        let write = save(dir.path(), 1, &saved, &TaskControl::default());
        std::fs::set_permissions(dir.path(), permissions).unwrap();
        assert_eq!(read.unwrap().revision, 1);
        // Root bypasses Unix permissions, including on some CI containers.
        if !std::process::Command::new("id")
            .arg("-u")
            .output()
            .unwrap()
            .stdout
            .starts_with(b"0\n")
        {
            assert!(write.is_err());
            assert_eq!(std::fs::read(dir.path().join(FILE)).unwrap(), before);
            let retry = save(dir.path(), 1, &saved, &TaskControl::default()).unwrap();
            assert_eq!(retry.revision, 2);
        }
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_records_and_locks_are_rejected_without_touching_targets() {
        for name in [FILE, ".study.lock"] {
            let dir = fixture();
            let note = load(dir.path()).unwrap();
            let target = dir.path().join("preserve");
            std::fs::write(&target, b"keep").unwrap();
            std::os::unix::fs::symlink(&target, dir.path().join(name)).unwrap();
            assert!(save(dir.path(), 0, &note, &TaskControl::default()).is_err());
            assert_eq!(std::fs::read(&target).unwrap(), b"keep");
            assert!(
                std::fs::symlink_metadata(dir.path().join(name))
                    .unwrap()
                    .file_type()
                    .is_symlink()
            );
        }
    }
}
