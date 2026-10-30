//! `AsrEvalTask`: the speech-recognition dataset an `asr_eval` names, loose on
//! the wire and strict inside as [`super::VlmEvalTask`] is, since the evals
//! server may hold datasets this client doesn't know yet.

use serde::{Deserialize, Serialize};
use strum::{AsRefStr, Display, EnumString, VariantArray};

/// The datasets the evals server holds a pinned ASR set for, spelled exactly as
/// its `POST /score` spells them, in its order: the nine public test sets of
/// the Open ASR Leaderboard's English average (huggingface/open_asr_leaderboard),
/// in the leaderboard's column order, then seven `google/fleurs` locales:
/// English and the languages of the leaderboard's multilingual FLEURS runs
/// (German, French, Italian, Spanish, Portuguese, Dutch; its Armenian is left
/// out), a selection that is pipette's own. The English sets keep the
/// `asr_esb_` prefix of the leaderboard's lineage (the ESB benchmark, arXiv
/// 2210.13352) so the ids stay stable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Display, AsRefStr, EnumString, VariantArray)]
pub enum KnownAsrEvalTask {
    #[strum(serialize = "asr_esb_ami_cleaned")]
    AsrEsbAmiCleaned,
    #[strum(serialize = "asr_esb_earnings22_cleaned_aa_chunked")]
    AsrEsbEarnings22CleanedAaChunked,
    #[strum(serialize = "asr_esb_gigaspeech_cleaned")]
    AsrEsbGigaspeechCleaned,
    #[strum(serialize = "asr_esb_librispeech_test_clean")]
    AsrEsbLibrispeechTestClean,
    #[strum(serialize = "asr_esb_librispeech_test_other")]
    AsrEsbLibrispeechTestOther,
    #[strum(serialize = "asr_esb_spgispeech")]
    AsrEsbSpgispeech,
    #[strum(serialize = "asr_esb_urgent2024")]
    AsrEsbUrgent2024,
    #[strum(serialize = "asr_esb_monsoon_en_in")]
    AsrEsbMonsoonEnIn,
    #[strum(serialize = "asr_esb_voxpopuli_cleaned_aa")]
    AsrEsbVoxpopuliCleanedAa,
    #[strum(serialize = "asr_fleurs_en_us")]
    AsrFleursEnUs,
    #[strum(serialize = "asr_fleurs_es_419")]
    AsrFleursEs419,
    #[strum(serialize = "asr_fleurs_pt_br")]
    AsrFleursPtBr,
    #[strum(serialize = "asr_fleurs_de_de")]
    AsrFleursDeDe,
    #[strum(serialize = "asr_fleurs_fr_fr")]
    AsrFleursFrFr,
    #[strum(serialize = "asr_fleurs_it_it")]
    AsrFleursItIt,
    #[strum(serialize = "asr_fleurs_nl_nl")]
    AsrFleursNlNl,
}

impl KnownAsrEvalTask {
    /// All variants, in declaration order.
    pub const ALL: &'static [Self] = <Self as VariantArray>::VARIANTS;

    /// The unit its error rate counts: words, for every dataset.
    pub fn error_unit(self) -> AsrErrorUnit {
        AsrErrorUnit::Word
    }

    /// The language its clips are in, as a BCP 47 primary language subtag
    /// (the recipe's [`super::LanguageSubtag`] spelling): English for the
    /// leaderboard's sets, the locale's language for FLEURS.
    pub fn language(self) -> &'static str {
        match self {
            Self::AsrEsbAmiCleaned
            | Self::AsrEsbEarnings22CleanedAaChunked
            | Self::AsrEsbGigaspeechCleaned
            | Self::AsrEsbLibrispeechTestClean
            | Self::AsrEsbLibrispeechTestOther
            | Self::AsrEsbSpgispeech
            | Self::AsrEsbUrgent2024
            | Self::AsrEsbMonsoonEnIn
            | Self::AsrEsbVoxpopuliCleanedAa
            | Self::AsrFleursEnUs => "en",
            Self::AsrFleursEs419 => "es",
            Self::AsrFleursPtBr => "pt",
            Self::AsrFleursDeDe => "de",
            Self::AsrFleursFrFr => "fr",
            Self::AsrFleursItIt => "it",
            Self::AsrFleursNlNl => "nl",
        }
    }
}

/// What an ASR error rate counts, which names its metric. Words only: the
/// evals server scores every dataset by word error rate (it has no
/// character-rate locale), and a scorer naming any other unit is refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Display, AsRefStr)]
#[strum(serialize_all = "snake_case")]
pub enum AsrErrorUnit {
    Word,
}

impl AsrErrorUnit {
    /// The headline metric's name.
    pub fn metric(self) -> &'static str {
        match self {
            Self::Word => "wer",
        }
    }
}

/// A task in a strict [`super::BenchmarkDefinition::AsrEval`]. Parsing never
/// fails or rewrites a task: an unknown one is kept verbatim.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(from = "String", into = "String")]
pub enum AsrEvalTask {
    Known(KnownAsrEvalTask),
    Unknown(String),
}

impl AsrEvalTask {
    /// The wire spelling.
    pub fn as_str(&self) -> &str {
        match self {
            Self::Known(known) => known.as_ref(),
            Self::Unknown(raw) => raw,
        }
    }

    /// [`KnownAsrEvalTask::error_unit`]; `None` for a task this build doesn't
    /// know, whose unit the scorer's response names instead.
    pub fn error_unit(&self) -> Option<AsrErrorUnit> {
        match self {
            Self::Known(known) => Some(known.error_unit()),
            Self::Unknown(_) => None,
        }
    }

    /// The id the evals server gives the dataset's sample at `position`:
    /// `sha256("<eval_id>|<position>")` in hex, its first 12 characters. A
    /// sample's id is its place in the dataset, so samples whose ids are not
    /// their positions' skipped or repeated one.
    pub fn sample_id(&self, position: u64) -> String {
        use sha2::Digest as _;
        let digest = sha2::Sha256::digest(format!("{}|{position}", self.as_str()));
        let mut id = hex::encode(digest);
        id.truncate(12);
        id
    }

    /// [`KnownAsrEvalTask::language`]; `None` for a task this build doesn't
    /// know.
    pub fn language(&self) -> Option<&'static str> {
        match self {
            Self::Known(known) => Some(known.language()),
            Self::Unknown(_) => None,
        }
    }

    /// The headline metric's name: the unit's (`wer`), or `error_rate` for a
    /// task this build doesn't know. Its success and its
    /// failure rows then agree, though a failure has no scorer to name a unit.
    pub fn metric(&self) -> &'static str {
        self.error_unit().map_or("error_rate", AsrErrorUnit::metric)
    }
}

impl From<String> for AsrEvalTask {
    fn from(raw: String) -> Self {
        raw.parse().map_or(Self::Unknown(raw), Self::Known)
    }
}

impl From<&str> for AsrEvalTask {
    fn from(raw: &str) -> Self {
        raw.parse()
            .map_or_else(|_| Self::Unknown(raw.to_string()), Self::Known)
    }
}

impl From<AsrEvalTask> for String {
    fn from(task: AsrEvalTask) -> Self {
        match task {
            AsrEvalTask::Known(known) => known.as_ref().to_string(),
            AsrEvalTask::Unknown(raw) => raw,
        }
    }
}

impl std::fmt::Display for AsrEvalTask {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[cfg(test)]
mod tests {
    /// The evals server's id for a position, as its builder mints it
    /// (`hashlib.sha256(b"asr_fleurs_nl_nl|0").hexdigest()[:12]`).
    #[test]
    fn a_sample_id_is_its_position() {
        let task = super::AsrEvalTask::from("asr_fleurs_nl_nl");
        assert_eq!(task.sample_id(0), SAMPLE_0);
        assert_ne!(task.sample_id(1), SAMPLE_0);
        assert_eq!(task.sample_id(0).len(), 12);
    }

    const SAMPLE_0: &str = "5c014d503305";

    /// `hashlib.sha256(b"asr_esb_urgent2024|0").hexdigest()[:12]`.
    const URGENT2024_SAMPLE_0: &str = "bdedb03cffcc";

    use rstest::rstest;

    use super::*;

    /// Must match the evals server's dataset ids exactly, or `POST /score` 404s.
    #[rstest]
    #[case(KnownAsrEvalTask::AsrEsbAmiCleaned, "asr_esb_ami_cleaned")]
    #[case(
        KnownAsrEvalTask::AsrEsbEarnings22CleanedAaChunked,
        "asr_esb_earnings22_cleaned_aa_chunked"
    )]
    #[case(
        KnownAsrEvalTask::AsrEsbGigaspeechCleaned,
        "asr_esb_gigaspeech_cleaned"
    )]
    #[case(
        KnownAsrEvalTask::AsrEsbLibrispeechTestClean,
        "asr_esb_librispeech_test_clean"
    )]
    #[case(
        KnownAsrEvalTask::AsrEsbLibrispeechTestOther,
        "asr_esb_librispeech_test_other"
    )]
    #[case(KnownAsrEvalTask::AsrEsbSpgispeech, "asr_esb_spgispeech")]
    #[case(KnownAsrEvalTask::AsrEsbUrgent2024, "asr_esb_urgent2024")]
    #[case(KnownAsrEvalTask::AsrEsbMonsoonEnIn, "asr_esb_monsoon_en_in")]
    #[case(
        KnownAsrEvalTask::AsrEsbVoxpopuliCleanedAa,
        "asr_esb_voxpopuli_cleaned_aa"
    )]
    #[case(KnownAsrEvalTask::AsrFleursEnUs, "asr_fleurs_en_us")]
    #[case(KnownAsrEvalTask::AsrFleursEs419, "asr_fleurs_es_419")]
    #[case(KnownAsrEvalTask::AsrFleursPtBr, "asr_fleurs_pt_br")]
    #[case(KnownAsrEvalTask::AsrFleursDeDe, "asr_fleurs_de_de")]
    #[case(KnownAsrEvalTask::AsrFleursFrFr, "asr_fleurs_fr_fr")]
    #[case(KnownAsrEvalTask::AsrFleursItIt, "asr_fleurs_it_it")]
    #[case(KnownAsrEvalTask::AsrFleursNlNl, "asr_fleurs_nl_nl")]
    fn a_known_task_is_spelled_as_the_scorer_spells_it(
        #[case] task: KnownAsrEvalTask,
        #[case] wire: &str,
    ) {
        assert_eq!(task.to_string(), wire);
        assert_eq!(AsrEvalTask::from(wire), AsrEvalTask::Known(task));
    }

    #[test]
    fn there_are_nine_open_asr_english_sets_and_seven_fleurs_locales() {
        let english = KnownAsrEvalTask::ALL
            .iter()
            .filter(|t| t.as_ref().starts_with("asr_esb_"))
            .count();
        let fleurs: Vec<&str> = KnownAsrEvalTask::ALL
            .iter()
            .map(AsRef::as_ref)
            .filter(|t| t.starts_with("asr_fleurs_"))
            .collect();
        assert_eq!(
            (english, fleurs.len(), KnownAsrEvalTask::ALL.len()),
            (9, 7, 16)
        );
        assert_eq!(
            fleurs,
            [
                "asr_fleurs_en_us",
                "asr_fleurs_es_419",
                "asr_fleurs_pt_br",
                "asr_fleurs_de_de",
                "asr_fleurs_fr_fr",
                "asr_fleurs_it_it",
                "asr_fleurs_nl_nl",
            ]
        );
    }

    /// The locales the evals server no longer holds are tasks this build does
    /// not know: kept verbatim, named `error_rate`, never `cer`.
    #[rstest]
    #[case("asr_fleurs_ja_jp")]
    #[case("asr_fleurs_pl_pl")]
    #[case("asr_fleurs_cs_cz")]
    #[case("asr_fleurs_da_dk")]
    #[case("asr_fleurs_el_gr")]
    #[case("asr_fleurs_fi_fi")]
    #[case("asr_fleurs_hu_hu")]
    #[case("asr_fleurs_ro_ro")]
    #[case("asr_fleurs_sv_se")]
    #[case("asr_fleurs_cmn_hans_cn")]
    #[case("asr_fleurs_ko_kr")]
    fn a_dropped_locale_is_an_unknown_task(#[case] wire: &str) {
        let task = AsrEvalTask::from(wire);
        assert_eq!(task, AsrEvalTask::Unknown(wire.to_string()));
        assert_eq!(task.error_unit(), None);
        assert_eq!(task.metric(), "error_rate");
    }

    /// The leaderboard's column order (`normalizer/eval_utils.score_results`),
    /// which the evals server declares its ids in: URGENT24 after SPGISpeech,
    /// Monsoon before VoxPopuli.
    #[test]
    fn the_english_sets_are_in_the_leaderboards_column_order() {
        let english: Vec<&str> = KnownAsrEvalTask::ALL
            .iter()
            .map(AsRef::as_ref)
            .take_while(|t| t.starts_with("asr_esb_"))
            .collect();
        assert_eq!(
            english,
            [
                "asr_esb_ami_cleaned",
                "asr_esb_earnings22_cleaned_aa_chunked",
                "asr_esb_gigaspeech_cleaned",
                "asr_esb_librispeech_test_clean",
                "asr_esb_librispeech_test_other",
                "asr_esb_spgispeech",
                "asr_esb_urgent2024",
                "asr_esb_monsoon_en_in",
                "asr_esb_voxpopuli_cleaned_aa",
            ]
        );
    }

    /// URGENT24 is known, scored by word, and its sample ids are its own (a
    /// negative control: neither a neighbour's nor an unknown task's spelling).
    #[test]
    fn urgent2024_is_a_known_word_task() {
        let task = AsrEvalTask::from("asr_esb_urgent2024");
        assert_eq!(task, AsrEvalTask::Known(KnownAsrEvalTask::AsrEsbUrgent2024));
        assert_eq!(task.error_unit(), Some(AsrErrorUnit::Word));
        assert_eq!(task.metric(), "wer");
        let unknown = AsrEvalTask::from("asr_esb_urgent2024_clean");
        assert_eq!(unknown.metric(), "error_rate");
        assert_eq!(task.sample_id(0), URGENT2024_SAMPLE_0);
        assert_ne!(unknown.sample_id(0), URGENT2024_SAMPLE_0);
        assert_ne!(
            AsrEvalTask::from("asr_esb_spgispeech").sample_id(0),
            URGENT2024_SAMPLE_0
        );
    }

    /// Every known task counts words, so its metric is `wer`.
    #[test]
    fn every_known_task_counts_words() {
        assert!(
            KnownAsrEvalTask::ALL
                .iter()
                .all(|t| t.error_unit() == AsrErrorUnit::Word
                    && AsrEvalTask::Known(*t).metric() == "wer")
        );
        assert_eq!(AsrErrorUnit::Word.metric(), "wer");
        assert_eq!(AsrErrorUnit::Word.to_string(), "word");
    }

    #[rstest]
    #[case::known(
        AsrEvalTask::Known(KnownAsrEvalTask::AsrFleursNlNl),
        "asr_fleurs_nl_nl"
    )]
    #[case::unknown(AsrEvalTask::Unknown("asr_fleurs_sw_ke".to_string()), "asr_fleurs_sw_ke")]
    fn a_task_round_trips_through_string_losslessly(
        #[case] task: AsrEvalTask,
        #[case] wire: &str,
    ) -> anyhow::Result<()> {
        assert_eq!(task.as_str(), wire);
        assert_eq!(serde_json::to_value(&task)?, wire);
        assert_eq!(serde_json::from_value::<AsrEvalTask>(wire.into())?, task);
        Ok(())
    }
}
