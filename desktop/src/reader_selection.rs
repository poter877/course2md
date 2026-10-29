//! A private, frozen selection draft. Only the explicit Start action submits it.
use super::*;
use crate::reader_images::{Loaded, Size as ImageSize};
use gpui_component::input::{Textarea, TextareaState};

const MAX_IMAGES: usize = 4;
// Bound thumbnail work even for notes containing thousands of frames.
const IMAGES_PER_PAGE: usize = 8;

struct SelectionPicture {
    path: PathBuf,
    label: String,
    selected: bool,
}

struct SelectionEditor {
    desktop: Entity<Desktop>,
    preview: Rc<notes::Preview>,
    version: PathBuf,
    generation: u64,
    spans: Vec<SourceSpan>,
    original: String,
    mode: AiRegionMode,
    pictures: Vec<SelectionPicture>,
    selected_paths: Vec<PathBuf>,
    picture_page: usize,
    pictures_loading: bool,
    instruction: Entity<TextareaState>,
    scroll: ScrollHandle,
    stale: bool,
    closed: Rc<Cell<bool>>,
    return_focus: Option<FocusHandle>,
    _observation: Subscription,
}

fn selection_quote(
    document: &course2md::artifact::Document,
    spans: &[SourceSpan],
) -> Option<String> {
    if spans.is_empty() {
        return None;
    }
    let mut previous: Option<&SourceSpan> = None;
    let mut quotes = Vec::with_capacity(spans.len());
    for span in spans {
        if span.start >= span.end
            || previous.is_some_and(|prev| {
                (span.section, span.speech) < (prev.section, prev.speech)
                    || ((span.section, span.speech) == (prev.section, prev.speech)
                        && span.start < prev.end)
            })
        {
            return None;
        }
        let text = selected_text(document, span)?;
        if text.trim().is_empty() {
            return None;
        }
        quotes.push(text);
        previous = Some(span);
    }
    Some(quotes.join("\n\n"))
}

fn same_selection_source(
    origin: &Rc<notes::Preview>,
    generation: u64,
    current: Option<&Rc<notes::Preview>>,
    current_generation: u64,
) -> bool {
    generation == current_generation && current.is_some_and(|current| Rc::ptr_eq(origin, current))
}

// Only receives the reader worker's validated frames; no filesystem access.
fn selection_pictures(
    preview: &notes::Preview,
    frames: &[Frame],
    selected_paths: &[PathBuf],
) -> Vec<SelectionPicture> {
    let mut pictures = Vec::new();
    let mut seen = HashSet::new();
    if let Some(manifest) = &preview.course.manifest {
        // load_reader_data keeps manifest/frame order and validates paths
        // using safe_asset_path on its worker. Never join untrusted asset
        // names here, call is_file in render, or give raw paths to img.
        for (_source, frame) in manifest.frames.iter().zip(frames.iter()) {
            if let Some(image) = &frame.image
                && seen.insert(image.path.clone())
            {
                pictures.push(SelectionPicture {
                    path: image.path.clone(),
                    label: frame_label(frame),
                    selected: selected_paths.contains(&image.path),
                });
            }
        }
    }
    // Keep an unreadable selected image visible so it cannot silently disappear
    // from the request; the normal failed-thumbnail state allows deselection.
    for path in selected_paths {
        if seen.insert(path.clone()) {
            pictures.push(SelectionPicture {
                path: path.clone(),
                label: course2md::tr!("截图").into(),
                selected: true,
            });
        }
    }
    // Preserve exactly the drag selection. Do not silently attach a nearby image
    // or drop selected images above the request limit; the user can deselect them.
    pictures.sort_by_key(|picture| !picture.selected);
    pictures
}

impl Desktop {
    pub(super) fn open_selection_editor(
        &mut self,
        spans: Vec<SourceSpan>,
        selected_paths: Vec<PathBuf>,
        mode: AiRegionMode,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.page != Page::Result || self.reading || self.reader_ui.ai_edit.busy() {
            return;
        }
        let Some(preview) = self.preview.as_ref() else {
            return;
        };
        let Some(original) = selection_quote(&preview.document, &spans) else {
            self.message = Some(course2md::tr!("所选原文已失效，请重新选择").into());
            cx.notify();
            return;
        };
        let pictures_loading = self.reader_ui.data_loading;
        let pictures = if pictures_loading {
            Vec::new()
        } else {
            selection_pictures(preview, &self.reader_ui.frames, &selected_paths)
        };
        let version = preview.course.dir.clone();
        let preview = preview.clone();
        let generation = self.read_generation;
        let instruction = cx.new(|cx| TextareaState::new(window, cx).auto_grow(3, 6));
        let focus = instruction.clone();
        let desktop = cx.entity();
        let return_focus = window.focused(cx);
        let closed = Rc::new(Cell::new(false));
        let content = cx.new(|cx| SelectionEditor {
            _observation: cx.observe(&desktop, |this: &mut SelectionEditor, desktop, cx| {
                // Cache completions notify Desktop too. Invalidation is sticky:
                // returning to the old note never resurrects this draft.
                this.sync_source(desktop.read(cx));
                cx.notify();
            }),
            desktop,
            preview,
            version,
            generation,
            spans,
            original,
            mode,
            pictures,
            selected_paths,
            picture_page: 0,
            pictures_loading,
            instruction,
            scroll: ScrollHandle::new(),
            stale: false,
            closed: closed.clone(),
            return_focus: return_focus.clone(),
        });
        window.open_dialog(cx, move |dialog, window, _| {
            let content = content.clone();
            let closed = closed.clone();
            let return_focus = return_focus.clone();
            let top = task_dialog_top(window);
            dialog
                .material("selection-editor-title", course2md::tr!("AI 处理选中文字"))
                .w(dialog_width(DIALOG_WIDE_WIDTH, window))
                .h((window.viewport_size().height - top - DIALOG_MARGIN).max(px(0.)))
                .min_h_0()
                .margin_top(top)
                .overlay_closable(false)
                .close_button(false)
                .content(move |body, _, _| body.min_h_0().overflow_hidden().child(content.clone()))
                // Enter in a field or the dialog must not silently send a request.
                .on_ok(|_, _, _| false)
                .on_close(move |_, window, cx| {
                    closed.set(true);
                    if let Some(focus) = &return_focus {
                        focus.focus(window, cx);
                    }
                })
        });
        window.defer(cx, move |window, cx| {
            focus.update(cx, |state, cx| state.focus(window, cx));
        });
    }
}

impl SelectionEditor {
    fn sync_source(&mut self, desktop: &Desktop) {
        self.stale |= !self.matches(desktop);
        if !self.stale && self.pictures_loading && !desktop.reader_ui.data_loading {
            self.pictures = selection_pictures(
                &self.preview,
                &desktop.reader_ui.frames,
                &self.selected_paths,
            );
            self.pictures_loading = false;
        }
    }

    fn matches(&self, desktop: &Desktop) -> bool {
        desktop.page == Page::Result
            && !desktop.reading
            && same_selection_source(
                &self.preview,
                self.generation,
                desktop.preview.as_ref(),
                desktop.read_generation,
            )
            && desktop.preview.as_ref().is_some_and(|preview| {
                preview.course.dir == self.version
                    && selection_quote(&preview.document, &self.spans).as_deref()
                        == Some(self.original.as_str())
            })
    }

    fn selected_count(&self) -> usize {
        self.pictures
            .iter()
            .filter(|picture| picture.selected)
            .count()
    }

    fn thumbnail(window: &Window) -> ImageSize {
        ImageSize::thumbnail(256. * window.scale_factor())
    }

    fn toggle(&mut self, index: usize, cx: &mut Context<Self>) {
        if self.stale || self.closed.get() {
            return;
        }
        let count = self.selected_count();
        if let Some(picture) = self.pictures.get_mut(index)
            && (picture.selected || count < MAX_IMAGES)
        {
            picture.selected = !picture.selected;
            cx.notify();
        }
    }

    fn dismiss(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.closed.set(true);
        window.close_dialog(cx);
        if let Some(focus) = &self.return_focus {
            focus.focus(window, cx);
        }
    }

    fn start(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.closed.get()
            || self.stale
            || self.pictures_loading
            || self.selected_count() > MAX_IMAGES
        {
            return;
        }
        let instruction = self.instruction.read(cx).value().to_string();
        let images: Vec<PathBuf> = self
            .pictures
            .iter()
            .filter(|picture| picture.selected)
            .map(|picture| picture.path.clone())
            .collect();
        let started = self.desktop.clone().update(cx, |desktop, cx| {
            if !self.matches(desktop) {
                self.stale = true;
                return false;
            }
            if desktop.reader_ui.ai_edit.busy() || images.len() > MAX_IMAGES {
                return false;
            }
            // Require successful decoding of each attachment. A missing or
            // corrupt image can be deselected, including for text-only work.
            if images.iter().any(|path| {
                !matches!(
                    desktop.reader_ui.images.get(
                        path,
                        Self::thumbnail(window),
                        Desktop::reader_images,
                        cx,
                    ),
                    Loaded::Ready(_, _)
                )
            }) {
                return false;
            }
            self.closed.set(true);
            desktop.start_ai_selection(self.spans.clone(), self.mode, images, instruction, cx);
            true
        });
        if started {
            self.dismiss(window, cx);
        } else {
            cx.notify();
        }
    }
}

impl Render for SelectionEditor {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.sync_source(self.desktop.clone().read(cx));
        let start = self.picture_page * IMAGES_PER_PAGE;
        let end = (start + IMAGES_PER_PAGE).min(self.pictures.len());
        let (busy, loaded) = self.desktop.clone().update(cx, |desktop, cx| {
            let mut loaded = HashMap::new();
            if !self.stale && !self.closed.get() {
                for (index, picture) in self.pictures.iter().enumerate() {
                    if (start..end).contains(&index) || picture.selected {
                        loaded.insert(
                            index,
                            desktop.reader_ui.images.get(
                                &picture.path,
                                Self::thumbnail(window),
                                Desktop::reader_images,
                                cx,
                            ),
                        );
                    }
                }
            }
            (desktop.reader_ui.ai_edit.busy(), loaded)
        });
        let count = self.selected_count();
        let failed = self.pictures.iter().enumerate().any(|(index, picture)| {
            picture.selected && matches!(loaded.get(&index), Some(Loaded::Failed))
        });
        let pending = self.pictures_loading
            || self.pictures.iter().enumerate().any(|(index, picture)| {
                picture.selected && !matches!(loaded.get(&index), Some(Loaded::Ready(_, _)))
            });
        let mode = match self.mode {
            AiRegionMode::Clean => course2md::tr!("清理语气词"),
            AiRegionMode::Polish => course2md::tr!("优化"),
            AiRegionMode::Translate => course2md::tr!("翻译"),
        };
        let mut gallery = h_flex().w_full().min_w_0().flex_wrap().gap_3();
        for index in start..end {
            let picture = &self.pictures[index];
            let selected = picture.selected;
            let mut figure = div()
                .relative()
                .w_full()
                .aspect_ratio(16. / 9.)
                .rounded(FIGURE_RADIUS)
                .overflow_hidden()
                .bg(color(SURFACE_HIGH));
            figure = match loaded.get(&index) {
                Some(Loaded::Ready(image, _)) => figure.child(
                    img(image.clone())
                        .absolute()
                        .inset_0()
                        .size_full()
                        .object_fit(ObjectFit::Contain),
                ),
                Some(Loaded::Failed) => figure.child(
                    unreadable_screenshot(("selection-failed", index))
                        .absolute()
                        .inset_0(),
                ),
                _ => figure.child(screenshot_placeholder()),
            };
            gallery = gallery.child(
                selection_card(
                    ("selection-picture", index),
                    selected,
                    if selected { 1. } else { 0. },
                )
                .flex_1()
                .flex_basis(rems(200. / 14.))
                .min_w_0()
                .p_2()
                .gap_2()
                .disabled(
                    self.stale
                        || (!selected
                            && (count >= MAX_IMAGES
                                || matches!(loaded.get(&index), Some(Loaded::Failed)))),
                )
                .aria_label(picture.label.clone())
                .child(figure)
                .child(semantic_label(
                    ("selection-picture-label", index),
                    picture.label.clone(),
                    if selected {
                        icons::check()
                    } else {
                        icons::image()
                    },
                ))
                .on_click(cx.listener(move |this, _, _, cx| this.toggle(index, cx))),
            );
        }
        let mut body = v_flex()
            .w_full()
            .min_w_0()
            .gap_4()
            .child(section_heading("selection-mode", icons::auto_fix(), mode))
            .child(
                v_flex()
                    .gap_2()
                    .child(section_heading(
                        "selection-source-label",
                        icons::text_lines(),
                        course2md::tr!("选中的原文"),
                    ))
                    .child(
                        accessible_text("selection-source", self.original.clone())
                            .text_size(TEXT_BODY)
                            .line_height(rems(22. / 14.)),
                    ),
            )
            .child(section_heading(
                "selection-pictures-label",
                icons::image(),
                course2md::tr!("附带截图（可选，最多 4 张）"),
            ))
            .child(
                accessible_text(
                    "selection-picture-count",
                    course2md::tr!("已选择 {} / 4 张", count),
                )
                .text_color(color(GRAY)),
            );
        if self.pictures_loading {
            body = body.child(
                accessible_text(
                    "selection-loading-pictures",
                    course2md::tr!("正在读取截图…"),
                )
                .text_color(color(GRAY)),
            );
        } else if self.pictures.is_empty() {
            body = body.child(
                accessible_text("selection-no-pictures", course2md::tr!("没有提取到截图"))
                    .text_color(color(GRAY)),
            );
        } else {
            body = body.child(gallery);
        }
        if self.pictures.len() > IMAGES_PER_PAGE {
            body = body.child(
                dialog_actions()
                    .child(
                        text_button("selection-previous", cx)
                            .icon(icons::chevron_left())
                            .label(course2md::tr!("上一页"))
                            .disabled(start == 0)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.picture_page = this.picture_page.saturating_sub(1);
                                cx.notify();
                            })),
                    )
                    .child(
                        text_button("selection-next", cx)
                            .icon(icons::chevron_right())
                            .label(course2md::tr!("下一页"))
                            .disabled(end >= self.pictures.len())
                            .on_click(cx.listener(|this, _, _, cx| {
                                if (this.picture_page + 1) * IMAGES_PER_PAGE < this.pictures.len() {
                                    this.picture_page += 1;
                                    cx.notify();
                                }
                            })),
                    ),
            );
        }
        body = body.child(
            v_flex()
                .gap_2()
                .child(section_heading(
                    "selection-instruction-label",
                    icons::edit(),
                    course2md::tr!("附加要求（可选）"),
                ))
                .child(
                    Textarea::new(&self.instruction)
                        .aria_label(course2md::tr!("附加要求（可选）"))
                        .disabled(self.stale),
                ),
        );
        let status = if self.stale {
            Some(course2md::tr!("所选原文已失效，请重新选择"))
        } else if failed {
            Some(course2md::tr!("所选截图无法读取，请取消选择后重试"))
        } else if pending {
            Some(course2md::tr!("正在读取截图…"))
        } else if busy {
            Some(course2md::tr!("正在处理"))
        } else {
            None
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
                        body.id("selection-editor-scroll")
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
                        footer.child(
                            accessible_text("selection-status", status).text_color(color(
                                if self.stale || failed { DANGER } else { GRAY },
                            )),
                        )
                    })
                    .child(
                        accessible_text(
                            "selection-send-hint",
                            course2md::tr!("开始后将把选中文字、所选截图和附加要求发送给 AI。"),
                        )
                        .text_size(TEXT_AUX)
                        .text_color(color(GRAY)),
                    )
                    .child(
                        dialog_actions()
                            .child(
                                text_button("selection-cancel", cx)
                                    .label(course2md::tr!("取消"))
                                    .on_click(
                                        cx.listener(|this, _, window, cx| this.dismiss(window, cx)),
                                    ),
                            )
                            .child(
                                primary_pill("selection-start")
                                    .icon(icons::auto_fix())
                                    .label(course2md::tr!("开始"))
                                    .disabled(self.stale || self.closed.get() || busy || pending)
                                    .on_click(
                                        cx.listener(|this, _, window, cx| this.start(window, cx)),
                                    ),
                            ),
                    ),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::{
        SourceSpan, load_reader_data, notes, same_selection_source, selection_pictures,
        selection_quote,
    };
    use std::rc::Rc;

    fn note() -> (tempfile::TempDir, notes::Preview) {
        let temp = tempfile::tempdir().unwrap();
        let course = notes::fixtures::published_note(
            temp.path(),
            1,
            &[0., 10., 20., 30., 40., 50.],
            Default::default(),
        );
        (temp, notes::read_preview(course).unwrap())
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
    fn freezes_only_valid_ordered_utf8_ranges_across_sections() {
        let (_temp, mut preview) = note();
        preview.document.sections[0].speech[0].text = "前原文后".into();
        preview.document.sections[1].speech[0].text = "second passage".into();
        let spans = [span(0, 3, 9), span(1, 0, 6)];
        let frozen = selection_quote(&preview.document, &spans).unwrap();
        assert_eq!(frozen, "原文\n\nsecond");
        for invalid in [
            vec![],
            vec![span(0, 1, 9)], // Middle of a UTF-8 character.
            vec![span(0, 3, 99)],
            vec![span(0, 3, 3)],
            vec![span(0, 3, 9), span(0, 6, 12)], // Overlap.
            vec![span(1, 0, 6), span(0, 3, 9)],  // Reversed source order.
            vec![span(99, 0, 1)],
        ] {
            assert!(selection_quote(&preview.document, &invalid).is_none());
        }
        preview.document.sections[0].speech[0].text = "新原文后".into();
        assert_eq!(frozen, "原文\n\nsecond");
    }

    #[test]
    fn reload_or_return_to_same_version_never_reuses_the_origin() {
        let (_temp, preview) = note();
        let origin = Rc::new(preview);
        let reloaded = Rc::new(origin.as_ref().clone());
        assert!(same_selection_source(&origin, 4, Some(&origin), 4));
        assert!(!same_selection_source(&origin, 4, Some(&origin), 5));
        assert!(!same_selection_source(&origin, 4, Some(&reloaded), 4));
        assert!(!same_selection_source(&origin, 4, None, 4));
    }

    #[test]
    fn defaults_are_exactly_the_dragged_images_and_keep_over_limit_visible() {
        let (_temp, mut preview) = note();
        let mut data = load_reader_data(&preview);
        let paths: Vec<_> = data
            .frames
            .iter()
            .filter_map(|frame| frame.image.as_ref().map(|image| image.path.clone()))
            .collect();
        assert!(
            selection_pictures(&preview, &data.frames, &[])
                .iter()
                .all(|p| !p.selected)
        );
        let pictures = selection_pictures(&preview, &data.frames, &paths);
        assert_eq!(pictures.len(), 6);
        assert_eq!(
            pictures.iter().filter(|picture| picture.selected).count(),
            6
        );

        // Duplicate manifest entries share one selectable image; missing files
        // and a malicious relative path never become attachment candidates.
        let manifest = preview.course.manifest.as_mut().unwrap();
        manifest.frames.push(manifest.frames[0].clone());
        data.frames.push(data.frames[0].clone());
        data.frames[1].image = None;
        let unavailable = selection_pictures(&preview, &data.frames, &[paths[1].clone()]);
        assert!(
            unavailable
                .iter()
                .any(|picture| picture.path == paths[1] && picture.selected)
        );
        let pictures = selection_pictures(&preview, &data.frames, &[paths[5].clone()]);
        assert_eq!(pictures.len(), 5);
        assert!(pictures[0].selected);
        assert_eq!(
            pictures[0].path,
            data.frames[5].image.as_ref().unwrap().path
        );
        assert_eq!(
            pictures.iter().filter(|picture| picture.selected).count(),
            1
        );

        preview.course.manifest.as_mut().unwrap().frames[0].image = "../outside.png".into();
        let checked = load_reader_data(&preview);
        assert!(checked.frames[0].image.is_none());
        let pictures = selection_pictures(&preview, &checked.frames, &paths);
        assert!(
            pictures
                .iter()
                .all(|picture| picture.path.starts_with(&preview.course.dir))
        );
        assert!(
            pictures
                .iter()
                .all(|picture| !picture.path.ends_with("outside.png"))
        );
    }
}
