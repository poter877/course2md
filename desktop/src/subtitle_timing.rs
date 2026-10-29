//! A timing correction belongs to the current video's subtitles, never defaults.
use super::*;
use crate::theme::*;
use anyhow::Context as _;

fn parse_offset(value: &str) -> Result<f64> {
    let offset = value.trim().parse::<f64>().ok().filter(|v| v.is_finite());
    offset.context(course2md::tr!("请输入有效秒数，例如 -1.5 或 2"))
}

struct TimingSource {
    draft_id: String,
    draft_revision: u64,
    subtitle: course2md::subtitle::CachedSubtitle,
}

impl TimingSource {
    fn validate(
        &self,
        draft: Option<&workspace::Draft>,
        preview: Option<&source::Source>,
        loading: bool,
        offset: f64,
    ) -> Result<()> {
        let draft = draft
            .filter(|draft| {
                draft.id == self.draft_id
                    && draft.revision == self.draft_revision
                    && draft.submitted_task.is_none()
            })
            .context(course2md::tr!("当前视频已变化，请重新调整字幕时间"))?;
        // Selecting/rereading subtitles does not advance the video's revision.
        // Check both the visible source and the saved draft before applying.
        let same_subtitle = |source: &source::Source| {
            source.identity == self.subtitle.source_identity
                && source.input == draft.input
                && source.subtitle_request.is_none()
                && source.subtitle_read_error.is_none()
                && source.selected_subtitle.as_ref() == Some(&self.subtitle)
        };
        anyhow::ensure!(
            !loading
                && draft.options.source_mode != course2md::config::TranscriptSource::Asr
                && preview.is_some_and(same_subtitle)
                && draft.source.as_ref().is_some_and(same_subtitle),
            course2md::tr!("当前字幕已变化，请重新打开时间调整")
        );
        let shifted = course2md::subtitle::shift_events(&self.subtitle.events, offset)?;
        anyhow::ensure!(
            !shifted.is_empty(),
            course2md::tr!("调整后没有可用字幕，请检查偏移秒数")
        );
        course2md::execution::validate_events(&shifted)
    }
}

struct SubtitleTimingDialog {
    desktop: Entity<Desktop>,
    source: TimingSource,
    input: Entity<InputState>,
    first_cue: Option<(f64, f64)>,
    error: Option<String>,
    _subscription: Subscription,
}

impl SubtitleTimingDialog {
    fn save(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let offset = match parse_offset(self.input.read(cx).value().as_ref()) {
            Ok(value) => value,
            Err(error) => {
                self.error = Some(error.to_string());
                cx.notify();
                return;
            }
        };
        let result = self.desktop.update(cx, |desktop, cx| {
            self.source.validate(
                desktop.draft(),
                desktop.source_preview.as_ref(),
                desktop.subtitle_loading,
                offset,
            )?;
            anyhow::ensure!(
                desktop.update_task_options(|options| options.subtitle_offset = offset, cx),
                desktop
                    .workspace_error
                    .clone()
                    .unwrap_or_else(|| course2md::tr!("当前输入记录暂时不可用").into())
            );
            cx.notify();
            Ok(())
        });
        match result {
            Ok(()) => window.close_dialog(cx),
            Err(error) => {
                self.error = Some(format!("{error:#}"));
                cx.notify();
            }
        }
    }
}

impl Render for SubtitleTimingDialog {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let preview = self.first_cue.and_then(|(before, end)| {
            let offset = parse_offset(self.input.read(cx).value().as_ref()).ok()?;
            if end + offset <= 0.0 {
                return Some(course2md::tr!("首条字幕移到视频开始前，将被跳过").to_owned());
            }
            let after = before + offset;
            after.is_finite().then(|| {
                course2md::tr!(
                    "首条字幕开始：{} 秒 → {} 秒",
                    format!("{before:.2}"),
                    format!("{:.2}", after.max(0.0))
                )
            })
        });
        v_flex()
            .id("subtitle-timing-dialog")
            .gap_3()
            .child(accessible_text(
                "subtitle-offset-label",
                course2md::tr!("偏移秒数"),
            ))
            .child(text_input(&self.input).aria_label(course2md::tr!("偏移秒数")))
            .child(
                accessible_text(
                    "subtitle-offset-help",
                    course2md::tr!("正数延后，负数提前；仅应用于当前视频的字幕。"),
                )
                .text_color(color(GRAY)),
            )
            .when_some(preview, |view, text| {
                view.child(accessible_text("subtitle-offset-preview", text))
            })
            .when_some(self.error.clone(), |view, error| {
                view.child(
                    accessible_text("subtitle-offset-error", error)
                        .role(Role::Alert)
                        .text_color(color(DANGER)),
                )
            })
            .child(
                dialog_actions()
                    .child(
                        text_button("cancel-subtitle-timing", cx)
                            .label(course2md::tr!("取消"))
                            .on_click(|_, window, cx| window.close_dialog(cx)),
                    )
                    .child(
                        primary_pill("apply-subtitle-timing")
                            .icon(icons::check())
                            .label(course2md::tr!("应用"))
                            .on_click(cx.listener(|this, _, window, cx| this.save(window, cx))),
                    ),
            )
    }
}

impl Desktop {
    pub(crate) fn open_subtitle_timing(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(draft) = self.draft().filter(|draft| draft.submitted_task.is_none()) else {
            return;
        };
        let draft_id = draft.id.clone();
        let draft_revision = draft.revision;
        let value = draft.options.subtitle_offset.to_string();
        let Some(subtitle) = self
            .source_preview
            .as_ref()
            .and_then(|source| source.selected_subtitle.clone())
        else {
            return;
        };
        let first_cue = subtitle
            .events
            .first()
            .map(|event| (event.start, event.end));
        let source = TimingSource {
            draft_id,
            draft_revision,
            subtitle,
        };
        let input = cx.new(|cx| InputState::new(window, cx).default_value(value));
        let focus = input.clone();
        let desktop = cx.entity();
        let content = cx.new(|cx| {
            let subscription = cx.subscribe_in(
                &input,
                window,
                |this: &mut SubtitleTimingDialog, _, event, window, cx| {
                    if matches!(event, InputEvent::PressEnter { .. }) {
                        this.save(window, cx);
                    }
                    if matches!(event, InputEvent::Change) {
                        this.error = None;
                        cx.notify();
                    }
                },
            );
            SubtitleTimingDialog {
                desktop,
                source,
                input,
                first_cue,
                error: None,
                _subscription: subscription,
            }
        });
        window.open_dialog(cx, move |dialog, window, _| {
            dialog
                .material("subtitle-timing-title", course2md::tr!("调整字幕时间"))
                .close_button(false)
                .overlay_closable(false)
                .w(dialog_width(DIALOG_WIDTH, window))
                .child(content.clone())
        });
        window.defer(cx, move |window, cx| {
            focus.update(cx, |state, cx| state.focus(window, cx))
        });
    }
}

#[cfg(test)]
mod tests {
    use super::{TimingSource, parse_offset};
    use crate::{source::Source, workspace::Draft};
    use course2md::{
        config::TranscriptSource, subtitle::CachedSubtitle, timeline::TranscriptEvent,
    };

    fn fixture() -> (TimingSource, Draft, Source) {
        let subtitle = CachedSubtitle {
            normalization_version: course2md::subtitle::NORMALIZATION_VERSION,
            source_identity: "video-a".into(),
            track_id: "track-a".into(),
            label: "captions".into(),
            path: "captions.srt".into(),
            events: [(1.0, 3.0), (5.0, 7.0)]
                .into_iter()
                .map(|(start, end)| TranscriptEvent {
                    start,
                    end,
                    text: "caption".into(),
                    raw: None,
                    translation: None,
                })
                .collect(),
        };
        let source = Source {
            input: "video.mp4".into(),
            identity: subtitle.source_identity.clone(),
            selected_subtitle: Some(subtitle.clone()),
            ..Default::default()
        };
        let mut draft = Draft::new(false, "library".into(), Default::default());
        draft.change_source(source.input.clone());
        draft.source = Some(source.clone());
        draft.options.subtitle_offset = 0.5;
        let snapshot = TimingSource {
            draft_id: draft.id.clone(),
            draft_revision: draft.revision,
            subtitle,
        };
        (snapshot, draft, source)
    }

    #[test]
    fn timing_accepts_signed_fractions_but_rejects_nonfinite_values() {
        assert_eq!(parse_offset(" -1.5 ").unwrap(), -1.5);
        assert_eq!(parse_offset("+2").unwrap(), 2.0);
        for value in ["", "-", "NaN", "inf", "-inf", "1e999"] {
            assert!(parse_offset(value).is_err());
        }
    }

    #[test]
    fn timing_validates_clipping_without_shifting_saved_subtitles() {
        let (snapshot, draft, source) = fixture();
        let original = draft.clone();
        for offset in [0.0, -1.5, -3.0, 2.0] {
            snapshot
                .validate(Some(&draft), Some(&source), false, offset)
                .unwrap();
        }
        assert_eq!(draft, original);
        assert_eq!(source.selected_subtitle.as_ref(), Some(&snapshot.subtitle));
    }

    #[test]
    fn timing_rejects_empty_or_invalid_shift_before_changing_options() {
        let (snapshot, draft, source) = fixture();
        for offset in [-7.0, -1e308, 1e308, f64::INFINITY, f64::NAN] {
            assert!(
                snapshot
                    .validate(Some(&draft), Some(&source), false, offset)
                    .is_err()
            );
            assert_eq!(draft.options.subtitle_offset, 0.5);
        }
        let (mut snapshot, mut draft, mut source) = fixture();
        snapshot.subtitle.events[0].start = f64::MAX / 2.0;
        snapshot.subtitle.events[0].end = f64::MAX;
        source.selected_subtitle = Some(snapshot.subtitle.clone());
        draft.source = Some(source.clone());
        assert!(
            snapshot
                .validate(Some(&draft), Some(&source), false, f64::MAX)
                .is_err()
        );
        snapshot.subtitle.events[0].start = -1.0;
        source.selected_subtitle = Some(snapshot.subtitle.clone());
        draft.source = Some(source.clone());
        assert!(
            snapshot
                .validate(Some(&draft), Some(&source), false, 0.0)
                .is_err()
        );
    }

    #[test]
    fn timing_rejects_subtitle_replacement_even_without_a_video_revision_change() {
        let (snapshot, draft, source) = fixture();
        for changed in 0..3 {
            let mut replacement = source.clone();
            let subtitle = replacement.selected_subtitle.as_mut().unwrap();
            match changed {
                0 => subtitle.track_id = "track-b".into(),
                1 => subtitle.events[0].text = "reread caption".into(),
                _ => subtitle.events[0].start = 2.0,
            }
            assert!(
                snapshot
                    .validate(Some(&draft), Some(&replacement), false, 1.0)
                    .is_err()
            );
            let mut replaced_draft = draft.clone();
            replaced_draft.source = Some(replacement.clone());
            assert_eq!(replaced_draft.revision, snapshot.draft_revision);
            assert!(
                snapshot
                    .validate(Some(&replaced_draft), Some(&replacement), false, 1.0)
                    .is_err()
            );
            assert!(
                snapshot
                    .validate(Some(&replaced_draft), Some(&source), false, 1.0)
                    .is_err()
            );
        }
    }

    #[test]
    fn timing_rejects_pending_subtitles_and_changed_or_submitted_drafts() {
        let (snapshot, draft, source) = fixture();
        assert!(
            snapshot
                .validate(Some(&draft), Some(&source), true, 1.0)
                .is_err()
        );
        assert!(snapshot.validate(None, Some(&source), false, 1.0).is_err());
        assert!(snapshot.validate(Some(&draft), None, false, 1.0).is_err());
        let mut pending = source.clone();
        pending.subtitle_request = Some(course2md::subtitle::file_track("other.srt".into(), None));
        assert!(
            snapshot
                .validate(Some(&draft), Some(&pending), false, 1.0)
                .is_err()
        );
        for changed in 0..4 {
            let mut draft = draft.clone();
            match changed {
                0 => draft.id = "other-draft".into(),
                1 => draft.revision += 1,
                2 => draft.submitted_task = Some("submitted".into()),
                _ => draft.options.source_mode = TranscriptSource::Asr,
            }
            assert!(
                snapshot
                    .validate(Some(&draft), Some(&source), false, 1.0)
                    .is_err()
            );
        }
    }
}
