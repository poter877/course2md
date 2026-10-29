//! A learning draft beside an immutable source, with explicit candidate approval.
use super::*;
use crate::{note_text::NoteText, theme::*};
use course2md::study::{OrganizedSection, SourceSpan, StudyAnswer, StudyNote, StudySummary};
use std::{
    cell::RefCell,
    ops::Range,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

#[derive(Clone, Copy, Default, PartialEq, Eq)]
enum Column {
    #[default]
    Source,
    Draft,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Work {
    Load,
    Ai,
    Save,
}

#[derive(Clone)]
struct Request {
    id: u64,
    version: PathBuf,
    kind: Work,
    cancelled: Arc<AtomicBool>,
}

#[derive(Clone)]
enum Candidate {
    Organized(BTreeMap<usize, OrganizedSection>),
    Answer(StudyAnswer),
    Summary(StudySummary),
}

impl Candidate {
    fn apply_to(&self, note: &StudyNote) -> StudyNote {
        let mut updated = note.clone();
        match self {
            Self::Organized(sections) => updated.organized.extend(sections.clone()),
            Self::Answer(answer) => updated.answers.push(answer.clone()),
            Self::Summary(summary) => updated.summary = Some(summary.clone()),
        }
        updated
    }
}

enum Operation {
    Organize(Vec<usize>),
    Ask(Vec<SourceSpan>, String),
    Summarize,
}

enum Update {
    Section(usize, OrganizedSection, usize, usize),
    Candidate(Candidate),
}

pub(crate) struct StudyState {
    pub(crate) enabled: bool,
    version: Option<PathBuf>,
    serial: u64,
    pending: Option<Request>,
    note: Option<Rc<StudyNote>>,
    images: Vec<Option<PathBuf>>,
    candidate: Option<Candidate>,
    selected: StudySelection,
    question_spans: Vec<SourceSpan>,
    question_expanded: bool,
    question_origin: Option<String>,
    source_marks: Vec<SourceSpan>,
    source_target: Option<SourceSpan>,
    source_position: Rc<RefCell<Option<(SourceSpan, Pixels)>>>,
    organized_progress: Option<(usize, usize)>,
    question: Entity<InputState>,
    _question_subscription: Subscription,
    error: Option<String>,
    status: Option<String>,
    column: Column,
    list: ListState,
    controls_scroll: ScrollHandle,
    layout: Option<(bool, u32)>,
}

impl StudyState {
    pub(crate) fn new(window: &mut Window, cx: &mut Context<Desktop>) -> Self {
        let question = cx.new(|cx| InputState::new(window, cx));
        let question_subscription = cx.subscribe_in(&question, window, |this, _, event, _, cx| {
            if this.study_ui.enabled && matches!(event, InputEvent::PressEnter { .. }) {
                this.study_ask(cx);
            }
        });
        Self {
            enabled: false,
            version: None,
            serial: 0,
            pending: None,
            note: None,
            images: Vec::new(),
            candidate: None,
            selected: StudySelection::default(),
            question_spans: Vec::new(),
            question_expanded: false,
            question_origin: None,
            source_marks: Vec::new(),
            source_target: None,
            source_position: Rc::default(),
            organized_progress: None,
            question,
            _question_subscription: question_subscription,
            error: None,
            status: None,
            column: Column::Source,
            list: ListState::new(0, ListAlignment::Top, px(800.)),
            controls_scroll: ScrollHandle::new(),
            layout: None,
        }
    }

    fn show_error(&mut self, message: impl Into<String>) {
        self.error = Some(message.into());
        // Controls begin with the toolbar and source-preservation note. Bring
        // newly inserted feedback into view even after scrolling to a question.
        self.controls_scroll.scroll_to_top_of_item(2);
    }

    fn begin(&mut self, kind: Work) -> Request {
        self.serial = self.serial.wrapping_add(1);
        let request = Request {
            id: self.serial,
            version: self.version.clone().expect("study version"),
            kind,
            cancelled: Arc::new(AtomicBool::new(false)),
        };
        self.pending = Some(request.clone());
        self.error = None;
        self.status = None;
        request
    }

    fn accepts(&self, request: &Request) -> bool {
        request.is_current(self.version.as_deref(), self.pending.as_ref())
    }

    fn cancel_ai(&mut self) {
        if self
            .pending
            .as_ref()
            .is_some_and(|pending| pending.kind == Work::Ai)
        {
            if let Some(pending) = self.pending.take() {
                pending.cancelled.store(true, Ordering::Release);
            }
            self.status = Some(course2md::tr!("已取消").into());
        }
    }

    fn redraw_rows(&mut self) {
        let top = self.list.logical_scroll_top();
        let count = self
            .note
            .as_ref()
            .map_or(0, |note| note.source.sections.len() + 1);
        self.list.reset(count);
        if count > 0 {
            self.list.scroll_to(ListOffset {
                item_ix: top.item_ix.min(count - 1),
                offset_in_item: top.offset_in_item,
            });
        }
    }
}

fn prepare_text(note: &StudyNote) {
    crate::math::prepare(
        note.source
            .sections
            .iter()
            .flat_map(|section| section.speech.iter())
            .flat_map(|speech| {
                std::iter::once(speech.text.as_str()).chain(speech.translation.as_deref())
            }),
    );
    crate::math::prepare(note.organized.values().map(|section| section.text.as_str()));
    crate::math::prepare(note.answers.iter().map(|answer| answer.text.as_str()));
    if let Some(summary) = &note.source.summary {
        crate::math::prepare(
            std::iter::once(summary.tldr.as_str())
                .chain(summary.key_points.iter().map(String::as_str))
                .chain(
                    summary
                        .outline
                        .iter()
                        .flat_map(|item| [item.title.as_str(), item.detail.as_str()]),
                ),
        );
    }
    if let Some(summary) = &note.summary {
        prepare_summary(summary);
    }
}

fn prepare_summary(summary: &StudySummary) {
    crate::math::prepare(
        std::iter::once(summary.overview.as_str())
            .chain(summary.points.iter().map(|point| point.text.as_str())),
    );
}

impl Desktop {
    pub(crate) fn suspend_study(&mut self) {
        self.study_ui.enabled = false;
        self.study_ui.cancel_ai();
        self.study_ui.selected.clear();
        self.study_ui.selected.pressed = None;
        self.study_ui.source_marks.clear();
        self.study_ui.source_target = None;
        self.study_ui.source_position.borrow_mut().take();
    }

    pub(crate) fn open_study(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(version) = self
            .preview
            .as_ref()
            .map(|preview| preview.course.dir.clone())
        else {
            return;
        };
        self.reader_stop_ai_for_study();
        if self.study_ui.version.as_ref() != Some(&version) {
            if let Some(pending) = &self.study_ui.pending {
                pending.cancelled.store(true, Ordering::Release);
            }
            self.study_ui.serial = self.study_ui.serial.wrapping_add(1);
            self.study_ui.version = Some(version.clone());
            self.study_ui.pending = None;
            self.study_ui.note = None;
            self.study_ui.images.clear();
            self.study_ui.candidate = None;
            self.study_ui.organized_progress = None;
            self.study_ui.selected.clear();
            self.study_ui.question_spans.clear();
            self.study_ui.question_origin = None;
            self.study_ui.error = None;
            self.study_ui.status = None;
            self.study_ui.column = Column::Source;
            self.study_ui
                .controls_scroll
                .set_offset(point(px(0.), px(0.)));
            self.study_ui.layout = None;
            self.study_ui
                .question
                .update(cx, |input, cx| input.set_value("", window, cx));
        }
        self.study_ui.enabled = true;
        if self.study_ui.candidate.is_none() && self.study_ui.pending.is_none() {
            self.load_study(cx);
        }
        cx.notify();
    }

    pub(crate) fn ask_study_from_reader(
        &mut self,
        spans: Vec<SourceSpan>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.page != Page::Result || self.reading {
            return;
        }
        let Some(preview) = self.preview.as_ref() else {
            return;
        };
        let snapshot = match StudyNote::from_document(preview.document.clone()) {
            Ok(note) if note.quote(&spans).is_ok() => note,
            _ => {
                self.message = Some(course2md::tr!("笔记已发生变化，请重新选择文字后再试").into());
                cx.notify();
                return;
            }
        };
        self.open_study(window, cx);
        if self
            .study_ui
            .note
            .as_ref()
            .is_some_and(|note| note.source_digest != snapshot.source_digest)
        {
            self.study_ui
                .show_error(course2md::tr!("笔记已发生变化，请重新选择文字后再试"));
            cx.notify();
            return;
        }
        self.study_ui.question_spans = spans;
        self.study_ui.question_origin = Some(snapshot.source_digest);
        self.study_ui.question_expanded = true;
        self.study_ui.controls_scroll.scroll_to_bottom();
        self.study_ui
            .question
            .update(cx, |input, cx| input.focus(window, cx));
        cx.notify();
    }

    fn load_study(&mut self, cx: &mut Context<Self>) {
        if self.study_ui.pending.is_some() || self.study_ui.candidate.is_some() {
            return;
        }
        self.study_ui.note = None;
        self.study_ui.images.clear();
        self.study_ui.selected.clear();
        self.study_ui.selected.pressed = None;
        self.study_ui.source_marks.clear();
        self.study_ui.source_target = None;
        self.study_ui.source_position.borrow_mut().take();
        self.study_ui.organized_progress = None;
        let request = self.study_ui.begin(Work::Load);
        let version = request.version.clone();
        let worker = crate::spawn_blocking_io(move || -> Result<_> {
            let note = course2md::study::load(&version)?;
            let _guard = course2md::note_edit::read_guard(&version)?;
            let images = note
                .source
                .sections
                .iter()
                .map(|section| {
                    (!section.image.is_empty())
                        .then(|| {
                            course2md::artifact::safe_asset_path(&version, &section.image).ok()
                        })
                        .flatten()
                })
                .collect();
            prepare_text(&note);
            Ok((note, images))
        });
        cx.spawn(async move |this, cx| {
            let result = worker
                .recv()
                .await
                .unwrap_or_else(|_| Err(anyhow::anyhow!(course2md::tr!("打开时意外中断，请重试"))));
            let _ = this.update(cx, |this, cx| {
                if !this.study_ui.accepts(&request) {
                    return;
                }
                this.study_ui.pending = None;
                match result {
                    Ok((note, images)) => {
                        if !question_still_valid(
                            this.study_ui.question_origin.as_deref(),
                            &this.study_ui.question_spans,
                            &note,
                        ) {
                            if !this.study_ui.question_spans.is_empty() {
                                this.study_ui.show_error(course2md::tr!(
                                    "笔记已发生变化，请重新选择文字后再试"
                                ));
                            }
                            this.study_ui.question_spans.clear();
                            this.study_ui.question_origin = None;
                        }
                        this.study_ui.note = Some(Rc::new(note));
                        this.study_ui.images = images;
                        this.study_ui.redraw_rows();
                    }
                    Err(error) => this.study_ui.show_error(format!("{error:#}")),
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn run_study_ai(&mut self, operation: Operation, cx: &mut Context<Self>) {
        if self.study_ui.pending.is_some() || self.study_ui.candidate.is_some() {
            return;
        }
        let Some(note) = self.study_ui.note.as_ref().map(|note| (**note).clone()) else {
            return;
        };
        let refs = self.preferences.default_refs();
        let setup = (|| -> Result<_> {
            anyhow::ensure!(
                self.preferences.generation().ai_allowed,
                course2md::tr!("已在设置中关闭 AI 处理：不运行校对、摘要、截图辅助与翻译。")
            );
            anyhow::ensure!(
                refs.llm.is_some(),
                course2md::tr!("先在 设置 → 服务 里添加 AI 服务并设为默认")
            );
            let mut base = course2md::settings::ConfigFile::default();
            base.llm.enabled = true;
            Ok((
                self.preferences.config_for_refs(&base, &refs)?.llm,
                self.preferences.credential_references_for(&base, &refs)?,
            ))
        })();
        let (mut settings, needed) = match setup {
            Ok(value) => value,
            Err(error) => {
                self.study_ui.show_error(format!("{error:#}"));
                cx.notify();
                return;
            }
        };
        self.study_ui.organized_progress = match &operation {
            Operation::Organize(sections) => Some((0, sections.len())),
            _ => None,
        };
        let request = self.study_ui.begin(Work::Ai);
        self.study_ui.controls_scroll.scroll_to_top_of_item(2);
        let version = request.version.clone();
        let cancelled = request.cancelled.clone();
        let vault = self.preferences.vault();
        let (tx, rx) = smol::channel::unbounded();
        let worker = crate::spawn_blocking_io(move || -> Result<()> {
            let control = course2md::dispatch::TaskControl::with_cancel_flag(cancelled);
            control.check()?;
            for reference in needed {
                settings.api_key = vault.resolve(&reference)?.expose().to_owned();
            }
            control.check()?;
            match operation {
                Operation::Organize(sections) => {
                    let total = sections.len();
                    for (ordinal, section) in sections.into_iter().enumerate() {
                        control.check()?;
                        let organized = course2md::study_ai::organize(
                            &version, &note, section, &settings, &control,
                        )?;
                        control.check()?;
                        crate::math::prepare(std::iter::once(organized.text.as_str()));
                        let _ = tx.send_blocking(Update::Section(
                            section,
                            organized,
                            ordinal + 1,
                            total,
                        ));
                    }
                }
                Operation::Ask(spans, question) => {
                    let text = course2md::study_ai::ask(
                        &version, &note, &spans, &question, &settings, &control,
                    )?;
                    control.check()?;
                    crate::math::prepare(std::iter::once(text.as_str()));
                    let answer = StudyAnswer::new(&note, spans, question, text)?;
                    let _ = tx.send_blocking(Update::Candidate(Candidate::Answer(answer)));
                }
                Operation::Summarize => {
                    let summary =
                        course2md::study_ai::summarize(&version, &note, &settings, &control)?;
                    control.check()?;
                    prepare_summary(&summary);
                    let _ = tx.send_blocking(Update::Candidate(Candidate::Summary(summary)));
                }
            }
            Ok(())
        });
        cx.spawn(async move |this, cx| {
            while let Ok(update) = rx.recv().await {
                let _ = this.update(cx, |this, cx| {
                    if !this.study_ui.accepts(&request) {
                        return;
                    }
                    match update {
                        Update::Section(index, section, done, total) => {
                            let candidate = this
                                .study_ui
                                .candidate
                                .get_or_insert_with(|| Candidate::Organized(BTreeMap::new()));
                            if let Candidate::Organized(sections) = candidate {
                                sections.insert(index, section);
                            }
                            this.study_ui.organized_progress = Some((done, total));
                            this.study_ui.status =
                                Some(course2md::tr!("正在整理章节 {}/{}", done, total));
                        }
                        Update::Candidate(candidate) => {
                            let row = candidate_row(&candidate);
                            this.study_ui.candidate = Some(candidate);
                            if this.study_ui.enabled {
                                this.study_ui.column = Column::Draft;
                                this.study_ui.question_spans.clear();
                                this.study_ui.redraw_rows();
                                this.study_ui.list.scroll_to(ListOffset {
                                    item_ix: row,
                                    offset_in_item: px(0.),
                                });
                            }
                        }
                    }
                    this.study_ui.redraw_rows();
                    cx.notify();
                });
            }
            let result = worker
                .recv()
                .await
                .unwrap_or_else(|_| Err(anyhow::anyhow!(course2md::tr!("处理意外中断，请重试"))));
            let _ = this.update(cx, |this, cx| {
                if !this.study_ui.accepts(&request) {
                    return;
                }
                this.study_ui.pending = None;
                this.study_ui.status = None;
                if let Err(error) = result {
                    this.study_ui.show_error(format!("{error:#}"));
                } else if this.study_ui.candidate.is_some() {
                    let feedback = if this.study_ui.organized_progress.is_some() {
                        3
                    } else {
                        2
                    };
                    this.study_ui
                        .controls_scroll
                        .scroll_to_top_of_item(feedback);
                }
                this.study_ui.redraw_rows();
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn save_study_candidate(&mut self, cx: &mut Context<Self>) {
        if self.study_ui.pending.is_some() {
            return;
        }
        let (Some(note), Some(candidate)) = (&self.study_ui.note, &self.study_ui.candidate) else {
            return;
        };
        let revision = note.revision;
        let updated = candidate.apply_to(note);
        let request = self.study_ui.begin(Work::Save);
        let version = request.version.clone();
        let control = course2md::dispatch::TaskControl::with_cancel_flag(request.cancelled.clone());
        let worker = crate::spawn_blocking_io(move || {
            course2md::study::save(&version, revision, &updated, &control)
        });
        cx.spawn(async move |this, cx| {
            let result = worker.recv().await.unwrap_or_else(|_| {
                Err(anyhow::anyhow!(course2md::tr!("学习笔记尚未保存，请重试")))
            });
            let _ = this.update(cx, |this, cx| {
                if !this.study_ui.accepts(&request) {
                    return;
                }
                this.study_ui.pending = None;
                match accept_saved_note(
                    &mut this.study_ui.note,
                    &mut this.study_ui.candidate,
                    result,
                ) {
                    Ok(()) => {
                        this.study_ui.organized_progress = None;
                        this.study_ui.status = Some(course2md::tr!("已保存").into());
                    }
                    Err(error) => this.study_ui.show_error(format!("{error:#}")),
                }
                this.study_ui.redraw_rows();
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn study_quote_selected(
        &mut self,
        spans: Vec<SourceSpan>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(note) = &self.study_ui.note else {
            return;
        };
        match note.quote(&spans) {
            Ok(_) => {
                self.study_ui.question_spans = spans;
                self.study_ui.question_origin = Some(note.source_digest.clone());
                self.study_ui.question_expanded = true;
                self.study_ui.controls_scroll.scroll_to_bottom();
                self.study_ui.error = None;
                self.study_ui
                    .question
                    .update(cx, |input, cx| input.focus(window, cx));
            }
            Err(error) => self.study_ui.show_error(format!("{error:#}")),
        }
        cx.notify();
    }

    fn study_ask(&mut self, cx: &mut Context<Self>) {
        let question = self.study_ui.question.read(cx).value().trim().to_owned();
        if question.is_empty() {
            self.study_ui.show_error(course2md::tr!("请输入问题"));
            cx.notify();
            return;
        }
        let spans = self.study_ui.question_spans.clone();
        if let Some(note) = &self.study_ui.note
            && let Err(error) = note.quote(&spans)
        {
            self.study_ui.show_error(format!("{error:#}"));
            cx.notify();
            return;
        }
        self.run_study_ai(Operation::Ask(spans, question), cx);
        if self.study_ui.pending.is_some() {
            self.study_ui.question_expanded = false;
        }
    }

    pub(crate) fn study_page(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let width = f32::from(window.viewport_size().width);
        let rem = f32::from(window.rem_size());
        let wide = width >= rem * 68.0;
        let layout = (wide, (width / rem * 100.0).round() as u32);
        if self.study_ui.layout != Some(layout) {
            self.study_ui.layout = Some(layout);
            self.study_ui.redraw_rows();
        }
        if let Some(target) = self.study_ui.source_target.clone() {
            let version = self.study_ui.version.clone();
            cx.on_next_frame(window, move |this, _, cx| {
                if !this.study_ui.enabled
                    || this.study_ui.version != version
                    || this.study_ui.source_target.as_ref() != Some(&target)
                {
                    return;
                }
                let measured = this.study_ui.source_position.borrow_mut().take();
                if let Some((measured_target, y)) = measured
                    && measured_target == target
                    && let Some(bounds) = this.study_ui.list.bounds_for_item(target.section + 1)
                {
                    this.study_ui.source_target = None;
                    this.study_ui.list.scroll_to(ListOffset {
                        item_ix: target.section + 1,
                        offset_in_item: (y - bounds.top() - px(12.)).max(px(0.)),
                    });
                    cx.notify();
                }
            });
        }
        let busy = self.study_ui.pending.is_some();
        let has_candidate = self.study_ui.candidate.is_some();
        let disabled = busy || has_candidate || self.study_ui.note.is_none();
        let saving = self
            .study_ui
            .pending
            .as_ref()
            .is_some_and(|pending| pending.kind == Work::Save);
        let working = self
            .study_ui
            .pending
            .as_ref()
            .is_some_and(|pending| pending.kind == Work::Ai);
        let has_readable = self
            .study_ui
            .note
            .as_ref()
            .is_some_and(|note| !readable_sections(note).is_empty());
        let header = h_flex()
            .w_full()
            .min_w_0()
            .flex_wrap()
            .gap_2()
            .items_center()
            .child(section_title("study-title", course2md::tr!("学习笔记")))
            .child(div().flex_1())
            .child(
                outline_pill("study-organize-all", cx)
                    .icon(icons::sparkles())
                    .label(course2md::tr!("整理全文"))
                    .disabled(disabled || !has_readable)
                    .on_click(cx.listener(|this, _, _, cx| {
                        let sections = this
                            .study_ui
                            .note
                            .as_ref()
                            .map(|note| readable_sections(note))
                            .unwrap_or_default();
                        this.run_study_ai(Operation::Organize(sections), cx);
                    })),
            )
            .child(
                outline_pill("study-summarize", cx)
                    .icon(icons::list_checks())
                    .label(course2md::tr!("生成学习摘要"))
                    .disabled(disabled)
                    .on_click(
                        cx.listener(|this, _, _, cx| this.run_study_ai(Operation::Summarize, cx)),
                    ),
            )
            .child(
                quiet("study-reload")
                    .icon(icons::refresh())
                    .label(course2md::tr!("重新加载学习笔记"))
                    .disabled(busy || has_candidate)
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.load_study(cx);
                        cx.notify();
                    })),
            )
            .child(
                quiet("study-return")
                    .icon(icons::book_open())
                    .label(course2md::tr!("返回阅读"))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.suspend_study();
                        cx.notify();
                    })),
            );
        let mut page = v_flex()
            .id("study-reader")
            .w_full()
            .h_full()
            .min_h_0()
            .min_w_0()
            .px(if wide {
                rems(24. / 14.)
            } else {
                rems(16. / 14.)
            })
            .py_3()
            .gap_3();
        let controls_bound =
            px((f32::from(window.viewport_size().height) - rem * 7.0).max(120.0) * 0.42);
        let controls_scroll = self.study_ui.controls_scroll.clone();
        let mut controls = v_flex().w_full().min_w_0().gap_3().child(header).child(
            accessible_text(
                "study-source-protection",
                course2md::tr!("原文保持不变，整理结果确认后单独保存。"),
            )
            .text_color(color(GRAY))
            .text_size(TEXT_BODY),
        );
        if let Some(error) = self.study_ui.error.clone() {
            controls = controls.child(
                notice_banner("study-error", NoticeKind::Danger, error).when(
                    self.study_ui.note.is_none() && !busy,
                    |notice| {
                        notice.child(
                            quiet("study-retry-load")
                                .icon(icons::refresh())
                                .label(course2md::tr!("重试"))
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.load_study(cx);
                                    cx.notify();
                                })),
                        )
                    },
                ),
            );
        }
        if working || saving || self.study_ui.status.is_some() {
            let status = self.study_ui.status.clone().unwrap_or_else(|| {
                if saving {
                    course2md::tr!("正在保存…").into()
                } else {
                    course2md::tr!("正在生成学习笔记…").into()
                }
            });
            controls = controls.child(
                h_flex()
                    .gap_3()
                    .items_center()
                    .flex_wrap()
                    .when(busy, |row| {
                        row.child(crate::motion::spinner("study-progress", cx))
                    })
                    .child(accessible_text("study-progress-label", status))
                    .when(working, |row| {
                        row.child(
                            quiet("study-cancel-ai")
                                .icon(icons::close())
                                .label(course2md::tr!("取消"))
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.study_ui.cancel_ai();
                                    cx.notify();
                                })),
                        )
                    }),
            );
        }
        if has_candidate && self.study_ui.error.is_some() && !busy {
            controls = controls.child(
                quiet("study-discard-reload")
                    .icon(icons::refresh())
                    .label(course2md::tr!("放弃预览并重新加载"))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.study_ui.candidate = None;
                        this.load_study(cx);
                        cx.notify();
                    })),
            );
        }
        if has_candidate {
            if let Some((done, total)) = self.study_ui.organized_progress {
                controls = controls.child(accessible_text(
                    "study-completed-count",
                    course2md::tr!("已整理 {}/{} 节，应用时仅保存已完成部分。", done, total),
                ));
            }
            let is_answer = matches!(self.study_ui.candidate, Some(Candidate::Answer(_)));
            controls = controls.child(
                notice_banner(
                    "study-preview-notice",
                    NoticeKind::Info,
                    course2md::tr!("预览尚未保存，确认后加入整理稿。"),
                )
                .child(
                    quiet("study-show-preview")
                        .icon(icons::notebook())
                        .label(course2md::tr!("查看预览"))
                        .on_click(cx.listener(|this, _, _, cx| {
                            if let Some(candidate) = &this.study_ui.candidate {
                                let row = candidate_row(candidate);
                                this.study_ui.column = Column::Draft;
                                this.study_ui.redraw_rows();
                                this.study_ui.list.scroll_to(ListOffset {
                                    item_ix: row,
                                    offset_in_item: px(0.),
                                });
                                cx.notify();
                            }
                        })),
                )
                .child(
                    primary_pill("study-apply")
                        .icon(icons::check())
                        .label(if is_answer {
                            course2md::tr!("加入整理稿")
                        } else {
                            course2md::tr!("应用整理结果")
                        })
                        .disabled(busy)
                        .on_click(cx.listener(|this, _, _, cx| this.save_study_candidate(cx))),
                )
                .child(
                    quiet("study-discard")
                        .icon(icons::close())
                        .label(course2md::tr!("放弃预览"))
                        .disabled(busy)
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.study_ui.candidate = None;
                            this.study_ui.organized_progress = None;
                            this.study_ui.error = None;
                            this.study_ui.status = None;
                            this.study_ui.redraw_rows();
                            cx.notify();
                        })),
                ),
            );
        }
        if self.study_ui.note.is_none() {
            return page
                .child(study_controls(controls, controls_bound, &controls_scroll))
                .when(busy, |page| {
                    page.child(crate::motion::spinner("study-loading", cx))
                })
                .into_any_element();
        }
        controls = controls.child(
            h_flex()
                .w_full()
                .items_center()
                .gap_3()
                .flex_wrap()
                .child(selection_button(
                    &self.study_ui.selected,
                    disabled,
                    cx,
                    |this| &mut this.study_ui.selected,
                    |this, spans, window, cx| this.study_quote_selected(spans, window, cx),
                ))
                .child(
                    accessible_text(
                        "study-selection-help",
                        course2md::tr!("在原文中划选文字，再输入问题。"),
                    )
                    .min_w_0()
                    .max_w_full()
                    .text_color(color(GRAY)),
                ),
        );
        if !self.study_ui.question_spans.is_empty() {
            let quote = self
                .study_ui
                .note
                .as_ref()
                .and_then(|note| note.quote(&self.study_ui.question_spans).ok())
                .unwrap_or_default();
            controls = controls.child(
                quiet("study-toggle-question")
                    .icon(icons::message_square())
                    .label(if self.study_ui.question_expanded {
                        course2md::tr!("收起")
                    } else {
                        course2md::tr!("输入问题")
                    })
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.study_ui.question_expanded = !this.study_ui.question_expanded;
                        if this.study_ui.question_expanded {
                            this.study_ui.controls_scroll.scroll_to_bottom();
                        }
                        cx.notify();
                    })),
            );
            if self.study_ui.question_expanded {
                controls = controls.child(
                    v_flex()
                        .w_full()
                        .min_w_0()
                        .gap_2()
                        .child(
                            div()
                                .w_full()
                                .min_w_0()
                                .max_h(rems(84. / 14.))
                                .overflow_hidden()
                                .text_color(color(GRAY))
                                .child(NoteText::new("study-question-quote", quote).preview()),
                        )
                        .child(
                            h_flex()
                                .gap_2()
                                .w_full()
                                .min_w_0()
                                .items_center()
                                .child(
                                    div().flex_1().min_w_0().child(
                                        text_input(&self.study_ui.question)
                                            .aria_label(course2md::tr!("输入问题")),
                                    ),
                                )
                                .child(
                                    primary_pill("study-ask")
                                        .icon(icons::message_square())
                                        .label(course2md::tr!("提问"))
                                        .disabled(disabled)
                                        .on_click(cx.listener(|this, _, _, cx| this.study_ask(cx))),
                                )
                                .child(
                                    quiet("study-close-question")
                                        .icon(icons::close())
                                        .label(course2md::tr!("取消"))
                                        .disabled(busy)
                                        .on_click(cx.listener(|this, _, _, cx| {
                                            this.study_ui.question_spans.clear();
                                            cx.notify();
                                        })),
                                ),
                        ),
                );
            }
        }
        page = page.child(study_controls(controls, controls_bound, &controls_scroll));
        if wide {
            page = page.child(
                h_flex()
                    .gap_3()
                    .w_full()
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .child(section_title("study-source-column", course2md::tr!("原文"))),
                    )
                    .child(div().flex_1().min_w_0().child(section_title(
                        "study-draft-column",
                        course2md::tr!("整理稿"),
                    ))),
            );
        } else {
            page = page.child(
                SingleChoiceGroup::new("study-columns", course2md::tr!("学习阅读视图"))
                    .tabs()
                    .options([
                        ("source", course2md::tr!("原文")),
                        ("draft", course2md::tr!("整理稿")),
                    ])
                    .icon("source", icons::book_open())
                    .icon("draft", icons::notebook())
                    .selected(if self.study_ui.column == Column::Source {
                        "source"
                    } else {
                        "draft"
                    })
                    .on_change(cx.listener(|this, selected: &SharedString, _, cx| {
                        this.study_ui.column = if selected.as_ref() == "source" {
                            Column::Source
                        } else {
                            Column::Draft
                        };
                        this.study_ui.redraw_rows();
                        cx.notify();
                    })),
            );
        }
        let weak = cx.weak_entity();
        let figure_width = (width - 72.0) / if wide { 2.0 } else { 1.0 } * window.scale_factor();
        let version = self.study_ui.version.clone();
        let list_state = self.study_ui.list.clone();
        page.child(
            div()
                .relative()
                .w_full()
                .min_w_0()
                .flex_1()
                .min_h_0()
                .child(
                    list(list_state.clone(), move |index, _, cx| {
                        weak.update(cx, |this, cx| {
                            if this.study_ui.version != version {
                                return div().into_any_element();
                            }
                            this.study_row(index, wide, figure_width, cx)
                        })
                        .unwrap_or_else(|_| div().into_any_element())
                    })
                    .w_full()
                    .h_full(),
                )
                .child(crate::backend::vertical_scrollbar_for(&list_state)),
        )
        .into_any_element()
    }

    fn study_row(
        &mut self,
        row: usize,
        wide: bool,
        figure_width: f32,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Some(note) = self.study_ui.note.clone() else {
            return div().into_any_element();
        };
        let show_source = wide || self.study_ui.column == Column::Source;
        let show_draft = wide || self.study_ui.column == Column::Draft;
        let mut pair = h_flex().w_full().min_w_0().gap_3().items_start().pb_3();
        if show_source {
            let content = if row == 0 {
                self.study_original_summary(&note)
            } else {
                self.study_source_section(&note, row - 1, figure_width, cx)
            };
            pair = pair.child(study_cell(content));
        }
        if show_draft {
            let content = if row == 0 {
                self.study_summary(&note, cx)
            } else {
                self.study_draft_section(&note, row - 1, figure_width, cx)
            };
            pair = pair.child(study_cell(content));
        }
        pair.into_any_element()
    }

    fn study_original_summary(&self, note: &StudyNote) -> AnyElement {
        let mut panel = v_flex().gap_3().w_full().min_w_0().child(section_title(
            "study-original-summary-title",
            course2md::tr!("原有摘要"),
        ));
        if let Some(summary) = &note.source.summary {
            panel = panel.child(study_text("study-original-summary", summary.tldr.clone()));
            for (index, point) in summary.key_points.iter().enumerate() {
                panel = panel.child(study_text(
                    ("study-original-point", index),
                    format!("• {point}"),
                ));
            }
            for (index, item) in summary.outline.iter().enumerate() {
                panel = panel.child(study_text(
                    ("study-original-outline", index),
                    format!(
                        "{} {}\n{}",
                        course2md::render::fmt_ts(item.t),
                        item.title,
                        item.detail
                    ),
                ));
            }
        } else {
            panel = panel.child(
                accessible_text("study-no-original-summary", course2md::tr!("原文暂无摘要"))
                    .text_color(color(GRAY)),
            );
        }
        panel.into_any_element()
    }

    fn study_summary(&self, note: &StudyNote, cx: &mut Context<Self>) -> AnyElement {
        let summary = match &self.study_ui.candidate {
            Some(Candidate::Summary(summary)) => Some(summary),
            _ => note.summary.as_ref(),
        };
        let mut panel = v_flex().gap_3().w_full().min_w_0().child(section_title(
            "study-summary-title",
            course2md::tr!("学习摘要"),
        ));
        if let Some(summary) = summary {
            panel = panel.child(study_text(
                "study-summary-overview",
                summary.overview.clone(),
            ));
            for (index, point) in summary.points.iter().enumerate() {
                let row = v_flex().gap_2().min_w_0().child(study_text(
                    ("study-summary-point", index),
                    point.text.clone(),
                ));
                let mut references = h_flex().w_full().min_w_0().flex_wrap().gap_2();
                for &section in &point.sections {
                    if let Some(source) = note.source.sections.get(section) {
                        references = references.child(
                            quiet(SharedString::from(format!("study-ref-{index}-{section}")))
                                .icon(icons::book_open())
                                .label(course2md::render::fmt_ts(source.t))
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.study_ui.column = Column::Source;
                                    this.study_ui.redraw_rows();
                                    this.study_ui.list.scroll_to(ListOffset {
                                        item_ix: section + 1,
                                        offset_in_item: px(0.),
                                    });
                                    cx.notify();
                                })),
                        );
                    }
                }
                panel = panel.child(row.child(references));
            }
        } else {
            panel = panel.child(
                accessible_text(
                    "study-summary-empty",
                    course2md::tr!("根据原文与截图生成摘要，保留章节出处。"),
                )
                .text_color(color(GRAY)),
            );
        }
        panel.into_any_element()
    }

    fn study_source_section(
        &mut self,
        note: &StudyNote,
        section: usize,
        figure_width: f32,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Some(source) = note.source.sections.get(section) else {
            return div().into_any_element();
        };
        let mut panel = v_flex().w_full().min_w_0().gap_3().child(section_title(
            SharedString::from(format!("study-source-time-{section}")),
            course2md::render::fmt_ts(source.t),
        ));
        if !source.image.is_empty() {
            let image = self.study_ui.images.get(section).cloned().flatten();
            panel = panel.child(self.study_reader_figure(
                image.as_deref(),
                figure_width,
                SharedString::from(format!("study-source-image-{section}")),
                cx,
            ));
        }
        for (speech, event) in source.speech.iter().enumerate() {
            let version = self.study_ui.version.clone();
            let digest = note.source_digest.clone();
            let weak = cx.weak_entity();
            let id = SharedString::from(format!(
                "study-source-{}-{}-{section}-{speech}",
                self.study_ui
                    .version
                    .as_deref()
                    .unwrap_or_else(|| std::path::Path::new(""))
                    .display(),
                note.source_digest
            ));
            let mut text = NoteText::new(id, event.text.clone())
                .document_order(((section as u64) << 32) | speech as u64)
                .selection_color(color(READING_SELECTION).into())
                .width_group("study-source")
                .highlights(
                    self.study_ui
                        .source_marks
                        .iter()
                        .filter(|span| span.section == section && span.speech == speech)
                        .map(|span| {
                            (
                                span.start..span.end,
                                HighlightStyle {
                                    background_color: Some(color(READING_SELECTION).into()),
                                    ..Default::default()
                                },
                            )
                        })
                        .collect(),
                )
                .on_selection(move |range, _, cx| {
                    let _ = weak.update(cx, |this, cx| {
                        if !this.study_ui.enabled
                            || this.study_ui.version != version
                            || !this
                                .study_ui
                                .note
                                .as_ref()
                                .is_some_and(|note| note.source_digest == digest)
                        {
                            return;
                        }
                        let old = match range {
                            Some(range) => this
                                .study_ui
                                .selected
                                .blocks
                                .insert((section, speech), range),
                            None => this.study_ui.selected.blocks.remove(&(section, speech)),
                        };
                        if old.is_some() || !this.study_ui.selected.blocks.is_empty() {
                            cx.notify();
                        }
                    });
                });
            if let Some(target) = self
                .study_ui
                .source_target
                .as_ref()
                .filter(|target| target.section == section && target.speech == speech)
                .cloned()
            {
                let position = self.study_ui.source_position.clone();
                text = text.on_source_position(target.start, move |point, _| {
                    *position.borrow_mut() = Some((target, point.y));
                });
            }
            panel = panel.child(
                div()
                    .w_full()
                    .min_w_0()
                    .text_size(rems(17. / 14.))
                    .line_height(relative(1.8))
                    .child(text),
            );
            if let Some(translation) = &event.translation {
                panel = panel.child(
                    study_text(
                        SharedString::from(format!("study-translation-{section}-{speech}")),
                        translation.clone(),
                    )
                    .text_color(color(GRAY)),
                );
            }
        }
        panel.into_any_element()
    }

    fn study_draft_section(
        &mut self,
        note: &StudyNote,
        section: usize,
        figure_width: f32,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Some(source) = note.source.sections.get(section) else {
            return div().into_any_element();
        };
        let disabled = self.study_ui.pending.is_some() || self.study_ui.candidate.is_some();
        let mut panel = v_flex().w_full().min_w_0().gap_3().child(
            h_flex()
                .gap_2()
                .flex_wrap()
                .items_center()
                .child(section_title(
                    SharedString::from(format!("study-draft-time-{section}")),
                    course2md::render::fmt_ts(source.t),
                ))
                .child(div().flex_1())
                .child(
                    quiet(SharedString::from(format!("study-organize-{section}")))
                        .icon(icons::sparkles())
                        .label(course2md::tr!("整理本节"))
                        .disabled(
                            disabled
                                || !source
                                    .speech
                                    .iter()
                                    .any(|speech| !speech.text.trim().is_empty()),
                        )
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.run_study_ai(Operation::Organize(vec![section]), cx)
                        })),
                ),
        );
        let candidate = match &self.study_ui.candidate {
            Some(Candidate::Answer(answer)) => Some(answer),
            _ => None,
        };
        for answer in candidate
            .into_iter()
            .chain(note.answers.iter())
            .filter(|answer| {
                answer
                    .spans
                    .first()
                    .is_some_and(|span| span.section == section)
            })
        {
            let spans = answer.spans.clone();
            let quote = note.quote(&spans).unwrap_or_default();
            let excerpt = quote_excerpt(&quote);
            let mut answer_panel = v_flex()
                .gap_2()
                .pt_3()
                .border_t_1()
                .border_color(color(HAIRLINE))
                .child(section_title(
                    SharedString::from(format!("study-answer-label-{}", answer.id)),
                    course2md::tr!("AI补充"),
                ))
                .child(
                    accessible_text(
                        SharedString::from(format!("study-answer-quote-{}", answer.id)),
                        excerpt,
                    )
                    .text_color(color(GRAY)),
                );
            let targets: std::collections::BTreeSet<_> =
                spans.iter().map(|span| span.section).collect();
            for (index, target) in targets.into_iter().enumerate() {
                let marks = spans.clone();
                let time = course2md::render::fmt_ts(note.source.sections[target].t);
                answer_panel = answer_panel.child(
                    quiet(SharedString::from(format!(
                        "study-answer-source-{}-{index}",
                        answer.id
                    )))
                    .icon(icons::book_open())
                    .label(course2md::tr!("查看原文 {}", time))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.study_ui.source_marks = marks.clone();
                        this.study_ui.source_target =
                            marks.iter().find(|span| span.section == target).cloned();
                        this.study_ui.source_position.borrow_mut().take();
                        this.study_ui.column = Column::Source;
                        this.study_ui.redraw_rows();
                        this.study_ui.list.scroll_to(ListOffset {
                            item_ix: target + 1,
                            offset_in_item: px(0.),
                        });
                        cx.notify();
                    })),
                );
            }
            panel = panel.child(
                answer_panel
                    .child(accessible_text(
                        SharedString::from(format!("study-answer-question-{}", answer.id)),
                        answer.question.clone(),
                    ))
                    .child(study_text(
                        SharedString::from(format!("study-answer-{}", answer.id)),
                        answer.text.clone(),
                    )),
            );
        }
        let organized = match &self.study_ui.candidate {
            Some(Candidate::Organized(sections)) => sections
                .get(&section)
                .or_else(|| note.organized.get(&section)),
            _ => note.organized.get(&section),
        }
        .cloned();
        if let Some(organized) = organized {
            if !source.image.is_empty() {
                let image = self.study_ui.images.get(section).cloned().flatten();
                panel = panel.child(self.study_reader_figure(
                    image.as_deref(),
                    figure_width,
                    SharedString::from(format!("study-draft-image-{section}")),
                    cx,
                ));
            }
            panel = panel.child(study_text(
                SharedString::from(format!("study-organized-{section}")),
                organized.text,
            ));
            for (index, review) in organized.review.into_iter().enumerate() {
                panel = panel.child(notice_banner(
                    SharedString::from(format!("study-review-{section}-{index}")),
                    NoticeKind::Info,
                    review,
                ));
            }
        } else {
            panel = panel.child(
                accessible_text(
                    SharedString::from(format!("study-empty-{section}")),
                    course2md::tr!("本节尚未整理"),
                )
                .text_color(color(GRAY)),
            );
        }
        panel.into_any_element()
    }
}

/// Bound controls independently of text scale so notes always retain reading space.
/// The view switch stays outside this region and all actions remain reachable.
fn study_controls(controls: Div, bound: Pixels, scroll: &ScrollHandle) -> Div {
    div()
        .relative()
        .w_full()
        .min_w_0()
        .flex_shrink_0()
        .child(
            controls
                .id("study-controls")
                .max_h(bound)
                .min_h_0()
                .pr(crate::scroll::GUTTER)
                .overflow_y_scroll()
                .track_scroll(scroll),
        )
        .child(
            gpui_component::scroll::Scrollbar::vertical(scroll)
                .mode(gpui_component::scroll::ScrollbarMode::Always),
        )
}

fn study_cell(content: AnyElement) -> Div {
    div()
        .flex_1()
        .flex_basis(px(0.))
        .min_w_0()
        .p_4()
        .rounded(RADIUS_CARD)
        .bg(color(SURFACE))
        .child(content)
}

fn study_text(id: impl Into<ElementId>, text: impl Into<SharedString>) -> Div {
    div()
        .w_full()
        .min_w_0()
        .text_size(rems(17. / 14.))
        .line_height(relative(1.8))
        .child(
            NoteText::new(id, text)
                .selection_color(color(READING_SELECTION).into())
                .width_group("study-draft"),
        )
}

fn readable_sections(note: &StudyNote) -> Vec<usize> {
    note.source
        .sections
        .iter()
        .enumerate()
        .filter_map(|(index, section)| {
            section
                .speech
                .iter()
                .any(|speech| !speech.text.trim().is_empty())
                .then_some(index)
        })
        .collect()
}

fn candidate_row(candidate: &Candidate) -> usize {
    match candidate {
        Candidate::Organized(sections) => sections.keys().next().map_or(0, |index| index + 1),
        Candidate::Answer(answer) => answer.spans.first().map_or(0, |span| span.section + 1),
        Candidate::Summary(_) => 0,
    }
}

fn quote_excerpt(quote: &str) -> String {
    let mut chars = quote.chars();
    let mut excerpt: String = chars.by_ref().take(120).collect();
    if chars.next().is_some() {
        excerpt.push('…');
    }
    excerpt
}

impl Drop for StudyState {
    fn drop(&mut self) {
        if let Some(pending) = &self.pending {
            pending.cancelled.store(true, Ordering::Release);
        }
    }
}

/// The live selection can clear during mouse capture, before a button receives its click.
/// Retain only the explicit press snapshot, independently of later clear notifications.
#[derive(Default)]
struct StudySelection {
    blocks: BTreeMap<(usize, usize), Range<usize>>,
    pressed: Option<Vec<SourceSpan>>,
}

impl StudySelection {
    fn clear(&mut self) {
        self.blocks.clear();
        self.pressed = None;
    }
    fn spans(&self) -> Vec<SourceSpan> {
        self.blocks
            .iter()
            .map(|(&(section, speech), range)| SourceSpan {
                section,
                speech,
                start: range.start,
                end: range.end,
            })
            .collect()
    }
}

fn selection_button<T: 'static>(
    selection: &StudySelection,
    disabled: bool,
    cx: &mut Context<T>,
    selection_of: fn(&mut T) -> &mut StudySelection,
    confirm: impl Fn(&mut T, Vec<SourceSpan>, &mut Window, &mut Context<T>) + 'static,
) -> Div {
    let spans = selection.spans();
    let pressed = spans.clone();
    div()
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(move |this, _, _, _| {
                if !disabled {
                    selection_of(this).pressed = Some(pressed.clone());
                }
            }),
        )
        .child(
            outline_pill("study-question-selection", cx)
                .icon(icons::message_square())
                .label(course2md::tr!("对选中文字提问"))
                .disabled(disabled)
                .on_click(cx.listener(move |this, _, window, cx| {
                    let spans = selection_of(this)
                        .pressed
                        .take()
                        .unwrap_or_else(|| spans.clone());
                    confirm(this, spans, window, cx);
                })),
        )
}

impl Request {
    fn is_current(&self, version: Option<&std::path::Path>, pending: Option<&Self>) -> bool {
        version == Some(self.version.as_path())
            && pending.is_some_and(|pending| pending.id == self.id)
            && !self.cancelled.load(Ordering::Acquire)
    }
}

/// A failed publication leaves both the source snapshot and the exact preview available to retry.
fn accept_saved_note(
    note: &mut Option<Rc<StudyNote>>,
    candidate: &mut Option<Candidate>,
    result: Result<StudyNote>,
) -> Result<()> {
    let saved = result?;
    *note = Some(Rc::new(saved));
    *candidate = None;
    Ok(())
}

fn question_still_valid(origin: Option<&str>, spans: &[SourceSpan], note: &StudyNote) -> bool {
    origin == Some(note.source_digest.as_str()) && note.quote(spans).is_ok()
}

#[cfg(test)]
mod tests {
    use super::{
        Candidate, NoteText, OrganizedSection, Request, SourceSpan, StudyAnswer, StudyNote,
        StudySelection, StudySummary, Work, accept_saved_note, candidate_row, question_still_valid,
        quote_excerpt, readable_sections, selection_button,
    };
    use course2md::{artifact::Document, dispatch::TaskControl};
    use gpui::prelude::*;
    use gpui::{Context, IntoElement, MouseButton, Pixels, Point, Render, Styled, Window, div, px};
    use std::{
        cell::RefCell,
        collections::BTreeMap,
        path::PathBuf,
        rc::Rc,
        sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        },
    };

    fn document() -> Document {
        serde_json::from_value(serde_json::json!({
            "schema": 1,
            "meta": {"title":"Lesson", "webpage_url":"", "duration":40},
            "sections": [
                {"t":0,"end":10,"image":"images/one.png","speech":[]},
                {"t":10,"end":20,"image":"images/two.png","speech":[
                    {"start":10,"end":15,"text":"甲🙂 repeat repeat"},
                    {"start":15,"end":20,"text":"第二段内容"}
                ]},
                {"t":20,"end":30,"image":"images/three.png","speech":[{"start":20,"end":30,"text":"  \n"}]},
                {"t":30,"end":40,"image":"images/four.png","speech":[{"start":30,"end":40,"text":"last"}]}
            ], "summary": null
        })).unwrap()
    }

    fn answer(note: &StudyNote) -> Candidate {
        Candidate::Answer(
            StudyAnswer::new(
                note,
                vec![SourceSpan {
                    section: 1,
                    speech: 0,
                    start: "甲🙂 repeat ".len(),
                    end: "甲🙂 repeat repeat".len(),
                }],
                "Why?".into(),
                "Explanation".into(),
            )
            .unwrap(),
        )
    }

    #[test]
    fn approval_changes_only_the_draft_and_preserves_exact_repeat_reference() {
        let note = StudyNote::from_document(document()).unwrap();
        let original = serde_json::to_value(&note.source).unwrap();
        let candidates = [
            answer(&note),
            Candidate::Organized(BTreeMap::from([(
                1,
                OrganizedSection {
                    text: "Organized".into(),
                    review: vec!["Review".into()],
                },
            )])),
            Candidate::Summary(StudySummary {
                overview: "Overview".into(),
                points: Vec::new(),
            }),
        ];
        for candidate in candidates {
            let updated = candidate.apply_to(&note);
            assert_eq!(serde_json::to_value(&updated.source).unwrap(), original);
            assert_eq!(updated.source_digest, note.source_digest);
            assert_eq!(updated.revision, note.revision);
            if let Candidate::Answer(answer) = candidate {
                assert_eq!(updated.quote(&answer.spans).unwrap(), "repeat");
                assert_eq!(answer.spans[0].start, "甲🙂 repeat ".len());
                assert_eq!(candidate_row(&Candidate::Answer(answer)), 2);
            }
        }
        assert!(note.answers.is_empty() && note.organized.is_empty() && note.summary.is_none());
    }

    #[test]
    fn failed_save_retains_preview_and_retry_publishes_without_touching_original() {
        let dir = tempfile::tempdir().unwrap();
        let bytes = serde_json::to_vec(&document()).unwrap();
        std::fs::write(dir.path().join("document.json"), &bytes).unwrap();
        let initial = course2md::study::load(dir.path()).unwrap();
        let mut note = Some(Rc::new(initial.clone()));
        let mut candidate = Some(answer(&initial));
        let updated = candidate.as_ref().unwrap().apply_to(&initial);
        // A filesystem conflict is recoverable without sending the AI request again.
        std::fs::create_dir(dir.path().join("study.json")).unwrap();
        let result = course2md::study::save(
            dir.path(),
            initial.revision,
            &updated,
            &TaskControl::default(),
        );
        assert!(accept_saved_note(&mut note, &mut candidate, result).is_err());
        assert!(matches!(candidate, Some(Candidate::Answer(_))));
        assert_eq!(note.as_ref().unwrap().revision, 0);
        std::fs::remove_dir(dir.path().join("study.json")).unwrap();
        let retry = candidate.as_ref().unwrap().apply_to(note.as_ref().unwrap());
        let result = course2md::study::save(dir.path(), 0, &retry, &TaskControl::default());
        accept_saved_note(&mut note, &mut candidate, result).unwrap();
        assert!(candidate.is_none());
        assert_eq!(note.unwrap().revision, 1);
        assert_eq!(course2md::study::load(dir.path()).unwrap().answers.len(), 1);
        assert_eq!(
            std::fs::read(dir.path().join("document.json")).unwrap(),
            bytes
        );
    }

    #[test]
    fn reload_after_source_conflict_restores_only_matching_question_intent() {
        let dir = tempfile::tempdir().unwrap();
        let original = document();
        std::fs::write(
            dir.path().join("document.json"),
            serde_json::to_vec(&original).unwrap(),
        )
        .unwrap();
        let initial = course2md::study::load(dir.path()).unwrap();
        let Candidate::Answer(answer) = answer(&initial) else {
            unreachable!()
        };
        assert!(question_still_valid(
            Some(&initial.source_digest),
            &answer.spans,
            &course2md::study::load(dir.path()).unwrap()
        ));
        let mut changed = original;
        changed.sections[1].speech[0].text = "changed".into();
        std::fs::write(
            dir.path().join("document.json"),
            serde_json::to_vec(&changed).unwrap(),
        )
        .unwrap();
        assert!(
            course2md::study::save(
                dir.path(),
                0,
                &Candidate::Answer(answer.clone()).apply_to(&initial),
                &TaskControl::default()
            )
            .is_err()
        );
        let reloaded = course2md::study::load(dir.path()).unwrap();
        assert!(!question_still_valid(
            Some(&initial.source_digest),
            &answer.spans,
            &reloaded
        ));
        assert_eq!(reloaded.source.sections[1].speech[0].text, "changed");
    }

    #[test]
    fn cancelled_and_late_callbacks_cannot_replace_a_new_request_even_after_returning() {
        let a = PathBuf::from("/notes/a");
        let old = Request {
            id: 1,
            version: a.clone(),
            kind: Work::Ai,
            cancelled: Arc::new(AtomicBool::new(false)),
        };
        let newer = Request {
            id: 3,
            version: a.clone(),
            kind: Work::Ai,
            cancelled: Arc::new(AtomicBool::new(false)),
        };
        assert!(old.is_current(Some(&a), Some(&old)));
        assert!(!old.is_current(Some(std::path::Path::new("/notes/b")), Some(&old)));
        assert!(!old.is_current(Some(&a), Some(&newer)));
        assert!(!old.is_current(Some(&a), None));
        old.cancelled.store(true, Ordering::Release);
        assert!(!old.is_current(Some(&a), Some(&old)));
        assert!(newer.is_current(Some(&a), Some(&newer)));
    }

    #[test]
    fn selection_aggregates_document_order_and_clear_removes_only_its_block() {
        let note = StudyNote::from_document(document()).unwrap();
        let mut selected = StudySelection::default();
        selected.blocks.insert((1, 1), 0.."第二段".len());
        selected.blocks.insert((1, 0), 0.."甲🙂".len());
        let spans = selected.spans();
        assert_eq!(spans[0].speech, 0);
        assert_eq!(note.quote(&spans).unwrap(), "甲🙂\n\n第二段");
        selected.pressed = Some(spans.clone());
        selected.blocks.remove(&(1, 0));
        assert_eq!(selected.spans().len(), 1);
        assert_eq!(selected.pressed.as_ref(), Some(&spans));
        selected.clear();
        assert!(selected.spans().is_empty() && selected.pressed.is_none());
        assert_eq!(readable_sections(&note), vec![1, 3]);
        assert!(quote_excerpt(&"🙂".repeat(121)).ends_with('…'));
    }

    struct SelectionHarness {
        selection: StudySelection,
        confirmed: Vec<SourceSpan>,
        position: Rc<RefCell<Option<Point<Pixels>>>>,
    }

    impl Render for SelectionHarness {
        fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            window.set_rem_size(px(14.));
            let weak = cx.weak_entity();
            let position = self.position.clone();
            div()
                .size_full()
                .text_size(px(16.))
                .line_height(px(24.))
                .child(gpui_base::TextSelectionLayer)
                .child(
                    div().w(px(500.)).child(
                        NoteText::new("study-test-source", "alpha beta gamma delta")
                            .on_source_position(0, move |point, height| {
                                *position.borrow_mut() =
                                    Some(point + gpui::point(px(0.), height / 2.))
                            })
                            .on_selection(move |range, _, cx| {
                                let _ = weak.update(cx, |this, cx| {
                                    match range {
                                        Some(range) => {
                                            this.selection.blocks.insert((0, 0), range);
                                        }
                                        None => {
                                            this.selection.blocks.remove(&(0, 0));
                                        }
                                    }
                                    cx.notify();
                                });
                            }),
                    ),
                )
                .child(
                    div()
                        .absolute()
                        .top(px(80.))
                        .left(px(10.))
                        .debug_selector(|| "study-test-button".into())
                        .child(selection_button(
                            &self.selection,
                            false,
                            cx,
                            |this| &mut this.selection,
                            |this, spans, _, cx| {
                                this.confirmed = spans;
                                cx.notify();
                            },
                        )),
                )
        }
    }

    #[gpui::test]
    fn question_button_keeps_the_rendered_quote_through_mousedown_clear_and_repaint(
        cx: &mut gpui::TestAppContext,
    ) {
        cx.update(|cx| {
            gpui_component::init(cx);
            crate::theme::init(cx);
        });
        let (view, cx) = cx.add_window_view(|_, _| SelectionHarness {
            selection: StudySelection::default(),
            confirmed: Vec::new(),
            position: Rc::default(),
        });
        let draw = |cx: &mut gpui::VisualTestContext| {
            cx.update(|window, cx| {
                window.draw(cx).clear(cx);
            });
            cx.run_until_parked();
        };
        draw(cx);
        let start = cx.update(|_, cx| view.read(cx).position.borrow().unwrap());
        let end = start + gpui::point(px(95.), px(0.));
        cx.simulate_mouse_down(start, MouseButton::Left, Default::default());
        cx.simulate_mouse_move(end, Some(MouseButton::Left), Default::default());
        cx.simulate_mouse_up(end, MouseButton::Left, Default::default());
        draw(cx);
        let expected = cx.update(|_, cx| view.read(cx).selection.spans());
        assert!(!expected.is_empty());
        let button = cx.debug_bounds("study-test-button").unwrap().center();
        cx.simulate_mouse_down(button, MouseButton::Left, Default::default());
        draw(cx);
        cx.update(|_, cx| assert!(view.read(cx).selection.blocks.is_empty()));
        cx.simulate_mouse_up(button, MouseButton::Left, Default::default());
        draw(cx);
        cx.update(|_, cx| assert_eq!(view.read(cx).confirmed, expected));
    }
}
