//! User accounts: the operator principal, as [`crate::client::Client`] is the
//! device's. A user holds many keys under a minted id, has no device profile (so
//! is never handed a job), and is always admitted by an operator.

use std::collections::BTreeSet;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use pipette_plan_types::{
    CampaignId, ContactEmail, ContactEmailError, NonEmptyTrimmedString, PublicKeyHex,
    UserAccountType, UserId, UserKeyId, UserStatus,
};

use crate::types::{DeriveFromKey, Mint};

/// A user account — the body of `users/{user_id}.json`. Keys, permissions and
/// memberships are separate records.
///
/// **Five required fields, deliberately**: a field that is only probably useful
/// is cheaper to add later than to carry (`docs/authentication.md` lists those
/// left out).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct User {
    pub user_id: UserId,
    /// The account's human handle and where a pre-auth key is sent. Stored
    /// **case-folded** because it is also the key of its `user-email-index/`
    /// entry, which keeps it unique across accounts: one mailbox, one key. Never
    /// verified: nothing here sends mail.
    pub contact_email: ContactEmail,
    /// Defaulted for a record written before the field, which a person held.
    #[serde(default)]
    pub account_type: UserAccountType,
    pub status: UserStatus,
    pub created_at: DateTime<Utc>,
    /// The campaigns a service account reads, and the only ones: set, the
    /// account holds `read_plans` alone ([`crate::user_permission::Holder`]).
    /// Replaced, never removed, so a reader is never widened into a member.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub campaign_scope: Option<BTreeSet<CampaignId>>,
}

impl From<User> for pipette_plan_types::User {
    fn from(user: User) -> Self {
        Self {
            user_id: user.user_id,
            contact_email: user.contact_email,
            account_type: user.account_type,
            status: user.status,
            created_at: user.created_at,
            campaign_scope: user.campaign_scope,
        }
    }
}

impl User {
    /// A new active account under a freshly minted id. Folds the address;
    /// fallible because a case mapping can change its length, so the result is
    /// re-checked.
    pub fn try_new(
        contact_email: ContactEmail,
        created_at: DateTime<Utc>,
    ) -> Result<Self, ContactEmailError> {
        Ok(Self {
            user_id: UserId::mint(),
            contact_email: ContactEmail::try_new(contact_email.as_str().to_lowercase())?,
            account_type: UserAccountType::Human,
            status: UserStatus::Active,
            created_at,
            campaign_scope: None,
        })
    }

    /// An account that asked for itself — [`UserStatus::Pending`] until an
    /// operator admits it (`users approve`). Otherwise as [`Self::try_new`].
    pub fn requested(
        contact_email: ContactEmail,
        created_at: DateTime<Utc>,
    ) -> Result<Self, ContactEmailError> {
        Ok(Self {
            status: UserStatus::Pending,
            ..Self::try_new(contact_email, created_at)?
        })
    }
}

/// One public key registered to a user — the body of
/// `user-keys/{user_id}/{key_id}.json`. One registration is one machine.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserKey {
    pub key_id: UserKeyId,
    pub public_key: PublicKeyHex,
    /// Freeform description of the machine this key lives on, e.g.
    /// `"work laptop"`: what makes "revoke the laptop" decidable from a list of ids.
    pub key_details: NonEmptyTrimmedString,
    /// When this key was added. Nothing branches on it, but a key nobody
    /// remembers adding is a compromise indicator a date makes visible.
    pub added_at: DateTime<Utc>,
    /// When the key stops signing, for a key made to sign in one browser; a
    /// machine's key has none. Kept after it passes, so the listing shows it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<DateTime<Utc>>,
}

impl From<UserKey> for pipette_plan_types::UserKey {
    fn from(key: UserKey) -> Self {
        Self {
            key_id: key.key_id,
            public_key: key.public_key,
            key_details: key.key_details,
            added_at: key.added_at,
            expires_at: key.expires_at,
        }
    }
}

impl UserKey {
    /// Build a registration for `public_key`, deriving its id.
    pub fn new(
        public_key: PublicKeyHex,
        key_details: NonEmptyTrimmedString,
        added_at: DateTime<Utc>,
    ) -> Self {
        Self {
            key_id: UserKeyId::derive(&public_key),
            public_key,
            key_details,
            added_at,
            expires_at: None,
        }
    }

    /// Whether the key has stopped signing at `now`.
    pub fn expired(&self, now: DateTime<Utc>) -> bool {
        self.expires_at.is_some_and(|at| at <= now)
    }
}

#[cfg(test)]
mod tests {
    use rstest::rstest;

    use super::*;

    fn public_key(byte: u8) -> anyhow::Result<PublicKeyHex> {
        Ok(PublicKeyHex::try_new(hex::encode(
            [byte; PublicKeyHex::BYTE_LEN],
        ))?)
    }

    /// A key id is a pure function of the key, which is what lets the reverse
    /// marker be the uniqueness check: registering the same key twice addresses
    /// the same marker, and two different keys never do.
    #[test]
    fn a_key_id_is_derived_from_the_key_alone() -> anyhow::Result<()> {
        let first = UserKeyId::derive(&public_key(0x11)?);
        let again = UserKeyId::derive(&public_key(0x11)?);
        let other = UserKeyId::derive(&public_key(0x22)?);

        assert_eq!(first, again);
        assert_ne!(first, other);
        assert!(first.as_str().starts_with(UserKeyId::PREFIX));
        Ok(())
    }

    /// Two registrations of one key under different descriptions stay one key.
    /// `key_details` is for a human reading the list and must not enter the
    /// identity.
    #[test]
    fn key_details_do_not_change_a_key_id() -> anyhow::Result<()> {
        let now = Utc::now();
        let laptop = UserKey::new(
            public_key(0x33)?,
            NonEmptyTrimmedString::try_new("work laptop")?,
            now,
        );
        let desktop = UserKey::new(
            public_key(0x33)?,
            NonEmptyTrimmedString::try_new("desktop")?,
            now,
        );

        assert_eq!(laptop.key_id, desktop.key_id);
        Ok(())
    }

    fn user(email: &str) -> anyhow::Result<User> {
        Ok(User::try_new(ContactEmail::try_new(email)?, Utc::now())?)
    }

    /// A new account is active and carries a fresh id under the `user_` prefix
    /// that names its kind. Each mint is fresh, so the id never has to be
    /// checked for collision — only the address does.
    #[test]
    fn a_created_user_is_active_with_a_fresh_id() -> anyhow::Result<()> {
        let first = user("operator@example.com")?;
        let second = user("operator@example.com")?;

        assert_eq!(first.status, UserStatus::Active);
        assert_ne!(first.user_id, second.user_id);
        assert!(first.user_id.as_str().starts_with(UserId::PREFIX));
        Ok(())
    }

    /// A minted id is a hyphenated UUIDv7 under the prefix, and v7's ordering is
    /// what makes a `users/` listing chronological. Asserted because the id form
    /// is a storage-key contract, not an internal detail.
    #[test]
    fn a_minted_id_is_a_time_ordered_uuid() -> anyhow::Result<()> {
        let first = UserId::mint();
        let second = UserId::mint();

        let uuid = first
            .as_str()
            .strip_prefix(UserId::PREFIX)
            .ok_or_else(|| anyhow::anyhow!("a minted id must carry {}", UserId::PREFIX))?;
        assert_eq!(
            uuid::Uuid::parse_str(uuid)?.get_version_num(),
            7,
            "a user id is a UUIDv7"
        );
        assert!(
            uuid.contains('-'),
            "rendered hyphenated, as job and plan ids are"
        );
        assert!(first < second, "v7 ids sort in mint order");
        Ok(())
    }

    /// The address is the account's index key, so one mailbox must be one key.
    /// `ContactEmail` stores what it was given — `Operator@Example.com` round-trips
    /// verbatim — so the fold has to happen here, at creation.
    #[rstest]
    #[case::local_part("Operator@example.com", "operator@example.com")]
    #[case::domain("operator@EXAMPLE.com", "operator@example.com")]
    #[case::already_folded("operator@example.com", "operator@example.com")]
    // Not an ASCII-only fold: `ContactEmail` admits a non-Latin address, and one
    // typed in caps must not claim a second key.
    #[case::non_ascii("ОПЕРАТОР@пример.example", "оператор@пример.example")]
    // A `+` tag is not case, so it survives: it is how a service account gets an
    // address distinguishable from its owner's.
    #[case::plus_tag_survives("Operator+CI@example.com", "operator+ci@example.com")]
    fn a_created_account_folds_its_address(
        #[case] given: &str,
        #[case] stored: &str,
    ) -> anyhow::Result<()> {
        assert_eq!(user(given)?.contact_email.as_str(), stored);
        Ok(())
    }

    /// The whole record, as five keys and nothing else. Asserted on the exact
    /// key set: the serialized shape is the storage contract, so a field added
    /// without a decision has to break here.
    #[test]
    fn the_record_is_five_fields() -> anyhow::Result<()> {
        let body = serde_json::to_value(user("operator@example.com")?)?;
        let mut keys: Vec<&str> = body
            .as_object()
            .ok_or_else(|| anyhow::anyhow!("a user must serialize as an object"))?
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort_unstable();

        assert_eq!(
            keys,
            [
                "account_type",
                "contact_email",
                "created_at",
                "status",
                "user_id"
            ]
        );
        assert_eq!(body["status"], serde_json::json!("active"));
        assert_eq!(body["account_type"], serde_json::json!("human"));
        Ok(())
    }

    /// A record an earlier build wrote, before the type, was a person's.
    #[test]
    fn a_record_without_a_type_is_a_humans() -> anyhow::Result<()> {
        let mut body = serde_json::to_value(User {
            account_type: UserAccountType::Service,
            ..user("ci@example.com")?
        })?;
        body.as_object_mut()
            .and_then(|record| record.remove("account_type"))
            .ok_or_else(|| anyhow::anyhow!("the record carries its type"))?;

        let read: User = serde_json::from_value(body)?;
        assert_eq!(read.account_type, UserAccountType::Human);
        Ok(())
    }

    /// A record an earlier build wrote with the membership on it still reads,
    /// and the next write drops the field.
    #[test]
    fn an_org_id_from_an_earlier_build_is_ignored() -> anyhow::Result<()> {
        let mut body = serde_json::to_value(user("operator@example.com")?)?;
        body["org_id"] = serde_json::json!("org_0195c3e2-a4f3-7b8c-9d1e-2f3a4b5c6d7e");

        let read: User = serde_json::from_value(body)?;
        assert_eq!(serde_json::to_value(read)?.get("org_id"), None);
        Ok(())
    }
}
