//! The record a `denied/{job_id}.{client_id}` marker holds, and the summary the
//! all-denied escalation writes from those records, so its synthetic failure
//! says why the clients failed.

use std::collections::BTreeMap;
use std::sync::LazyLock;

use chrono::{DateTime, Utc};
use regex::{Captures, Regex};
use serde::{Deserialize, Serialize};

use pipette_plan_types::{ClientId, ELLIPSIS, keep_tail};

/// The first words of every all-denied failure message.
const ALL_DENIED_HEADLINE: &str = "All eligible clients reported failure";

/// The maximum stored `failure_reason`, in bytes; [`keep_tail`] keeps the end,
/// where a runtime usually writes its final error.
const MAX_STORED_REASON_BYTES: usize = 4096;

/// The maximum length, in bytes, of the `client_version` or `runtime_version`
/// that a marker stores. A real version string is much shorter, so the cap
/// cuts only abusive input.
const MAX_STORED_VERSION_BYTES: usize = 256;

/// The maximum length, in bytes, of one reason in the summary.
const MAX_SHOWN_REASON_BYTES: usize = 500;

/// The maximum number of reason groups that the summary lists.
const MAX_SHOWN_GROUPS: usize = 5;

/// The maximum number of client ids that one group lists.
const MAX_SHOWN_CLIENTS: usize = 3;

/// The text that the summary shows for a denial with no stored reason.
const NO_REASON: &str = "no reason recorded";

/// The text that the summary shows for a denial whose marker the escalation
/// cannot read.
const UNREADABLE_REASON: &str = "reason unreadable";

/// The retriable failure a client reported when it denied a job. Every field is
/// optional: an empty marker body reads as the default record.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DenialRecord {
    /// When the server accepted the failure report.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub denied_at: Option<DateTime<Utc>>,
    /// The client's `failure_reason`, cut to [`MAX_STORED_REASON_BYTES`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure_reason: Option<String>,
    /// The client's `client_version`, cut to [`MAX_STORED_VERSION_BYTES`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_version: Option<String>,
    /// The client's `runtime_version`, cut to [`MAX_STORED_VERSION_BYTES`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime_version: Option<String>,
}

impl DenialRecord {
    pub fn new(
        denied_at: DateTime<Utc>,
        failure_reason: &str,
        client_version: Option<&str>,
        runtime_version: Option<&str>,
    ) -> Self {
        Self {
            denied_at: Some(denied_at),
            failure_reason: Some(keep_tail(failure_reason, MAX_STORED_REASON_BYTES)),
            client_version: client_version.map(|v| keep_tail(v, MAX_STORED_VERSION_BYTES)),
            runtime_version: runtime_version.map(|v| keep_tail(v, MAX_STORED_VERSION_BYTES)),
        }
    }

    /// The marker body for this record.
    pub fn to_bytes(&self) -> Result<Vec<u8>, serde_json::Error> {
        serde_json::to_vec(self)
    }

    /// Read a marker body. An empty body is the default record.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, serde_json::Error> {
        if bytes.trim_ascii().is_empty() {
            return Ok(Self::default());
        }
        serde_json::from_slice(bytes)
    }
}

/// One roster member's part in the all-denied summary.
#[derive(Debug, Clone)]
pub struct DeniedClient {
    pub client_id: ClientId,
    /// The member's stored denial, or `None` if the escalation cannot read the
    /// marker. An empty marker holds the default record.
    pub record: Option<DenialRecord>,
}

/// The failure message for a job that every roster member denied.
///
/// Members are grouped by [`group_key`], so reasons differing only in
/// timestamps, hex addresses or long numbers are one group. Each group shows
/// its size, some client ids and its latest reason; the largest comes first,
/// at most [`MAX_SHOWN_GROUPS`] with a reason, and the two reasonless groups
/// always show.
///
/// Example:
///
/// ```text
/// All eligible clients reported failure (10 clients, 2 distinct reasons):
/// - 8 clients (ev1_a, ev1_b, ev1_c, +5 more): llama-server failed to load model: out of memory
/// - 2 clients (ev1_x, ev1_y): llama-server crashed mid-completion: exit signal: 11 (SIGSEGV)
/// ```
pub fn summarize_denials(denials: &[DeniedClient]) -> String {
    let mut sorted: Vec<&DeniedClient> = denials.iter().collect();
    sorted.sort_by(|a, b| a.client_id.cmp(&b.client_id));

    let groups: BTreeMap<GroupKey, ReasonGroup> =
        sorted.into_iter().fold(BTreeMap::new(), |mut acc, denial| {
            let (key, at, shown) = match &denial.record {
                None => (GroupKey::Unreadable, None, None),
                Some(record) => {
                    let reason = record.failure_reason.as_deref().unwrap_or_default();
                    let shown = display_reason(reason);
                    if shown.is_empty() {
                        (GroupKey::NoReason, record.denied_at, None)
                    } else {
                        let key = GroupKey::Reason(group_key(reason, &shown));
                        (key, record.denied_at, Some(shown))
                    }
                }
            };
            acc.entry(key)
                .or_default()
                .add(&denial.client_id, at, shown);
            acc
        });

    let mut groups: Vec<(GroupKey, ReasonGroup)> = groups.into_iter().collect();
    groups.sort_by(|(ka, a), (kb, b)| {
        b.clients
            .len()
            .cmp(&a.clients.len())
            .then_with(|| ka.cmp(kb))
    });

    let reasons = groups.iter().filter(|(key, _)| key.is_reason()).count();
    let mut out = format!(
        "{ALL_DENIED_HEADLINE} ({}, {}):",
        count(denials.len(), "client", "clients"),
        count(reasons, "distinct reason", "distinct reasons"),
    );
    let mut reasons_seen = 0;
    let (shown, hidden): (Vec<_>, Vec<_>) = groups.iter().partition(|(key, _)| {
        if !key.is_reason() {
            return true;
        }
        reasons_seen += 1;
        reasons_seen <= MAX_SHOWN_GROUPS
    });
    for (key, group) in shown {
        out.push_str("\n- ");
        out.push_str(&group.line(key));
    }
    if !hidden.is_empty() {
        let hidden_clients: usize = hidden.iter().map(|(_, g)| g.clients.len()).sum();
        out.push_str(&format!(
            "\n- +{} ({})",
            count(
                hidden.len(),
                "more distinct reason",
                "more distinct reasons"
            ),
            count(hidden_clients, "client", "clients"),
        ));
    }
    out
}

/// What the roster members in one group have in common. The order puts the
/// groups that hold a reason first.
#[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
enum GroupKey {
    /// The members gave reasons with this [`group_key`].
    Reason(String),
    /// The escalation cannot read the members' markers.
    Unreadable,
    /// The members' markers hold no reason.
    NoReason,
}

impl GroupKey {
    fn is_reason(&self) -> bool {
        matches!(self, Self::Reason(_))
    }
}

/// The roster members that gave one reason.
#[derive(Default)]
struct ReasonGroup {
    clients: Vec<String>,
    /// The shown text of the most recent reason in the group, and its time.
    /// `None` for a group that holds no reason.
    latest: Option<(Option<DateTime<Utc>>, String)>,
}

impl ReasonGroup {
    fn add(&mut self, client_id: &ClientId, at: Option<DateTime<Utc>>, shown: Option<String>) {
        self.clients.push(client_id.to_string());
        if let Some(shown) = shown
            && self.latest.as_ref().is_none_or(|(latest, _)| at > *latest)
        {
            self.latest = Some((at, shown));
        }
    }

    fn line(&self, key: &GroupKey) -> String {
        let who = listed(self.clients.iter().map(String::as_str), MAX_SHOWN_CLIENTS);
        let reason = match key {
            GroupKey::Reason(_) => self.latest.as_ref().map_or_else(String::new, |(_, shown)| {
                keep_tail(shown, MAX_SHOWN_REASON_BYTES)
            }),
            GroupKey::Unreadable => UNREADABLE_REASON.to_owned(),
            GroupKey::NoReason => NO_REASON.to_owned(),
        };
        format!(
            "{} ({who}): {reason}",
            count(self.clients.len(), "client", "clients")
        )
    }
}

/// Join the first `max` items with `", "`, and name how many more there are.
fn listed<'a>(items: impl ExactSizeIterator<Item = &'a str>, max: usize) -> String {
    let total = items.len();
    let mut out = items.take(max).collect::<Vec<_>>().join(", ");
    if total > max {
        out.push_str(&format!(", +{} more", total - max));
    }
    out
}

fn count(n: usize, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

/// A reason as the summary shows it: timestamps removed, the brackets that
/// held only a timestamp removed, and whitespace collapsed to single spaces.
///
/// A timestamp at the start of a log line often has a separator after it, as
/// in `12:04:51: OOM`. If the reason starts with a timestamp, the separators
/// (`:`, `-`, `|`, `,`) that follow it are removed too.
fn display_reason(reason: &str) -> String {
    let (stripped, leading) = strip_timestamps(reason);
    let collapsed = remove_empty_brackets(&stripped)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    if leading {
        collapsed
            .trim_start_matches(|c: char| c.is_whitespace() || ":-|,".contains(c))
            .to_owned()
    } else {
        collapsed
    }
}

/// The grouping key for a stored reason, and `shown`, its [`display_reason`].
///
/// The key is the [`reason_key`] of the shown text, less the fragment that a
/// cut leaves at the start (see [`drop_cut_fragment`]). If nothing is left, the
/// key is that of the whole shown text.
fn group_key(reason: &str, shown: &str) -> String {
    let whole = display_reason(drop_cut_fragment(reason));
    reason_key(if whole.is_empty() { shown } else { &whole })
}

/// The part of a cut reason after its first line, or after its first word if
/// it holds one line; a reason not starting with [`ELLIPSIS`] is returned
/// whole. Two copies of one log cut at a fixed byte count can differ at the
/// cut, but agree after its first line break.
fn drop_cut_fragment(reason: &str) -> &str {
    let Some(rest) = reason.strip_prefix(ELLIPSIS) else {
        return reason;
    };
    rest.split_once('\n')
        .or_else(|| rest.split_once(char::is_whitespace))
        .map_or(rest, |(_, after)| after)
}

/// A hex address, or a run of digits. A digit run is one token, so a hex
/// address starts only where a run starts.
static DEVICE_VALUE: LazyLock<Option<Regex>> =
    LazyLock::new(|| literal_regex(r"(?<hex>0[xX][0-9a-fA-F]+)|[0-9]+"));

/// An ISO 8601 date, date and time, or time of day with seconds, and the digits
/// right before and right after it.
static TIMESTAMP: LazyLock<Option<Regex>> = LazyLock::new(|| {
    literal_regex(
        r"(?x)
        (?<before>[0-9]*)
        (?:
            [0-9]{4}-[0-9]{2}-[0-9]{2}
            (?: [Tt\ ] [0-9]{2}:[0-9]{2} (?::[0-9]{2})?
                (?:[.,][0-9]+)? (?:[Zz]|[+-](?:[0-9]{2}:[0-9]{2}|[0-9]{4}))? )?
          | [0-9]{2}:[0-9]{2}:[0-9]{2}
            (?:[.,][0-9]+)? (?:[Zz]|[+-](?:[0-9]{2}:[0-9]{2}|[0-9]{4}))?
        )
        (?<after>[0-9]*)",
    )
});

/// A `[...]` or `(...)` that holds only whitespace.
static EMPTY_BRACKETS: LazyLock<Option<Regex>> =
    LazyLock::new(|| literal_regex(r"\[\s*\]|\(\s*\)"));

/// The grouping key for a shown reason: hex addresses become `0x#`, and each
/// run of four or more digits becomes `#`.
///
/// These values change from device to device for the same fault: addresses,
/// process ids, and byte counts. Shorter numbers stay, so that `exit signal:
/// 11` and `exit signal: 6` are different groups.
fn reason_key(shown: &str) -> String {
    let Some(device_value) = DEVICE_VALUE.as_ref() else {
        return shown.to_owned();
    };
    device_value
        .replace_all(shown, |caps: &Captures| {
            let token = caps.get(0).map_or("", |m| m.as_str());
            if caps.name("hex").is_some() {
                "0x#"
            } else if token.len() >= 4 {
                "#"
            } else {
                token
            }
            .to_owned()
        })
        .into_owned()
}

/// Remove every ISO 8601 date, date and time, or time of day with seconds.
/// Also tell if a removed timestamp starts the text, after any whitespace and
/// opening brackets.
///
/// A date takes an optional time. A time takes an optional fraction and an
/// optional zone (`Z`, `+02:00`, `-0700`). A timestamp with a digit right
/// before or right after it is part of a longer number, and stays.
fn strip_timestamps(s: &str) -> (String, bool) {
    let Some(timestamp) = TIMESTAMP.as_ref() else {
        return (s.to_owned(), false);
    };
    let mut leading = false;
    let stripped = timestamp
        .replace_all(s, |caps: &Captures| {
            let in_number = ["before", "after"]
                .iter()
                .any(|name| caps.name(name).is_some_and(|m| !m.is_empty()));
            let whole = caps.get(0);
            if in_number {
                return whole.map_or("", |m| m.as_str()).to_owned();
            }
            let before = whole.and_then(|m| s.get(..m.start())).unwrap_or_default();
            leading |= before
                .chars()
                .all(|c| c.is_whitespace() || "[(".contains(c));
            String::new()
        })
        .into_owned();
    (stripped, leading)
}

/// Remove each `[...]` or `(...)` that holds only whitespace.
fn remove_empty_brackets(s: &str) -> String {
    EMPTY_BRACKETS
        .as_ref()
        .map_or_else(|| s.to_owned(), |re| re.replace_all(s, "").into_owned())
}

/// Compile a pattern that this module writes as a literal.
///
/// A pattern that does not compile logs an error and gives `None`. The step
/// that uses the pattern then leaves the text as it is, so the summary loses
/// only some grouping. The unit tests compile every pattern.
fn literal_regex(pattern: &str) -> Option<Regex> {
    Regex::new(pattern)
        .inspect_err(|e| {
            tracing::error!(
                pattern,
                error = %e,
                "denial summary: literal pattern does not compile, step skipped"
            );
        })
        .ok()
}

#[cfg(test)]
mod tests {
    use rstest::rstest;

    use super::*;

    fn denial(client_id: &str, reason: Option<&str>, minute: u32) -> anyhow::Result<DeniedClient> {
        let at = format!("2026-09-21T14:{minute:02}:00Z").parse().ok();
        Ok(DeniedClient {
            client_id: ClientId::try_new(client_id)?,
            record: Some(DenialRecord {
                denied_at: at,
                failure_reason: reason.map(str::to_owned),
                ..DenialRecord::default()
            }),
        })
    }

    #[rstest]
    #[case::leading_bracketed(
        "[2026-03-10T12:04:51Z] llama-server failed to load model: out of memory",
        "llama-server failed to load model: out of memory"
    )]
    #[case::fraction_and_offset("2026-03-10 12:04:51.123+02:00: runtime OOM", "runtime OOM")]
    #[case::bare_date("(2026-03-10) crash", "crash")]
    #[case::time_of_day("at 12:04:51 the runtime crashed", "at the runtime crashed")]
    #[case::keeps_short_times("layer 12:34 failed", "layer 12:34 failed")]
    #[case::keeps_signal("exit signal: 11 (SIGSEGV)", "exit signal: 11 (SIGSEGV)")]
    #[case::collapses_whitespace("  OOM\n  at decode\tstep ", "OOM at decode step")]
    #[case::only_a_timestamp("[2026-03-10T12:04:51Z]", "")]
    #[case::keeps_a_leading_flag(
        "--ctx-size 8192 exceeds model maximum",
        "--ctx-size 8192 exceeds model maximum"
    )]
    #[case::keeps_a_leading_dash("-ENOMEM from mmap", "-ENOMEM from mmap")]
    #[case::bracketed_then_separator("(12:04:51) | OOM", "OOM")]
    fn display_reason_removes_timestamps(#[case] reason: &str, #[case] expected: &str) {
        assert_eq!(display_reason(reason), expected);
    }

    #[test]
    fn every_literal_pattern_compiles() {
        for (name, pattern) in [
            ("DEVICE_VALUE", &DEVICE_VALUE),
            ("TIMESTAMP", &TIMESTAMP),
            ("EMPTY_BRACKETS", &EMPTY_BRACKETS),
        ] {
            assert!(pattern.is_some(), "{name} does not compile");
        }
    }

    #[rstest]
    #[case::hex("fault at 0x7ffe1234abcd", "fault at 0x#")]
    #[case::long_digits("failed to allocate 3221225472 bytes", "failed to allocate # bytes")]
    #[case::short_digits("exit signal: 11", "exit signal: 11")]
    #[case::version_label("b5000 build", "b# build")]
    fn reason_key_masks_device_specific_values(#[case] shown: &str, #[case] expected: &str) {
        assert_eq!(reason_key(shown), expected);
    }

    /// Each case is a list of `(client_id, reason, minute)` and the summary it
    /// gives. A near-identical reason joins the group of the reason it matches.
    /// The group shows its most recent reason.
    #[rstest]
    #[case::exactly_identical(
        &[
            ("ev1_a", Some("llama-server failed to load model: out of memory"), 0),
            ("ev1_b", Some("llama-server failed to load model: out of memory"), 1),
        ],
        "All eligible clients reported failure (2 clients, 1 distinct reason):\n\
         - 2 clients (ev1_a, ev1_b): llama-server failed to load model: out of memory"
    )]
    #[case::differ_in_timestamps(
        &[
            ("ev1_0", Some("[2026-09-21T14:00:11Z] llama-server failed to load model: out of memory"), 0),
            ("ev1_1", Some("[2026-09-21T14:01:11Z] llama-server failed to load model: out of memory"), 1),
            ("ev1_2", Some("[2026-09-21T14:02:11Z] llama-server failed to load model: out of memory"), 2),
        ],
        "All eligible clients reported failure (3 clients, 1 distinct reason):\n\
         - 3 clients (ev1_0, ev1_1, ev1_2): llama-server failed to load model: out of memory"
    )]
    #[case::differ_in_whitespace(
        &[
            ("ev1_a", Some("OOM at decode step"), 0),
            ("ev1_b", Some("OOM\n  at decode\tstep "), 1),
        ],
        "All eligible clients reported failure (2 clients, 1 distinct reason):\n\
         - 2 clients (ev1_a, ev1_b): OOM at decode step"
    )]
    #[case::differ_in_long_numbers(
        &[
            ("ev1_a", Some("failed to allocate 3221225472 bytes (pid 48213)"), 2),
            ("ev1_b", Some("failed to allocate 1073741824 bytes (pid 9120)"), 4),
            ("ev1_c", Some("failed to allocate 2147483648 bytes (pid 30007)"), 3),
        ],
        "All eligible clients reported failure (3 clients, 1 distinct reason):\n\
         - 3 clients (ev1_a, ev1_b, ev1_c): failed to allocate 1073741824 bytes (pid 9120)"
    )]
    #[case::differ_in_hex_addresses(
        &[
            ("ev1_a", Some("segfault at 0x7ffe1234abcd in libggml.so"), 5),
            ("ev1_b", Some("segfault at 0x7ffe9876fedc in libggml.so"), 1),
        ],
        "All eligible clients reported failure (2 clients, 1 distinct reason):\n\
         - 2 clients (ev1_a, ev1_b): segfault at 0x7ffe1234abcd in libggml.so"
    )]
    #[case::short_numbers_stay_apart(
        &[
            ("ev1_a", Some("exit signal: 11 (SIGSEGV)"), 0),
            ("ev1_b", Some("exit signal: 6 (SIGABRT)"), 1),
        ],
        "All eligible clients reported failure (2 clients, 2 distinct reasons):\n\
         - 1 client (ev1_a): exit signal: 11 (SIGSEGV)\n\
         - 1 client (ev1_b): exit signal: 6 (SIGABRT)"
    )]
    #[case::separate_groups_largest_first(
        &[
            ("ev1_x", Some("exit signal: 11 (SIGSEGV)"), 1),
            ("ev1_a", Some("failed to allocate 3221225472 bytes"), 2),
            ("ev1_b", Some("failed to allocate 2147483648 bytes"), 3),
            ("ev1_c", Some("failed to allocate 1073741824 bytes"), 4),
            ("ev1_d", Some("failed to allocate 1073741824 bytes"), 0),
            ("ev1_y", Some("exit signal: 6 (SIGABRT)"), 5),
            ("ev1_z", None, 6),
        ],
        "All eligible clients reported failure (7 clients, 3 distinct reasons):\n\
         - 4 clients (ev1_a, ev1_b, ev1_c, +1 more): failed to allocate 1073741824 bytes\n\
         - 1 client (ev1_x): exit signal: 11 (SIGSEGV)\n\
         - 1 client (ev1_y): exit signal: 6 (SIGABRT)\n\
         - 1 client (ev1_z): no reason recorded"
    )]
    fn summarize_denials_groups_reasons(
        #[case] members: &[(&str, Option<&str>, u32)],
        #[case] expected: &str,
    ) -> anyhow::Result<()> {
        let denials = members
            .iter()
            .map(|&(client_id, reason, minute)| denial(client_id, reason, minute))
            .collect::<anyhow::Result<Vec<_>>>()?;
        assert_eq!(summarize_denials(&denials), expected);
        Ok(())
    }

    #[test]
    fn groups_past_the_limit_are_counted() -> anyhow::Result<()> {
        let denials = (0..7)
            .map(|i| denial(&format!("ev1_{i}"), Some(&format!("fault {i}")), 0))
            .collect::<anyhow::Result<Vec<_>>>()?;
        let summary = summarize_denials(&denials);
        assert!(
            summary.starts_with(
                "All eligible clients reported failure (7 clients, 7 distinct reasons):"
            )
        );
        assert_eq!(summary.lines().count(), 1 + MAX_SHOWN_GROUPS + 1);
        assert!(
            summary.ends_with("\n- +2 more distinct reasons (2 clients)"),
            "{summary}"
        );
        Ok(())
    }

    /// The overflow line counts only the groups that hold a reason. The groups
    /// with no reason sort last among groups of one size, but always show.
    #[test]
    fn groups_without_a_reason_are_never_counted_as_hidden() -> anyhow::Result<()> {
        let mut denials = (0..6)
            .map(|i| denial(&format!("ev1_{i}"), Some(&format!("fault {i}")), 0))
            .collect::<anyhow::Result<Vec<_>>>()?;
        denials.push(denial("ev1_n", None, 0)?);
        denials.push(DeniedClient {
            client_id: ClientId::try_new("ev1_u")?,
            record: None,
        });
        assert_eq!(
            summarize_denials(&denials),
            "All eligible clients reported failure (8 clients, 6 distinct reasons):\n\
             - 1 client (ev1_0): fault 0\n\
             - 1 client (ev1_1): fault 1\n\
             - 1 client (ev1_2): fault 2\n\
             - 1 client (ev1_3): fault 3\n\
             - 1 client (ev1_4): fault 4\n\
             - 1 client (ev1_u): reason unreadable\n\
             - 1 client (ev1_n): no reason recorded\n\
             - +1 more distinct reason (1 client)"
        );
        Ok(())
    }

    /// Two copies of one long log, stored through [`DenialRecord::new`], are
    /// one group. The copies differ in the length of one value near the end, so
    /// the cut falls at different characters.
    #[rstest]
    #[case::lines(
        (0..300).map(|i| format!("load tensor {i:04} ok\n")).collect::<String>(),
        "llama-server pid {pid} failed: out of memory\nexiting"
    )]
    #[case::one_line_cut_in_a_number(
        format!("{}0", "x".repeat(2000)),
        "{pid}0 {}",
    )]
    fn copies_of_a_cut_reason_are_one_group(
        #[case] head: String,
        #[case] tail: &str,
    ) -> anyhow::Result<()> {
        let reason = |pid: &str| {
            let tail = tail.replace("{pid}", pid);
            // A one-line tail pads to leave two digits of the number in the cut.
            let tail = tail.replace(
                "{}",
                &"y ".repeat((MAX_STORED_REASON_BYTES - ELLIPSIS.len() - 3) / 2),
            );
            format!("{head}{tail}")
        };
        let denials = ["48213", "9120"]
            .iter()
            .zip(["ev1_a", "ev1_b"])
            .map(|(pid, client_id)| {
                Ok(DeniedClient {
                    client_id: ClientId::try_new(client_id)?,
                    record: Some(DenialRecord::new(Utc::now(), &reason(pid), None, None)),
                })
            })
            .collect::<anyhow::Result<Vec<_>>>()?;
        let summary = summarize_denials(&denials);
        assert!(
            summary.starts_with(
                "All eligible clients reported failure (2 clients, 1 distinct reason):"
            ),
            "{summary}"
        );
        Ok(())
    }

    #[test]
    fn a_long_reason_is_cut_in_the_summary() -> anyhow::Result<()> {
        let reason = format!("{} final error", "x".repeat(2000));
        let summary = summarize_denials(&[denial("ev1_a", Some(&reason), 0)?]);
        let shown = summary
            .split_once("(ev1_a): ")
            .map(|(_, shown)| shown)
            .ok_or_else(|| anyhow::anyhow!("no group line in {summary:?}"))?;
        assert!(
            shown.len() <= MAX_SHOWN_REASON_BYTES,
            "{} bytes",
            shown.len()
        );
        assert!(
            shown.starts_with('…') && shown.ends_with(" final error"),
            "{shown:?}"
        );
        Ok(())
    }

    #[rstest]
    #[case::empty(b"".as_slice(), DenialRecord::default())]
    #[case::whitespace(b" \n".as_slice(), DenialRecord::default())]
    #[case::reason(
        br#"{"failure_reason":"OOM"}"#.as_slice(),
        DenialRecord { failure_reason: Some("OOM".to_owned()), ..DenialRecord::default() }
    )]
    fn from_bytes_reads_empty_and_json_markers(
        #[case] bytes: &[u8],
        #[case] expected: DenialRecord,
    ) -> anyhow::Result<()> {
        assert_eq!(DenialRecord::from_bytes(bytes)?, expected);
        Ok(())
    }

    #[test]
    fn new_caps_the_stored_fields() -> anyhow::Result<()> {
        let long = "x".repeat(MAX_STORED_REASON_BYTES * 2);
        let record = DenialRecord::new(Utc::now(), &long, Some("1.4.2"), Some(&long));
        let back = DenialRecord::from_bytes(&record.to_bytes()?)?;
        assert!(
            back.failure_reason
                .is_some_and(|r| r.len() <= MAX_STORED_REASON_BYTES)
        );
        assert_eq!(back.client_version.as_deref(), Some("1.4.2"));
        assert!(
            back.runtime_version
                .is_some_and(|v| v.len() <= MAX_STORED_VERSION_BYTES)
        );
        Ok(())
    }
}
