//! The ASR suites' headlines, which the evals server leaves to its consumer:
//! it scores one dataset per request and keeps no run state. Exactly
//! pipette-scores' `scoring/asr/suite.py` (`open_asr_mean`, `fleurs_mean`):
//! the equal-weight mean of the suite's datasets' error rates, never a pooled
//! rate, and no headline at all unless every dataset is there once, with a
//! rate.
//!
//! Nothing in mgmt calls it yet. Its consumer is the follow-up that shows a
//! suite's headline: the run report, which reads a model's datasets back from
//! the warehouse rows and computes the headline on read (mgmt stores none). It
//! lands here, in the crate mgmt and clients share, ahead of that report, so
//! that the report and any client summarising its own run read one definition
//! rather than each re-deriving the leaderboard's averaging.

use crate::benchmark::{AsrErrorUnit, AsrEvalTask, KnownAsrEvalTask};

/// A suite whose datasets' error rates average into one headline.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AsrSuite {
    /// The Open ASR Leaderboard (English): its nine public test sets, word
    /// error rate, averaged as the leaderboard's `score_results` averages its
    /// public columns (huggingface/open_asr_leaderboard
    /// `normalizer/eval_utils.py`). Named for the leaderboard's ESB lineage,
    /// as the `asr_esb_` ids are, so the name stays stable.
    Esb,
    /// The seven FLEURS locales, word error rate, averaged alike.
    Fleurs,
}

/// One dataset's scored result, as `/score`'s `context` names it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AsrDatasetResult<'a> {
    pub task: &'a AsrEvalTask,
    pub error_unit: AsrErrorUnit,
    /// The corpus rate as a ratio; `None` when the scorer reported none (a
    /// dataset with no reference word). mgmt finalizes no asr_eval without a
    /// rate, so a published row always has one; `None` yields no headline.
    pub error_rate: Option<f64>,
}

/// A suite's headline.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AsrSuiteHeadline {
    /// The mean of the datasets' `error_rate` ratios, each weighted alike.
    pub error_rate: f64,
    /// [`Self::error_rate`] × 100: percent, the leaderboard's unit; unrounded.
    pub result: f64,
    /// The figure the leaderboard's console prints (its "Composite Results"
    /// line): each dataset's percent rate rounded to two decimals, averaged,
    /// the mean shown to two decimals. Its public CSV `avg` is the same mean
    /// of the rounded rates, left unrounded. Within 0.01 of [`Self::result`]:
    /// up to 0.005 from the per-dataset rounding and up to 0.005 from the
    /// final one. Rounded as Python's `round(x, 2)`: the nearest two-decimal
    /// value, ties to even.
    pub leaderboard_result: f64,
}

impl AsrSuite {
    /// The suite's datasets, in [`KnownAsrEvalTask`]'s declaration order.
    pub fn tasks(self) -> impl Iterator<Item = KnownAsrEvalTask> {
        KnownAsrEvalTask::ALL
            .iter()
            .copied()
            .filter(move |task| self.holds(*task))
    }

    fn holds(self, task: KnownAsrEvalTask) -> bool {
        let english = task.as_ref().starts_with("asr_esb_");
        match self {
            Self::Esb => english,
            Self::Fleurs => !english,
        }
    }

    /// The suite's headline: the mean of its datasets' error rates, each
    /// weighted alike, summed in the suite's order as the reference does.
    /// `None`, never a partial mean, unless every dataset of the suite is
    /// there exactly once and nothing else is, each with a rate, each
    /// counting words. (A scored run answers every sample: a partial one is
    /// never scored, so it has no result to pass here.)
    pub fn headline<'a>(
        self,
        results: impl IntoIterator<Item = AsrDatasetResult<'a>>,
    ) -> Option<AsrSuiteHeadline> {
        let wanted: Vec<KnownAsrEvalTask> = self.tasks().collect();
        let mut rates: Vec<Option<f64>> = vec![None; wanted.len()];
        for result in results {
            let AsrEvalTask::Known(task) = result.task else {
                return None;
            };
            let slot = rates.get_mut(wanted.iter().position(|t| t == task)?)?;
            if slot.is_some() || result.error_unit != task.error_unit() {
                return None;
            }
            *slot = Some(result.error_rate?);
        }
        let rates: Vec<f64> = rates.into_iter().collect::<Option<_>>()?;
        #[expect(
            clippy::cast_precision_loss,
            reason = "a suite holds at most nine datasets"
        )]
        let n = rates.len() as f64;
        let error_rate = rates.iter().sum::<f64>() / n;
        let rounded = rates
            .iter()
            .map(|rate| round_2(100.0 * rate))
            .sum::<Option<f64>>()?;
        Some(AsrSuiteHeadline {
            error_rate,
            result: error_rate * 100.0,
            leaderboard_result: round_2(rounded / n)?,
        })
    }
}

/// Python's `round(x, 2)`: `x` correctly rounded to two decimals, ties to
/// even, which Rust's fixed-precision formatting computes exactly. `None` for
/// a value that is not finite.
fn round_2(x: f64) -> Option<f64> {
    if !x.is_finite() {
        return None;
    }
    format!("{x:.2}").parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tasks(suite: AsrSuite) -> Vec<AsrEvalTask> {
        suite.tasks().map(AsrEvalTask::Known).collect()
    }

    fn results<'a>(tasks: &'a [AsrEvalTask], rates: &[f64]) -> Vec<AsrDatasetResult<'a>> {
        tasks
            .iter()
            .zip(rates)
            .map(|(task, rate)| AsrDatasetResult {
                task,
                error_unit: task.error_unit().unwrap_or(AsrErrorUnit::Word),
                error_rate: Some(*rate),
            })
            .collect()
    }

    #[test]
    fn the_suites_hold_nine_and_seven() {
        assert_eq!(AsrSuite::Esb.tasks().count(), 9);
        assert!(
            AsrSuite::Esb
                .tasks()
                .any(|t| t == KnownAsrEvalTask::AsrEsbUrgent2024)
        );
        assert_eq!(
            AsrSuite::Fleurs.tasks().collect::<Vec<_>>(),
            [
                KnownAsrEvalTask::AsrFleursEnUs,
                KnownAsrEvalTask::AsrFleursEs419,
                KnownAsrEvalTask::AsrFleursPtBr,
                KnownAsrEvalTask::AsrFleursDeDe,
                KnownAsrEvalTask::AsrFleursFrFr,
                KnownAsrEvalTask::AsrFleursItIt,
                KnownAsrEvalTask::AsrFleursNlNl,
            ]
        );
    }

    /// pipette-scores' `test_asr_suite.py` vector: locale i at (i + 1)% over
    /// unequal sizes averages to 4.0%, not its pooled rate.
    #[test]
    fn fleurs_is_the_equal_weight_mean_of_seven() -> anyhow::Result<()> {
        let fleurs = tasks(AsrSuite::Fleurs);
        let rates = [0.01, 0.02, 0.03, 0.04, 0.05, 0.06, 0.07];
        let headline = AsrSuite::Fleurs
            .headline(results(&fleurs, &rates))
            .ok_or_else(|| anyhow::anyhow!("no headline"))?;
        assert!((headline.error_rate - 0.04).abs() < 1e-12, "{headline:?}");
        assert!((headline.leaderboard_result - 4.0).abs() < f64::EPSILON);
        // Negative controls: the pooled rate over sizes 100, 1000, 10000, ...
        // is not the headline, and neither the English sets nor a locale the
        // evals server dropped belong to the suite.
        let words = [1e2, 1e3, 1e4, 1e2, 1e3, 1e4, 1e2];
        let pooled =
            rates.iter().zip(words).map(|(r, w)| r * w).sum::<f64>() / words.iter().sum::<f64>();
        assert!((pooled - headline.error_rate).abs() > 1e-3);
        let english = tasks(AsrSuite::Esb);
        let mut with_english = results(&fleurs, &rates);
        with_english.extend(results(&english, &[0.01]));
        assert_eq!(AsrSuite::Fleurs.headline(with_english), None);
        let dropped = [AsrEvalTask::from("asr_fleurs_ja_jp")];
        let mut with_dropped = results(&fleurs, &rates);
        with_dropped.extend(results(&dropped, &[0.01]));
        assert_eq!(AsrSuite::Fleurs.headline(with_dropped), None);
        Ok(())
    }

    /// pipette-scores' `test_asr_suite.py` vector: rates 1%..9% over word
    /// counts 100..100000 average to 5.0%, not their pooled rate.
    #[test]
    fn open_asr_english_is_the_equal_weight_mean_of_nine() -> anyhow::Result<()> {
        let english = tasks(AsrSuite::Esb);
        let rates = [0.01, 0.02, 0.03, 0.04, 0.05, 0.06, 0.07, 0.08, 0.09];
        let headline = AsrSuite::Esb
            .headline(results(&english, &rates))
            .ok_or_else(|| anyhow::anyhow!("no headline"))?;
        assert!((headline.error_rate - 0.05).abs() < 1e-12, "{headline:?}");
        assert!((headline.result - 5.0).abs() < 1e-10, "{headline:?}");
        assert!((headline.leaderboard_result - 5.0).abs() < f64::EPSILON);
        // Negative control: the pooled rate over the vector's sizes
        // (100, 1000, 10000, 100000, ...) is not the headline.
        let words = [1e2, 1e3, 1e4, 1e5, 1e2, 1e3, 1e4, 1e5, 1e2];
        let pooled =
            rates.iter().zip(words).map(|(r, w)| r * w).sum::<f64>() / words.iter().sum::<f64>();
        assert!((pooled - headline.error_rate).abs() > 1e-3);
        // The eight sets of the earlier English suite (URGENT24 left out) are
        // no headline any more, though their 4.5% mean was the old one.
        let eight: Vec<AsrDatasetResult<'_>> = results(&english, &rates)
            .into_iter()
            .filter(|r| r.task.as_str() != "asr_esb_urgent2024")
            .collect();
        assert_eq!(eight.len(), 8);
        assert_eq!(AsrSuite::Esb.headline(eight), None);
        Ok(())
    }

    /// `test_asr_suite.py`'s rounding vector: one set at 1/3, eight at 1%:
    /// the unrounded percent is (100/3 + 8)/9 = 4.5926..., the leaderboard's
    /// (33.33 + 8)/9 rounded, 4.59.
    #[test]
    fn the_leaderboard_figure_rounds_each_set_then_the_mean() -> anyhow::Result<()> {
        let english = tasks(AsrSuite::Esb);
        let mut rates = [0.01; 9];
        rates[0] = 1.0 / 3.0;
        let headline = AsrSuite::Esb
            .headline(results(&english, &rates))
            .ok_or_else(|| anyhow::anyhow!("no headline"))?;
        assert!((headline.result - (100.0 / 3.0 + 8.0) / 9.0).abs() < 1e-12);
        assert!((headline.leaderboard_result - 4.59).abs() < f64::EPSILON);
        // Negative control: the figure rounds each set before the mean, so it
        // is not the unrounded mean rounded. Eight sets at 0.0371% and one at
        // 0: each rounds to 0.04, so the leaderboard prints 0.04, where the
        // unrounded mean, 0.03298%, would round to 0.03.
        let mut rates = [0.000_371; 9];
        rates[8] = 0.0;
        let headline = AsrSuite::Esb
            .headline(results(&english, &rates))
            .ok_or_else(|| anyhow::anyhow!("no headline"))?;
        assert!((headline.leaderboard_result - 0.04).abs() < f64::EPSILON);
        assert_eq!(round_2(headline.result), Some(0.03));
        Ok(())
    }

    /// `test_asr_suite.py`'s bound vector: both roundings can push the same
    /// way, so the console figure is within 0.01 of the unrounded mean, not
    /// 0.005. Eight sets at 1.0049% round to 1.00 and one at 1.0449% to 1.04;
    /// the figure is round(9.04 / 9, 2) = 1.00 for an unrounded 1.00934%.
    #[test]
    fn the_leaderboard_figure_is_within_0_01_of_the_mean() -> anyhow::Result<()> {
        let english = tasks(AsrSuite::Esb);
        let mut rates = [0.010_049; 9];
        rates[8] = 0.010_449;
        let headline = AsrSuite::Esb
            .headline(results(&english, &rates))
            .ok_or_else(|| anyhow::anyhow!("no headline"))?;
        assert!((headline.result - (8.0 * 1.0049 + 1.0449) / 9.0).abs() < 1e-10);
        assert!((headline.leaderboard_result - 1.0).abs() < f64::EPSILON);
        let off = headline.result - headline.leaderboard_result;
        assert!(off > 0.005 && off <= 0.01, "{off}");
        Ok(())
    }

    /// Python's `round(x, 2)` rounds the exact binary value, ties to even:
    /// `round(0.125, 2) == 0.12`, `round(0.375, 2) == 0.38`,
    /// `round(2.675, 2) == 2.67` (2.675 is stored just below).
    #[test]
    fn round_2_is_pythons_round() {
        assert_eq!(round_2(0.125), Some(0.12));
        assert_eq!(round_2(0.375), Some(0.38));
        assert_eq!(round_2(2.675), Some(2.67));
        assert_eq!(round_2(33.333_333_333_333_336), Some(33.33));
        assert_eq!(round_2(f64::NAN), None);
        assert_eq!(round_2(f64::INFINITY), None);
    }

    #[test]
    fn a_suite_short_of_a_dataset_has_no_headline() {
        let english = tasks(AsrSuite::Esb);
        let mut eight = results(&english, &[0.01; 9]);
        eight.pop();
        assert_eq!(AsrSuite::Esb.headline(eight), None);
    }

    #[test]
    fn a_repeated_or_foreign_dataset_has_no_headline() {
        let english = tasks(AsrSuite::Esb);
        let mut repeated = results(&english, &[0.01; 9]);
        repeated[8] = repeated[0];
        assert_eq!(AsrSuite::Esb.headline(repeated), None);
        let fleurs = tasks(AsrSuite::Fleurs);
        let mut foreign = results(&english, &[0.01; 9]);
        foreign.extend(results(&fleurs, &[0.01]));
        assert_eq!(AsrSuite::Esb.headline(foreign), None);
        let unknown = [AsrEvalTask::from("asr_esb_urgent2024_clean")];
        let mut with_unknown = results(&english, &[0.01; 9]);
        with_unknown.extend(results(&unknown, &[0.01]));
        assert_eq!(AsrSuite::Esb.headline(with_unknown), None);
    }

    #[test]
    fn a_rateless_dataset_has_no_headline() {
        let fleurs = tasks(AsrSuite::Fleurs);
        let rates = [0.1, 0.2, 0.3, 0.4, 0.5, 0.6, 0.7];
        let mut rateless = results(&fleurs, &rates);
        rateless[2].error_rate = None;
        assert_eq!(AsrSuite::Fleurs.headline(rateless), None);
        let fine = AsrSuite::Fleurs.headline(results(&fleurs, &rates));
        assert!(fine.is_some_and(|h| (h.error_rate - 0.4).abs() < 1e-12));
    }

    /// The headline is the same whatever order the results come in: it sums
    /// in the suite's order.
    #[test]
    fn the_input_order_does_not_change_the_headline() {
        let english = tasks(AsrSuite::Esb);
        let rates = [0.1, 0.2, 0.3, 0.4, 0.5, 0.6, 0.7, 0.8, 0.9];
        let forward = AsrSuite::Esb.headline(results(&english, &rates));
        let mut reversed = results(&english, &rates);
        reversed.reverse();
        assert!(forward.is_some());
        assert_eq!(AsrSuite::Esb.headline(reversed), forward);
    }
}
