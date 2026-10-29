//! Envelopes shared across answers.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// A page of a cursor-paginated listing, or a listing answered whole in one page.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[cfg_attr(feature = "schemars", schemars(rename = "{T}Page"))]
pub struct ListPage<T> {
    pub items: Vec<T>,
    /// Where the next page starts; `null` on the last.
    pub next_cursor: Option<String>,
    /// Records that did not read, so a short page reads as incomplete; `0` on a
    /// listing that does not count them.
    pub unreadable: usize,
}

impl<T> ListPage<T> {
    /// A listing answered whole: one page, no cursor.
    pub fn whole(items: Vec<T>, unreadable: usize) -> Self {
        Self {
            items,
            next_cursor: None,
            unreadable,
        }
    }
}

/// A body with, when the caller asked `names=true`, a readable name for each id
/// it shows, or unasked on a page that always names them. Otherwise the body is
/// as it was.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[cfg_attr(feature = "schemars", schemars(rename = "Named{P}"))]
pub struct Named<P> {
    #[serde(flatten)]
    pub body: P,
    /// A readable name for each id the body shows, keyed by the id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub names: Option<BTreeMap<String, String>>,
}

/// A one-record write's answer: the record as stored after it, and whether the
/// write changed anything. `changed: false` is a success, so a retry is safe.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
#[cfg_attr(feature = "schemars", schemars(rename = "Changed{T}"))]
pub struct Changed<T> {
    #[serde(flatten)]
    pub record: T,
    pub changed: bool,
}
