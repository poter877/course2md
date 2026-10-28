//! 视频帧 ↔ 幻灯片页对齐：视觉相似度定时间，字幕文本做校验与边界微调。
//!
//! 视觉一路用灰度 SSIM 选取可靠锚点，允许跳页与动画重复帧；
//! 文本一路把每页 `pdftotext` 的文字和该时间段内的字幕做加权词重叠，并在
//! ±几秒内移动页边界，让“这一页说的内容”与“这一页显示的文字”尽量一致。
//! 两路都弱的页标 `uncertain`，写进 alignment.json，渲染时也提示待核对。

use crate::dispatch::TaskControl;
use crate::slides::SlidePage;
use crate::timeline::{FrameEvent, TranscriptEvent};
use anyhow::{Context, Result};
use image::{GrayImage, imageops::FilterType};
use image_compare::Algorithm;
use std::collections::{HashMap, HashSet};
use std::path::Path;

/// 匹配时统一到的宽度；和 slides.rs 的缩略图一致。
const MATCH_WIDTH: u32 = 320;
/// 边界微调窗口（秒）。
const REFINE_WINDOW: f64 = 3.0;
/// 边界微调步长（秒）。
const REFINE_STEP: f64 = 0.5;
/// 文本分降到这个比例以下、且视觉也不高时，标记 uncertain。
const WEAK_TEXT: f64 = 0.2;
/// 单帧最佳匹配低于此分就不当“锚点”：黑屏、过渡帧或画面异常不应
/// 把对齐拖偏；短过渡继承邻页，长不匹配区间保留视频画面。
const ANCHOR_SCORE: f64 = 0.5;
/// 最佳页必须比第二名高出这个分差，单帧噪声才不会被当成新的一页。
const ANCHOR_MARGIN: f64 = 0.03;
/// 一段连续对不上任何 PDF 页的画面，超过这个时长就保留为视频截图。
const VIDEO_MIN_SECS: f64 = 4.0;

/// 一页在时间轴上的位次。
#[derive(Debug, Clone, serde::Serialize)]
pub struct SlideSpan {
    /// `pages` 里的下标；`None` 表示这段画面在 PDF 里没有对应页。
    pub page: Option<usize>,
    pub deck: Option<usize>,
    pub page_number: Option<u32>,
    /// 成图路径：PDF 页用 `slides/...`，视频独有画面用 `frames/...`。
    pub image: String,
    /// 这段画面第一次出现在屏幕上的时间（秒）
    pub t: f64,
    /// 视觉匹配分（0–1，运行内相对值）
    pub visual: f64,
    /// 文本校验分（0–1）
    pub text: f64,
    /// 对上的页面关键词（按权重取前 8 个）
    pub keywords: Vec<String>,
    pub uncertain: bool,
}

/// 对齐入口。PDF 页优先；连续一段画面和任何页都对不上时，保留视频截图。
pub fn align(
    work: &Path,
    frames: &[FrameEvent],
    pages: &[SlidePage],
    speech: &[TranscriptEvent],
    media_end: f64,
    threshold: f64,
    control: &TaskControl,
) -> Result<Vec<SlideSpan>> {
    control.check()?;
    if frames.is_empty() || pages.is_empty() {
        return Ok(Vec::new());
    }
    let frame_gray: Vec<GrayImage> = frames
        .iter()
        .map(|frame| {
            control.check()?;
            let path = work.join(&frame.image);
            let img = image::open(&path)
                .with_context(|| crate::tr!("无法读取截图用于对齐：{}", path.display()))?
                .to_luma8();
            Ok(fit_width(&img, MATCH_WIDTH))
        })
        .collect::<Result<_>>()?;

    let scores = score_frames(&frame_gray, pages, control)?;
    // 每行只保留前两名，避免保存完整矩阵或对全部 PDF 页排序。
    let page_of: Vec<Option<usize>> = scores
        .iter()
        .map(|score| {
            (score.best >= ANCHOR_SCORE && score.best - score.second >= ANCHOR_MARGIN)
                .then_some(score.page)
        })
        .collect();

    let mut spans: Vec<SlideSpan> = Vec::new();
    let mut i = 0usize;
    while i < frames.len() {
        control.check()?;
        if let Some(page) = page_of[i] {
            let start = i;
            while i < frames.len() && page_of[i] == Some(page) {
                i += 1;
            }
            let visual = (start..i).map(|k| scores[k].best).fold(0.0f64, f64::max);
            spans.push(slide_span(page, pages, frames[start].t, visual));
        } else {
            let start = i;
            while i < frames.len() && page_of[i].is_none() {
                i += 1;
            }
            let end_t = frames.get(i).map_or(media_end, |frame| frame.t);
            let visual = (start..i).map(|k| scores[k].best).fold(0.0f64, f64::max);
            if end_t - frames[start].t >= VIDEO_MIN_SECS {
                // 每张已检测的画面都有自己的时间归属；整段只留首帧会丢掉
                // 后续演示/白板步骤，并把这些步骤的字幕错归到首张截图。
                for k in start..i {
                    spans.push(video_span(&frames[k], scores[k].best));
                }
            } else if let Some(prev) = spans.last().and_then(|span| span.page) {
                // 短过渡帧：继承上一页，不单独成段。
                spans.push(slide_span_with(prev, pages, frames[start].t, visual));
            } else if let Some((_, next)) = page_of
                .iter()
                .enumerate()
                .skip(i)
                .find_map(|(index, page)| page.map(|page| (index, page)))
            {
                spans.push(slide_span_with(next, pages, frames[start].t, visual));
            } else {
                for k in start..i {
                    spans.push(video_span(&frames[k], scores[k].best));
                }
            }
        }
    }
    // 相邻同页合并（继承段可能造成重复）。
    let mut merged: Vec<SlideSpan> = Vec::new();
    for span in spans {
        if let Some(last) = merged.last_mut()
            && last.page.is_some()
            && last.page == span.page
        {
            last.visual = last.visual.max(span.visual);
            continue;
        }
        merged.push(span);
    }
    if merged.is_empty() {
        return Ok(Vec::new());
    }
    // 去掉“夹在两段同一套之间”的单页错配，以及第二套开始后倒回第一套的尾页。
    let mut filtered: Vec<SlideSpan> = Vec::new();
    let mut max_deck = 0usize;
    for (index, span) in merged.iter().enumerate() {
        let Some(deck) = span.deck else {
            filtered.push(span.clone());
            continue;
        };
        let prev_deck = filtered.last().and_then(|last| last.deck);
        let next_deck = merged.get(index + 1).and_then(|next| next.deck);
        let isolated = prev_deck.is_some_and(|prev| prev != deck)
            && next_deck.is_some_and(|next| next != deck);
        if isolated || deck < max_deck {
            continue;
        }
        max_deck = max_deck.max(deck);
        filtered.push(span.clone());
    }
    let mut spans = filtered;
    let index = TextIndex::new(pages, speech, control)?;
    refine_boundaries(&mut spans, &index, media_end, control)?;
    let ends: Vec<f64> = (0..spans.len())
        .map(|i| spans.get(i + 1).map_or(media_end, |next| next.t))
        .collect();
    for (i, span) in spans.iter_mut().enumerate() {
        control.check()?;
        let Some(page) = span.page else {
            span.uncertain = span.visual < threshold;
            continue;
        };
        let (score, keywords) = index.score(page, span.t, ends[i], true);
        span.text = score;
        span.keywords = keywords;
        span.uncertain = span.visual < threshold && score < WEAK_TEXT;
    }
    Ok(spans)
}

fn slide_span(page: usize, pages: &[SlidePage], t: f64, visual: f64) -> SlideSpan {
    slide_span_with(page, pages, t, visual)
}

fn slide_span_with(page: usize, pages: &[SlidePage], t: f64, visual: f64) -> SlideSpan {
    SlideSpan {
        page: Some(page),
        deck: Some(pages[page].deck),
        page_number: Some(pages[page].page),
        image: pages[page].image.clone(),
        t,
        visual,
        text: 0.0,
        keywords: Vec::new(),
        uncertain: false,
    }
}

fn video_span(frame: &FrameEvent, visual: f64) -> SlideSpan {
    SlideSpan {
        page: None,
        deck: None,
        page_number: None,
        image: frame.image.clone(),
        t: frame.t,
        visual,
        text: 0.0,
        keywords: Vec::new(),
        uncertain: false,
    }
}

#[derive(Clone, Copy)]
struct FrameScore {
    page: usize,
    best: f64,
    second: f64,
}

impl Default for FrameScore {
    fn default() -> Self {
        Self {
            page: 0,
            best: f64::NEG_INFINITY,
            second: 0.0,
        }
    }
}

impl FrameScore {
    fn consider(&mut self, page: usize, score: f64) {
        if score > self.best || (score == self.best && page < self.page) {
            self.second = self.best.max(0.0);
            self.best = score;
            self.page = page;
        } else {
            self.second = self.second.max(score);
        }
    }
}

/// 按帧并行。每帧只按 PDF 的不同尺寸缩放一次，而不是每页重复缩放；
/// 同尺寸时直接借用。只保存前两名，内存由 O(帧数 × 页数) 降为 O(帧数)。
fn score_frames(
    frames: &[GrayImage],
    pages: &[SlidePage],
    control: &TaskControl,
) -> Result<Vec<FrameScore>> {
    control.check()?;
    let mut groups: HashMap<(u32, u32), Vec<usize>> = HashMap::new();
    for (i, page) in pages.iter().enumerate() {
        groups.entry(page.gray.dimensions()).or_default().push(i);
    }
    let workers = std::thread::available_parallelism()
        .map(|value| value.get())
        .unwrap_or(4)
        .clamp(1, 8);
    let mut scores = vec![FrameScore::default(); frames.len()];
    let chunk = frames.len().div_ceil(workers).max(1);
    std::thread::scope(|scope| -> Result<()> {
        let mut handles = Vec::new();
        for (index, rows) in scores.chunks_mut(chunk).enumerate() {
            let start = index * chunk;
            let frames = &frames[start..start + rows.len()];
            let groups = &groups;
            handles.push(scope.spawn(move || -> Result<()> {
                for (row, frame) in rows.iter_mut().zip(frames) {
                    control.check()?;
                    for (&(w, h), indices) in groups {
                        control.check()?;
                        let resized;
                        let left = if frame.dimensions() == (w, h) {
                            frame
                        } else {
                            resized = fit(frame, w, h);
                            &resized
                        };
                        for (n, &page) in indices.iter().enumerate() {
                            if n.is_multiple_of(16) {
                                control.check()?;
                            }
                            let score = image_compare::gray_similarity_structure(
                                &Algorithm::MSSIMSimple,
                                left,
                                &pages[page].gray,
                            )
                            .map(|s| s.score)
                            .unwrap_or(0.0);
                            row.consider(page, score);
                        }
                    }
                }
                Ok(())
            }));
        }
        for handle in handles {
            handle.join().expect("alignment worker panicked")?;
        }
        Ok(())
    })?;
    control.check()?;
    Ok(scores)
}

fn fit(image: &GrayImage, width: u32, height: u32) -> GrayImage {
    let (width, height) = (width.max(1), height.max(1));
    if image.width() == width && image.height() == height {
        return image.clone();
    }
    image::imageops::resize(image, width, height, FilterType::Triangle)
}

fn fit_width(image: &GrayImage, width: u32) -> GrayImage {
    let height = ((image.height() as f64 * width as f64 / image.width().max(1) as f64).round()
        as u32)
        .max(2);
    fit(image, width, height)
}

/// 页面文字 → 词表：ASR 常见错词先纠正，再切词、去停用词。
struct TextIndex {
    page_tokens: Vec<HashSet<String>>,
    idf: HashMap<String, f64>,
    page_weights: Vec<f64>,
    speech: Vec<(f64, HashSet<String>)>,
}

impl TextIndex {
    fn new(pages: &[SlidePage], speech: &[TranscriptEvent], control: &TaskControl) -> Result<Self> {
        let mut page_tokens: Vec<HashSet<String>> = Vec::with_capacity(pages.len());
        for page in pages {
            control.check()?;
            page_tokens.push(normalize(&page.text).into_iter().collect());
        }
        let total = pages.len().max(1) as f64;
        let mut df: HashMap<String, usize> = HashMap::new();
        for tokens in &page_tokens {
            for token in tokens {
                *df.entry(token.clone()).or_insert(0) += 1;
            }
        }
        let idf: HashMap<String, f64> = df
            .into_iter()
            .map(|(token, count)| (token, (1.0 + total / count as f64).ln()))
            .collect();
        let page_weights = page_tokens
            .iter()
            .map(|tokens| tokens.iter().map(|token| idf[token]).sum())
            .collect();
        let mut spoken = Vec::with_capacity(speech.len());
        for event in speech {
            control.check()?;
            spoken.push((
                (event.start + event.end) / 2.0,
                normalize(&event.text).into_iter().collect(),
            ));
        }
        Ok(Self {
            page_tokens,
            idf,
            page_weights,
            speech: spoken,
        })
    }

    /// 时间段内字幕与本页文字的加权重叠：对上的词权重 / 本页词总权重。
    fn score(
        &self,
        page_idx: usize,
        start: f64,
        end: f64,
        with_keywords: bool,
    ) -> (f64, Vec<String>) {
        let page = &self.page_tokens[page_idx];
        let spoken: HashSet<&String> = self
            .speech
            .iter()
            .filter(|(mid, _)| *mid >= start && *mid < end)
            .flat_map(|(_, tokens)| tokens.iter())
            .collect();
        let mut matched: Vec<(&String, f64)> = Vec::new();
        let mut matched_weight = 0.0;
        for token in page {
            if spoken.contains(token) {
                let weight = self.idf[token];
                matched_weight += weight;
                if with_keywords {
                    matched.push((token, weight));
                }
            }
        }
        let total_weight = self.page_weights[page_idx];
        let score = if total_weight > 0.05 {
            (matched_weight / total_weight).min(1.0)
        } else {
            0.5
        };
        matched.sort_unstable_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(b.0)));
        let keywords = matched
            .into_iter()
            .take(8)
            .map(|(token, _)| token.clone())
            .collect();
        (score, keywords)
    }
}

/// 把每个页边界在 ±REFINE_WINDOW 秒内移动，使相邻两页的文本分之和最大；
/// 分数打平时保持原边界（否则会被“最早候选”一路推到窗口边缘）。
fn refine_boundaries(
    spans: &mut [SlideSpan],
    index: &TextIndex,
    media_end: f64,
    control: &TaskControl,
) -> Result<()> {
    control.check()?;
    for i in 1..spans.len() {
        control.check()?;
        let center = spans[i].t;
        let prev = spans[i - 1].t;
        let next = spans.get(i + 1).map_or(media_end, |span| span.t);
        let (Some(left_page), Some(right_page)) = (spans[i - 1].page, spans[i].page) else {
            continue;
        };
        let lower = (prev + 1.0).max(center - REFINE_WINDOW);
        let upper = (next - 1.0).min(center + REFINE_WINDOW);
        if upper <= lower {
            continue;
        }
        let score_at = |t: f64| {
            index.score(left_page, prev, t, false).0 + index.score(right_page, t, next, false).0
        };
        let mut best = score_at(center);
        let mut best_t = center;
        let mut offset = -REFINE_WINDOW;
        while offset <= REFINE_WINDOW + 1e-9 {
            control.check()?;
            let candidate = (center + offset).clamp(lower, upper);
            let score = score_at(candidate);
            let closer = (candidate - center).abs() < (best_t - center).abs() - 1e-9;
            if score > best + 1e-6 || ((score - best).abs() <= 1e-6 && closer) {
                best = score;
                best_t = candidate;
            }
            offset += REFINE_STEP;
        }
        spans[i].t = best_t;
    }
    Ok(())
}

/// 字幕与页文字的归一化：小写、纠正常见 ASR 错词、按非字母数字切词。
fn normalize(text: &str) -> Vec<String> {
    let mut lowered = text.to_lowercase();
    for (from, to) in CORRECTIONS {
        if lowered.contains(from) {
            lowered = lowered.replace(from, to);
        }
    }
    lowered
        .split(|ch: char| !ch.is_alphanumeric())
        .filter(|word| word.len() >= 2 && !STOPWORDS.contains(word))
        .map(str::to_string)
        .collect()
}

const CORRECTIONS: &[(&str, &str)] = &[
    ("i lock", "iloc"),
    ("eyelock", "iloc"),
    ("eyilock", "iloc"),
    ("do i lock", "loc"),
    ("dotlock", "loc"),
    ("dolock", "loc"),
    ("data frame", "dataframe"),
    ("pendas", "pandas"),
    ("numpai", "numpy"),
    ("group by", "groupby"),
    ("drop na", "dropna"),
    ("fill na", "fillna"),
    ("pie table", "pivot table"),
    ("pivot table", "pivottable"),
    ("scikit learn", "sklearn"),
    ("mat plot lib", "matplotlib"),
];

const STOPWORDS: &[&str] = &[
    "the",
    "and",
    "for",
    "you",
    "that",
    "this",
    "with",
    "have",
    "are",
    "was",
    "not",
    "but",
    "can",
    "will",
    "from",
    "they",
    "what",
    "when",
    "where",
    "how",
    "why",
    "all",
    "any",
    "our",
    "your",
    "its",
    "his",
    "her",
    "them",
    "then",
    "than",
    "there",
    "here",
    "into",
    "onto",
    "about",
    "just",
    "like",
    "know",
    "want",
    "get",
    "got",
    "going",
    "one",
    "two",
    "three",
    "say",
    "said",
    "see",
    "okay",
    "right",
    "good",
    "great",
    "cool",
    "well",
    "actually",
    "really",
    "basically",
    "kind",
    "sort",
    "thing",
    "things",
    "some",
    "much",
    "many",
    "more",
    "most",
    "other",
    "such",
    "very",
];

#[cfg(test)]
mod tests {
    use super::*;

    fn page(gray: GrayImage, text: &str) -> SlidePage {
        SlidePage {
            deck: 1,
            page: 1,
            image: "slides/page.jpg".into(),
            text: text.into(),
            gray,
        }
    }

    fn frame(dir: &Path, t: f64, brightness: u8) -> FrameEvent {
        let image = format!("frame-{t}-{brightness}.png");
        GrayImage::from_pixel(320, 180, image::Luma([brightness]))
            .save(dir.join(&image))
            .unwrap();
        FrameEvent { t, image }
    }

    #[test]
    fn unmatched_run_keeps_each_frame_and_its_subtitle_interval() {
        let dir = tempfile::tempdir().unwrap();
        let pages = [page(
            GrayImage::from_pixel(320, 180, image::Luma([255])),
            "",
        )];
        // 白色 PDF → 三步暗色演示 → PDF；最后一张演示虽短也属于整个区间。
        let frames = vec![
            frame(dir.path(), 0.0, 255),
            frame(dir.path(), 2.0, 0),
            frame(dir.path(), 4.0, 20),
            frame(dir.path(), 7.0, 40),
            frame(dir.path(), 8.0, 255),
        ];
        let speech = crate::subtitle::parse_subtitle(
            "1\n00:00:02,100 --> 00:00:02,900\nfirst\n\n2\n00:00:04,000 --> 00:00:05,000\nsecond\n\n3\n00:00:07,000 --> 00:00:07,800\nthird\n\n4\n00:00:08,000 --> 00:00:09,000\nrestored\n",
        );
        let spans = align(
            dir.path(),
            &frames,
            &pages,
            &speech,
            10.0,
            0.6,
            &TaskControl::default(),
        )
        .unwrap();
        assert_eq!(
            spans.iter().map(|s| s.t).collect::<Vec<_>>(),
            [0.0, 2.0, 4.0, 7.0, 8.0]
        );
        assert_eq!(
            spans.iter().map(|s| s.page).collect::<Vec<_>>(),
            [Some(0), None, None, None, Some(0)]
        );
        for i in 1..4 {
            assert_eq!(spans[i].image, frames[i].image);
        }
        let sections = crate::timeline::merge(
            spans
                .iter()
                .map(|s| FrameEvent {
                    t: s.t,
                    image: s.image.clone(),
                })
                .collect(),
            speech,
            10.0,
        );
        assert!(sections[0].speech.is_empty());
        for (section, word) in sections[1..]
            .iter()
            .zip(["first", "second", "third", "restored"])
        {
            assert_eq!(section.speech.len(), 1);
            assert_eq!(section.speech[0].text, word);
        }
        assert_eq!(sections[3].end, 8.0);
        assert!(
            spans[1].visual < spans[3].visual,
            "each frame retains its own confidence"
        );
    }

    #[test]
    fn unmatched_tail_and_short_unmatched_clip_keep_all_frames() {
        let dir = tempfile::tempdir().unwrap();
        let pages = [page(
            GrayImage::from_pixel(320, 180, image::Luma([255])),
            "",
        )];
        for (frames, end) in [
            (
                vec![
                    frame(dir.path(), 0.0, 255),
                    frame(dir.path(), 2.0, 0),
                    frame(dir.path(), 4.0, 20),
                ],
                9.0,
            ),
            (
                vec![frame(dir.path(), 0.0, 0), frame(dir.path(), 1.0, 20)],
                2.0,
            ),
        ] {
            let spans = align(
                dir.path(),
                &frames,
                &pages,
                &[],
                end,
                0.6,
                &TaskControl::default(),
            )
            .unwrap();
            assert_eq!(spans.len(), frames.len());
            for (span, frame) in spans.iter().zip(&frames) {
                assert_eq!(span.t, frame.t);
                if span.page.is_none() {
                    assert_eq!(span.image, frame.image);
                }
            }
        }
    }

    #[test]
    fn short_transition_merges_with_adjacent_pdf_without_moving_onset() {
        let dir = tempfile::tempdir().unwrap();
        let pages = [page(
            GrayImage::from_pixel(320, 180, image::Luma([255])),
            "",
        )];
        let frames = vec![
            frame(dir.path(), 0.0, 255),
            frame(dir.path(), 2.0, 0),
            frame(dir.path(), 3.0, 255),
        ];
        let spans = align(
            dir.path(),
            &frames,
            &pages,
            &[],
            6.0,
            0.6,
            &TaskControl::default(),
        )
        .unwrap();
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].t, 0.0);
        assert_eq!(spans[0].page, Some(0));
    }

    #[test]
    fn cached_dimensions_and_linear_ranking_match_pairwise_reference() {
        let frames: Vec<_> = [0u32, 5, 13]
            .into_iter()
            .map(|offset| {
                GrayImage::from_fn(80, 40, |x, y| {
                    image::Luma([((x * 7 + y * 11 + offset) % 256) as u8])
                })
            })
            .collect();
        let pages: Vec<_> = [
            (32, 18, 0),
            (32, 18, 1),
            (24, 32, 2),
            (80, 40, 0),
            (32, 18, 2),
        ]
        .into_iter()
        .map(|(w, h, i)| page(fit(&frames[i], w, h), ""))
        .collect();
        let scores = score_frames(&frames, &pages, &TaskControl::default()).unwrap();
        for (frame, actual) in frames.iter().zip(scores) {
            let mut reference: Vec<_> = pages
                .iter()
                .enumerate()
                .map(|(i, p)| {
                    let resized = fit(frame, p.gray.width(), p.gray.height());
                    (
                        i,
                        image_compare::gray_similarity_structure(
                            &Algorithm::MSSIMSimple,
                            &resized,
                            &p.gray,
                        )
                        .unwrap()
                        .score,
                    )
                })
                .collect();
            reference.sort_by(|a, b| b.1.total_cmp(&a.1));
            assert_eq!(actual.page, reference[0].0);
            assert!((actual.best - reference[0].1).abs() < 1e-12);
            assert!((actual.second - reference[1].1).abs() < 1e-12);
        }
        let duplicates = vec![page(frames[0].clone(), ""), page(frames[0].clone(), "")];
        let tied = score_frames(&frames[..1], &duplicates, &TaskControl::default()).unwrap();
        assert_eq!(tied[0].page, 0);
        assert_eq!(tied[0].best, tied[0].second);
    }

    #[test]
    fn text_scores_use_half_open_midpoint_intervals_and_stable_ties() {
        let pages = [
            page(GrayImage::new(32, 18), "alpha"),
            page(GrayImage::new(32, 18), "beta"),
        ];
        let speech = vec![TranscriptEvent {
            start: 4.0,
            end: 6.0,
            text: "beta".into(),
            raw: None,
            translation: None,
        }];
        let control = TaskControl::default();
        let index = TextIndex::new(&pages, &speech, &control).unwrap();
        assert_eq!(index.score(1, 0.0, 5.0, true).0, 0.0);
        assert_eq!(index.score(1, 5.0, 10.0, true), (1.0, vec!["beta".into()]));
        assert_eq!(index.score(1, 5.0, 10.0, false), (1.0, vec![]));
        let mut spans = vec![
            slide_span(0, &pages, 0.0, 1.0),
            slide_span(1, &pages, 5.0, 1.0),
        ];
        refine_boundaries(&mut spans, &index, 10.0, &control).unwrap();
        assert_eq!(
            spans[1].t, 5.0,
            "equal text scores must keep the visual boundary"
        );
    }

    #[test]
    fn parallel_scoring_observes_cancellation_after_work_begins() {
        use crate::dispatch::{ControlSource, Failure};
        use std::time::{Duration, Instant};
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("control.json");
        let control = TaskControl::open(
            dir.path(),
            ControlSource {
                control_path: Some(path.clone()),
                ..Default::default()
            },
        )
        .unwrap();
        let frames = vec![GrayImage::from_pixel(320, 180, image::Luma([80])); 32];
        let pages: Vec<_> = (0..256).map(|_| page(frames[0].clone(), "")).collect();
        let started = Instant::now();
        std::thread::scope(|scope| {
            scope.spawn(|| {
                std::thread::sleep(Duration::from_millis(50));
                crate::checkpoint::atomic_write(&path, br#"{"intent":"cancel"}"#).unwrap();
            });
            let error = score_frames(&frames, &pages, &control)
                .err()
                .expect("work must stop on cancel");
            assert!(error.downcast_ref::<Failure>().is_some());
        });
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "workers must not finish the whole matrix before cancelling"
        );
    }

    #[test]
    fn cancelled_alignment_checks_before_io_and_scoring() {
        use crate::dispatch::{ControlSource, Failure};
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("control.json");
        std::fs::write(&path, r#"{"intent":"cancel"}"#).unwrap();
        let control = TaskControl::open(
            dir.path(),
            ControlSource {
                control_path: Some(path),
                ..Default::default()
            },
        )
        .unwrap();
        let frames = [FrameEvent {
            t: 0.0,
            image: "missing.png".into(),
        }];
        let pages = [page(GrayImage::new(32, 18), "")];
        let error = align(dir.path(), &frames, &pages, &[], 10.0, 0.6, &control).unwrap_err();
        assert!(error.downcast_ref::<Failure>().is_some());
        let error = score_frames(&[GrayImage::new(32, 18)], &pages, &control)
            .err()
            .unwrap();
        assert!(error.downcast_ref::<Failure>().is_some());
    }

    #[test]
    fn normalizes_asr_mishearings_and_stopwords() {
        let tokens = normalize("I lock and the dotlock: pendas data frame, group by!");
        assert!(tokens.contains(&"iloc".to_string()));
        assert!(tokens.contains(&"loc".to_string()));
        assert!(tokens.contains(&"pandas".to_string()));
        assert!(tokens.contains(&"dataframe".to_string()));
        assert!(tokens.contains(&"groupby".to_string()));
        assert!(!tokens.contains(&"the".to_string()));
        assert!(!tokens.contains(&"and".to_string()));
    }
}
