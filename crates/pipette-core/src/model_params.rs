//! Model parameter counts kept in mgmt's own records, one per model: the
//! model parameter table the scorer reads, edited through the API.

use chrono::{DateTime, SubsecRound, Utc};
use serde::{Deserialize, Serialize};

use pipette_plan_types::{ModelKey, NonEmptyTrimmedString, PositiveCount, UserId};

/// One model's parameter counts in millions, under its model key.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelParams {
    pub key: ModelKey,
    pub total_millions: PositiveCount,
    /// Equal to `total_millions` for a dense model.
    pub active_millions: PositiveCount,
    /// Where the counts come from, for a reader. Nothing keys on it.
    pub note: Option<NonEmptyTrimmedString>,
    pub updated_at: DateTime<Utc>,
    /// `None` on a row no account wrote: a seeded or imported one.
    pub updated_by: Option<UserId>,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ModelParamsError {
    #[error("total_millions must be > 0, got {0}")]
    Total(i32),
    #[error("active_millions must be > 0 and at most total_millions ({total}), got {active}")]
    Active { total: i32, active: i32 },
    #[error("note must be at most {max} characters, got {got}")]
    Note { max: usize, got: usize },
}

impl ModelParams {
    pub const MAX_NOTE_CHARS: usize = 500;

    /// A row an account writes. `updated_at` keeps whole microseconds, what a
    /// SQL timestamp holds, so a record round-trips equal on every backend.
    pub fn new(
        key: ModelKey,
        total_millions: PositiveCount,
        active_millions: Option<PositiveCount>,
        note: Option<NonEmptyTrimmedString>,
        updated_by: UserId,
        updated_at: DateTime<Utc>,
    ) -> Result<Self, ModelParamsError> {
        Self::checked(
            key,
            total_millions,
            active_millions,
            note,
            Some(updated_by),
            updated_at,
        )
    }

    /// A row `models import` reads from a file, under [`ModelParams::new`]'s
    /// rules, with no account and no note.
    pub fn imported(
        key: ModelKey,
        total_millions: i32,
        active_millions: i32,
        updated_at: DateTime<Utc>,
    ) -> Result<Self, ModelParamsError> {
        let total = PositiveCount::try_new(total_millions)
            .map_err(|_| ModelParamsError::Total(total_millions))?;
        let active =
            PositiveCount::try_new(active_millions).map_err(|_| ModelParamsError::Active {
                total: total_millions,
                active: active_millions,
            })?;
        Self::checked(key, total, Some(active), None, None, updated_at)
    }

    fn checked(
        key: ModelKey,
        total_millions: PositiveCount,
        active_millions: Option<PositiveCount>,
        note: Option<NonEmptyTrimmedString>,
        updated_by: Option<UserId>,
        updated_at: DateTime<Utc>,
    ) -> Result<Self, ModelParamsError> {
        let active_millions = active_millions.unwrap_or(total_millions);
        if active_millions > total_millions {
            return Err(ModelParamsError::Active {
                total: total_millions.into_inner(),
                active: active_millions.into_inner(),
            });
        }
        let chars = note.as_ref().map_or(0, |text| text.chars().count());
        if chars > Self::MAX_NOTE_CHARS {
            return Err(ModelParamsError::Note {
                max: Self::MAX_NOTE_CHARS,
                got: chars,
            });
        }
        Ok(Self {
            key,
            total_millions,
            active_millions,
            note,
            updated_at: updated_at.trunc_subsecs(6),
            updated_by,
        })
    }
}

impl From<ModelParams> for pipette_plan_types::ModelParams {
    fn from(params: ModelParams) -> Self {
        let ModelParams {
            key,
            total_millions,
            active_millions,
            note,
            updated_at,
            updated_by,
        } = params;
        Self {
            model_key: key,
            total_millions,
            active_millions,
            note,
            updated_at,
            updated_by,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::Mint;

    #[test]
    fn active_defaults_to_total() -> anyhow::Result<()> {
        let key = ModelKey::try_new("acmelm-4.5b-text")?;
        let total = PositiveCount::try_new(4512)?;
        let dense = ModelParams::new(key, total, None, None, UserId::mint(), Utc::now())?;
        assert_eq!(dense.active_millions, total);
        Ok(())
    }

    /// The stored shape, pinned in both directions: the record names its key
    /// `key`, whatever the wire calls it.
    #[test]
    fn the_stored_shape_is_pinned() -> anyhow::Result<()> {
        let params = ModelParams::new(
            ModelKey::try_new("lfm2-8b-a1b")?,
            PositiveCount::try_new(8340)?,
            Some(PositiveCount::try_new(1500)?),
            Some(NonEmptyTrimmedString::try_new("MoE")?),
            UserId::try_new("user_0195c3e2-a4f3-7b8c-9d1e-2f3a4b5c6d7e")?,
            "2026-10-11T00:00:00Z".parse()?,
        )?;
        let stored = serde_json::json!({
            "key": "lfm2-8b-a1b",
            "total_millions": 8340,
            "active_millions": 1500,
            "note": "MoE",
            "updated_at": "2026-10-11T00:00:00Z",
            "updated_by": "user_0195c3e2-a4f3-7b8c-9d1e-2f3a4b5c6d7e",
        });
        assert_eq!(serde_json::to_value(&params)?, stored);
        assert_eq!(serde_json::from_value::<ModelParams>(stored)?, params);
        Ok(())
    }

    /// A file's counts are bare integers, so an import checks that they are positive.
    #[rstest::rstest]
    #[case::zero_total(0, 0, Err(ModelParamsError::Total(0)))]
    #[case::zero_active(10, 0, Err(ModelParamsError::Active { total: 10, active: 0 }))]
    #[case::active_above_total(10, 11, Err(ModelParamsError::Active { total: 10, active: 11 }))]
    #[case::sparse(10, 4, Ok(()))]
    fn an_import_checks_its_counts(
        #[case] total: i32,
        #[case] active: i32,
        #[case] expected: Result<(), ModelParamsError>,
    ) -> anyhow::Result<()> {
        let key = ModelKey::try_new("lfm2-8b-a1b")?;
        let imported = ModelParams::imported(key, total, active, Utc::now()).map(|_| ());
        assert_eq!(imported, expected);
        Ok(())
    }

    #[rstest::rstest]
    #[case::at_the_cap(ModelParams::MAX_NOTE_CHARS, Ok(()))]
    #[case::past_the_cap(
        ModelParams::MAX_NOTE_CHARS + 1,
        Err(ModelParamsError::Note { max: 500, got: 501 })
    )]
    fn a_long_note_is_refused(
        #[case] chars: usize,
        #[case] expected: Result<(), ModelParamsError>,
    ) -> anyhow::Result<()> {
        let noted = ModelParams::new(
            ModelKey::try_new("lfm2-8b-a1b")?,
            PositiveCount::try_new(8340)?,
            Some(PositiveCount::try_new(1500)?),
            Some(NonEmptyTrimmedString::try_new("é".repeat(chars))?),
            UserId::mint(),
            Utc::now(),
        );
        assert_eq!(noted.map(|_| ()), expected);
        Ok(())
    }
}
