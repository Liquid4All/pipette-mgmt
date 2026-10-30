//! A mock S3 bucket for trait-level store tests. Test-only, not in a testkit
//! crate, whose `aws-smithy-mocks` would add 23 unlinked crates to
//! `THIRD-PARTY-LICENSES.md`; the queue-sweep test in `pipette-mgmt` copies it.

use aws_sdk_s3::operation::list_objects_v2::ListObjectsV2Output;
use aws_sdk_s3::types::Object;
use aws_smithy_mocks::{Rule, mock};

use super::S3TodoStore;

pub(crate) fn store(client: aws_sdk_s3::Client) -> S3TodoStore {
    S3TodoStore::new(client, "test-bucket", "")
}

/// Answers like a bucket holding `keys`, filtered by the requested prefix, so a
/// store that addresses the wrong prefix reads a wrong answer.
pub(crate) fn bucket_list_rule(keys: &'static [&'static str]) -> Rule {
    mock!(aws_sdk_s3::Client::list_objects_v2).then_compute_output(move |req| {
        let prefix = req.prefix().unwrap_or_default().to_string();
        let contents: Vec<Object> = keys
            .iter()
            .filter(|key| key.starts_with(&prefix))
            .map(|key| Object::builder().key(*key).build())
            .collect();
        ListObjectsV2Output::builder()
            .set_contents(Some(contents))
            .is_truncated(false)
            .build()
    })
}
