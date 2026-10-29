//! Private source-text drafts; only an explicit save reaches the transaction worker.
use super::*;
use course2md::note_edit::SelectionEdit;
use gpui_component::input::{Textarea, TextareaState};

const MAX_MANUAL_SPANS: usize = 64;

#[derive(Debug, PartialEq, Eq)]
enum DraftError {
    TooManySpans,
    InvalidSelection,
}

struct ManualSource {
    preview: Rc<notes::Preview>,
    generation: u64,
    spans: Vec<SourceSpan>,
    originals: Vec<String>,
    stale: bool,
}

impl ManualSource {
    fn new(
        preview: Rc<notes::Preview>,
        generation: u64,
        spans: Vec<SourceSpan>,
    ) -> Result<Self, DraftError> {
        // Check the limit before allocating any input entities or source copies.
        if spans.len() > MAX_MANUAL_SPANS {
            return Err(DraftError::TooManySpans);
        }
        if spans.is_empty() {
            return Err(DraftError::InvalidSelection);
        }
        let mut originals = Vec::with_capacity(spans.len());
        let mut previous: Option<&SourceSpan> = None;
        for span in &spans {
            if previous.is_some_and(|previous| {
                (span.section, span.speech) < (previous.section, previous.speech)
                    || ((span.section, span.speech) == (previous.section, previous.speech)
                        && span.start < previous.end)
            }) {
                return Err(DraftError::InvalidSelection);
            }
            // The parent's accessor validates bounds, nonempty ranges and UTF-8.
            // Preserve whitespace and line breaks exactly, including whitespace-only spans.
            originals.push(
                selected_text(&preview.document, span)
                    .ok_or(DraftError::InvalidSelection)?
                    .to_owned(),
            );
            previous = Some(span);
        }
        Ok(Self {
            preview,
            generation,
            spans,
            originals,
            stale: false,
        })
    }

    fn observe(
        &mut self,
        page: Page,
        reading: bool,
        preview: Option<&Rc<notes::Preview>>,
        generation: u64,
    ) {
        // Invalidation is sticky: revisiting a note must not resurrect its draft.
        self.stale |= page != Page::Result
            || reading
            || generation != self.generation
            || !preview.is_some_and(|preview| Rc::ptr_eq(&self.preview, preview));
    }

    fn sync(&mut self, desktop: &Desktop) {
        self.observe(
            desktop.page,
            desktop.reading,
            desktop.preview.as_ref(),
            desktop.read_generation,
        );
    }

    fn edits(&self, values: Vec<String>, delete: bool) -> Option<Vec<SelectionEdit>> {
        if self.stale || values.len() != self.spans.len() {
            return None;
        }
        Some(
            self.spans
                .iter()
                .zip(values)
                .map(|(span, value)| SelectionEdit {
                    section: span.section,
                    speech: span.speech,
                    start: span.start,
                    end: span.end,
                    text: if delete { String::new() } else { value },
                })
                .collect(),
        )
    }
}

struct ManualEditor {
    desktop: Entity<Desktop>,
    source: ManualSource,
    inputs: Vec<Entity<TextareaState>>,
    delete: bool,
    scroll: ScrollHandle,
    saving: Rc<Cell<bool>>,
    closed: Rc<Cell<bool>>,
    error: Option<String>,
    return_focus: Option<FocusHandle>,
    _observation: Subscription,
}

impl Desktop {
    pub(super) fn open_manual_editor(
        &mut self,
        spans: Vec<SourceSpan>,
        delete: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.page != Page::Result || self.reading || self.reader_ui.ai_edit.busy() {
            return;
        }
        let Some(preview) = self.preview.clone() else {
            return;
        };
        let source = match ManualSource::new(preview, self.read_generation, spans) {
            Ok(source) => source,
            Err(error) => {
                self.message = Some(match error {
                    DraftError::TooManySpans => course2md::tr!(
                        "一次最多修改或删除 {} 段选中文字，请缩小选区后重试",
                        MAX_MANUAL_SPANS
                    ),
                    DraftError::InvalidSelection => {
                        course2md::tr!("所选原文已失效，请重新选择").into()
                    }
                });
                cx.notify();
                return;
            }
        };
        let inputs: Vec<_> = source
            .originals
            .iter()
            .map(|text| {
                cx.new(|cx| {
                    TextareaState::new(window, cx)
                        .auto_grow(3, 8)
                        .default_value(text.clone())
                })
            })
            .collect();
        let focus = inputs[0].clone();
        let preferred_height_rems = (220. + inputs.len() as f32 * 180.) / 14.;
        let desktop = cx.entity();
        let return_focus = window.focused(cx);
        let closed = Rc::new(Cell::new(false));
        let saving = Rc::new(Cell::new(false));
        let content = cx.new(|cx| ManualEditor {
            _observation: cx.observe(&desktop, |this: &mut ManualEditor, desktop, cx| {
                this.source.sync(desktop.read(cx));
                cx.notify();
            }),
            desktop,
            source,
            inputs,
            delete,
            scroll: ScrollHandle::new(),
            saving: saving.clone(),
            closed: closed.clone(),
            error: None,
            return_focus: return_focus.clone(),
        });
        let focus_closed = closed.clone();
        window.open_dialog(cx, move |dialog, window, _| {
            let content = content.clone();
            let closed = closed.clone();
            let saving = saving.clone();
            let return_focus = return_focus.clone();
            let top = task_dialog_top(window);
            dialog
                .material(
                    "manual-editor-title",
                    if delete {
                        course2md::tr!("删除选中文字")
                    } else {
                        course2md::tr!("修改选中文字")
                    },
                )
                .w(dialog_width(DIALOG_WIDE_WIDTH, window))
                .h((window.rem_size() * preferred_height_rems)
                    .min((window.viewport_size().height - top - DIALOG_MARGIN).max(px(0.))))
                .min_h_0()
                .margin_top(top)
                .overlay_closable(false)
                .close_button(false)
                .content(move |body, _, _| body.min_h_0().overflow_hidden().child(content.clone()))
                // Enter inserts a newline in a field; it never submits this dialog.
                .on_ok(|_, _, _| false)
                .on_cancel(move |_, _, _| !saving.get())
                .on_close(move |_, window, cx| {
                    closed.set(true);
                    if let Some(focus) = &return_focus {
                        focus.focus(window, cx);
                    }
                })
        });
        window.defer(cx, move |window, cx| {
            if !focus_closed.get() {
                focus.update(cx, |state, cx| state.focus(window, cx));
            }
        });
    }
}

impl ManualEditor {
    fn dismiss(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.saving.get() || self.closed.replace(true) {
            return;
        }
        window.close_dialog(cx);
        if let Some(focus) = &self.return_focus {
            focus.focus(window, cx);
        }
    }

    fn save(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.source.sync(self.desktop.read(cx));
        if self.closed.get() || self.source.stale || self.saving.get() {
            return;
        }
        let values = self
            .inputs
            .iter()
            .map(|input| input.read(cx).value().to_string())
            .collect();
        let Some(edits) = self.source.edits(values, self.delete) else {
            return;
        };
        self.error = None;
        self.saving.set(true);
        let editor = cx.weak_entity();
        let window_handle = window.window_handle();
        let started = self.desktop.update(cx, |desktop, cx| {
            desktop.save_manual_selection(
                self.source.preview.clone(),
                self.source.generation,
                edits,
                move |result, cx| {
                    // Also safe if a rejected request completes synchronously:
                    // the editor/window are still borrowed by the Save handler.
                    cx.defer(move |cx| {
                        let _ = window_handle.update(cx, |_, window, cx| {
                            let _ = editor.update(cx, |this, cx| {
                                this.saving.set(false);
                                if this.closed.get() {
                                    return;
                                }
                                match result {
                                    Ok(()) => this.dismiss(window, cx),
                                    Err(error) => {
                                        this.error = Some(error);
                                        this.scroll.set_offset(point(px(0.), px(0.)));
                                        this.source.sync(this.desktop.read(cx));
                                        cx.notify();
                                    }
                                }
                            });
                        });
                    });
                },
                cx,
            )
        });
        if !started {
            self.saving.set(false);
            self.source.sync(self.desktop.read(cx));
            self.error = Some(course2md::tr!("修改暂时无法保存，请重试").into());
        }
        cx.notify();
    }
}

impl Render for ManualEditor {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.source.sync(self.desktop.read(cx));
        let saving = self.saving.get();
        let busy = self.desktop.read(cx).reader_ui.ai_edit.busy();
        let mut body = v_flex()
            .w_full()
            .min_w_0()
            .gap_4()
            .when_some(self.error.clone(), |body, error| {
                body.child(
                    accessible_text("manual-error", error)
                        .text_color(color(DANGER))
                        .flex_shrink_0(),
                )
            })
            .child(section_heading(
                "manual-source-label",
                icons::text_lines(),
                course2md::tr!("选中的原文"),
            ));
        for (index, input) in self.inputs.iter().enumerate() {
            let label = course2md::tr!("{} / {} 段", index + 1, self.inputs.len());
            body = body.child(
                v_flex()
                    .w_full()
                    .min_w_0()
                    .flex_shrink_0()
                    .gap_2()
                    .child(section_heading(
                        ("manual-span-label", index),
                        if self.delete {
                            icons::delete()
                        } else {
                            icons::edit()
                        },
                        label.clone(),
                    ))
                    .child(
                        Textarea::new(input)
                            .aria_label(label)
                            .readonly(self.delete)
                            .disabled(self.source.stale || saving),
                    ),
            );
        }
        let status = if saving {
            Some(course2md::tr!("正在保存…"))
        } else if self.source.stale {
            Some(course2md::tr!("所选原文已失效，请重新选择"))
        } else if busy {
            Some(course2md::tr!("正在处理"))
        } else {
            None
        };
        let save = if self.delete {
            danger_pill("manual-save", cx)
                .icon(icons::delete())
                .label(course2md::tr!("删除"))
        } else {
            primary_pill("manual-save")
                .icon(icons::save())
                .label(course2md::tr!("保存"))
        };
        v_flex()
            .size_full()
            .min_h_0()
            .min_w_0()
            .gap_3()
            .child(
                div()
                    .relative()
                    .flex_1()
                    .min_h_0()
                    .min_w_0()
                    .overflow_hidden()
                    .child(
                        body.id("manual-editor-scroll")
                            .size_full()
                            .pr(crate::scroll::GUTTER)
                            .overflow_y_scroll()
                            .track_scroll(&self.scroll),
                    )
                    .child(crate::scroll::vertical_scrollbar(&self.scroll)),
            )
            .child(
                v_flex()
                    .flex_shrink_0()
                    .gap_2()
                    .when_some(status, |footer, status| {
                        footer.child(accessible_text("manual-status", status).text_color(color(
                            if self.source.stale && !saving {
                                DANGER
                            } else {
                                GRAY
                            },
                        )))
                    })
                    .child(
                        dialog_actions()
                            .child(
                                text_button("manual-cancel", cx)
                                    .label(course2md::tr!("取消"))
                                    .disabled(saving)
                                    .on_click(
                                        cx.listener(|this, _, window, cx| this.dismiss(window, cx)),
                                    ),
                            )
                            .child(
                                save.disabled(
                                    saving || busy || self.source.stale || self.closed.get(),
                                )
                                .on_click(cx.listener(|this, _, window, cx| this.save(window, cx))),
                            ),
                    ),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::{DraftError, ManualSource, Page, SourceSpan, notes, selected_text};
    use std::rc::Rc;

    fn note() -> (tempfile::TempDir, Rc<notes::Preview>) {
        let temp = tempfile::tempdir().unwrap();
        let course =
            notes::fixtures::published_note(temp.path(), 1, &[0., 10.], Default::default());
        let mut preview = notes::read_preview(course).unwrap();
        preview.document.sections[0].speech[0].text = "前原文后 \n".into();
        preview.document.sections[1].speech[0].text = "second passage".into();
        (temp, Rc::new(preview))
    }

    fn span(section: usize, start: usize, end: usize) -> SourceSpan {
        SourceSpan {
            section,
            speech: 0,
            start,
            end,
        }
    }

    #[test]
    fn validates_the_entire_selection_before_allocating_fields() {
        let (_temp, preview) = note();
        let draft = ManualSource::new(
            preview.clone(),
            4,
            vec![span(0, 3, 9), span(0, 9, 14), span(1, 0, 6)],
        )
        .unwrap();
        assert_eq!(draft.originals, ["原文", "后 \n", "second"]);
        let whitespace = ManualSource::new(preview.clone(), 4, vec![span(0, 12, 14)]).unwrap();
        assert_eq!(whitespace.originals, [" \n"]);
        for spans in [
            vec![],
            vec![span(0, 1, 9)],
            vec![span(0, 3, 8)],
            vec![span(0, 3, 99)],
            vec![span(0, 3, 3)],
            vec![span(0, 9, 3)],
            vec![span(0, 3, 9), span(0, 6, 12)],
            vec![span(0, 3, 9), span(0, 3, 9)],
            vec![span(1, 0, 6), span(0, 3, 9)],
            vec![span(99, 0, 1)],
            vec![SourceSpan {
                speech: 99,
                ..span(0, 0, 3)
            }],
        ] {
            assert_eq!(
                ManualSource::new(preview.clone(), 4, spans).err(),
                Some(DraftError::InvalidSelection)
            );
        }
    }

    #[test]
    fn accepts_64_fields_and_rejects_65_without_truncating() {
        let (_temp, mut preview) = note();
        Rc::make_mut(&mut preview).document.sections[0].speech[0].text = "x".repeat(65);
        let spans: Vec<_> = (0..64).map(|i| span(0, i, i + 1)).collect();
        assert_eq!(
            ManualSource::new(preview.clone(), 4, spans.clone())
                .unwrap()
                .originals
                .len(),
            64
        );
        let mut too_many = spans;
        too_many.push(span(0, 64, 65));
        assert_eq!(
            ManualSource::new(preview, 4, too_many).err(),
            Some(DraftError::TooManySpans)
        );
    }

    #[test]
    fn freezes_source_and_preserves_exact_replacements_and_delete_coordinates() {
        let (_temp, preview) = note();
        let draft =
            ManualSource::new(preview.clone(), 4, vec![span(0, 3, 9), span(1, 0, 6)]).unwrap();
        let values = vec!["  新文\n🙂 ".to_owned(), String::new()];
        let edits = draft.edits(values.clone(), false).unwrap();
        assert_eq!(edits[0].text, values[0]);
        assert!(edits[1].text.is_empty());
        for (edit, span) in edits.iter().zip(&draft.spans) {
            assert_eq!(
                (edit.section, edit.speech, edit.start, edit.end),
                (span.section, span.speech, span.start, span.end)
            );
        }
        let deletes = draft.edits(values, true).unwrap();
        for (delete, edit) in deletes.iter().zip(edits) {
            assert!(delete.text.is_empty());
            assert_eq!(
                (delete.section, delete.speech, delete.start, delete.end),
                (edit.section, edit.speech, edit.start, edit.end)
            );
        }
        assert!(draft.edits(vec![], false).is_none());
        assert_eq!(
            selected_text(&preview.document, &draft.spans[0]),
            Some("原文")
        );
        assert_eq!(draft.originals, ["原文", "second"]);
    }

    #[test]
    fn invalidation_is_sticky_for_navigation_reload_and_replaced_preview() {
        let (_temp, preview) = note();
        let reloaded = Rc::new(preview.as_ref().clone());
        for (page, reading, current, generation) in [
            (Page::Library, false, Some(&preview), 4),
            (Page::Result, true, Some(&preview), 4),
            (Page::Result, false, Some(&preview), 5),
            (Page::Result, false, Some(&reloaded), 4),
            (Page::Result, false, None, 4),
        ] {
            let mut draft = ManualSource::new(preview.clone(), 4, vec![span(0, 3, 9)]).unwrap();
            draft.observe(Page::Result, false, Some(&preview), 4);
            assert!(!draft.stale);
            draft.observe(page, reading, current, generation);
            assert!(draft.stale);
            draft.observe(Page::Result, false, Some(&preview), 4);
            assert!(draft.stale);
            assert!(draft.edits(vec!["replacement".into()], false).is_none());
        }
    }
}
