//! The processing choices a note is generated with: what the workbench edits
//! for the current input, and what a task's configuration implies.

impl ConversionOptions {
    /// Whether the options use a cloud recognition service.
    pub(crate) fn uses_cloud_provider(&self) -> bool {
        self.provider == Some(course2md::config::AsrProvider::Api)
    }
}

#[cfg(test)]
mod tests {
    use super::ConversionOptions;
    use course2md::{config::TranscriptSource, settings::ConfigFile};

    #[test]
    fn timing_is_frozen_in_subtitle_plans_and_does_not_shift_speech_recognition() {
        let mut options = ConversionOptions {
            subtitle_offset: -1.5,
            ..Default::default()
        };
        let mut config = ConfigFile::default();
        options.apply_to(&mut config);
        assert_eq!(config.defaults.subtitle_offset, Some(-1.5));
        assert_eq!(
            ConversionOptions::from_config(&config).subtitle_offset,
            -1.5
        );
        options.source_mode = TranscriptSource::Asr;
        options.apply_to(&mut config);
        assert_eq!(config.defaults.subtitle_offset, None);
        assert_eq!(options.subtitle_offset, -1.5);
        options.source_mode = TranscriptSource::Subtitle;
        assert_eq!(options.as_planned().subtitle_offset, -1.5);
    }

    #[test]
    fn clearing_subtitle_timing_removes_the_previous_task_config_value() {
        let mut config = ConfigFile::default();
        config.defaults.subtitle_offset = Some(-1.5);
        let mut options = ConversionOptions::from_config(&config);
        assert_eq!(options.as_planned().subtitle_offset, -1.5);
        options.subtitle_offset = 0.0;
        options.apply_to(&mut config);
        assert_eq!(config.defaults.subtitle_offset, None);
        assert_eq!(ConversionOptions::from_config(&config).subtitle_offset, 0.0);
    }
}

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub(crate) struct ConversionOptions {
    #[serde(with = "crate::provider_serde")]
    pub(crate) provider: Option<course2md::config::AsrProvider>,
    #[serde(with = "crate::workspace::text_source_index")]
    pub(crate) source_mode: course2md::config::TranscriptSource,
    /// Seconds added to subtitle timestamps for this video only.
    #[serde(default)]
    pub(crate) subtitle_offset: f64,
    pub(crate) llm: bool,
    #[serde(default)]
    pub(crate) summarize: bool,
    #[serde(default)]
    pub(crate) vision: bool,
    /// Bilingual notes: target language code, or None for no translation.
    #[serde(default)]
    pub(crate) translate: Option<String>,
    pub(crate) keep_video: bool,
    pub(crate) formats: [bool; 3],
}

impl ConversionOptions {
    pub(crate) fn from_config(config: &course2md::settings::ConfigFile) -> Self {
        let provider = config.defaults.provider;
        let source_mode = config.defaults.transcript_source.unwrap_or_default();
        let llm = config.llm.enabled;
        let keep_video = config.defaults.keep_video.unwrap_or(false);
        let formats = config
            .defaults
            .formats
            .as_ref()
            .map(|formats| {
                use course2md::config::OutputFormat::*;
                [
                    formats.contains(&Md),
                    formats.contains(&Html),
                    formats.contains(&Json),
                ]
            })
            .unwrap_or([true, true, false]);
        Self {
            provider,
            source_mode,
            subtitle_offset: config.defaults.subtitle_offset.unwrap_or(0.0),
            llm,
            summarize: config.llm.summarize,
            vision: config.llm.vision,
            translate: config.llm.translate.clone(),
            keep_video,
            formats,
        }
    }
}

impl Default for ConversionOptions {
    fn default() -> Self {
        let mut value = Self::from_config(&Default::default());
        value.formats = [false; 3];
        value
    }
}

impl ConversionOptions {
    /// These options as a task plan records them: the configuration keeps
    /// only what takes effect (vision only with proofreading).
    pub(crate) fn as_planned(&self) -> Self {
        let mut config = course2md::settings::ConfigFile::default();
        self.apply_to(&mut config);
        Self::from_config(&config)
    }
    pub(crate) fn apply_to(&self, config: &mut course2md::settings::ConfigFile) {
        use course2md::config::OutputFormat;
        config.defaults.provider = self.provider;
        config.defaults.transcript_source = Some(self.source_mode);
        config.defaults.subtitle_offset = (self.source_mode
            != course2md::config::TranscriptSource::Asr)
            .then_some(self.subtitle_offset)
            .filter(|offset| *offset != 0.0);
        config.defaults.keep_video = Some(self.keep_video);
        config.defaults.resume = Some(true);
        config.defaults.formats = Some(
            [OutputFormat::Md, OutputFormat::Html, OutputFormat::Json]
                .into_iter()
                .zip(self.formats)
                .filter_map(|(f, selected)| selected.then_some(f))
                .collect(),
        );
        config.llm.enabled = self.llm;
        config.llm.summarize = self.summarize;
        config.llm.vision = self.llm && self.vision;
        config.llm.translate = self.translate.clone();
        config.llm.disable_hint = true;
    }
}
