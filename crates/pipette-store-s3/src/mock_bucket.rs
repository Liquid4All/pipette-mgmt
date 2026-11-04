//! Mock S3 buckets for trait-level store tests. This module is test-only. It
//! is not in a testkit crate, because `aws-smithy-mocks` would add 23 unlinked
//! crates to `THIRD-PARTY-LICENSES.md`. The queue-sweep test in
//! `pipette-pipeline` (`queue_maintenance.rs`) copies [`bucket_list_rule`].

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use aws_sdk_s3::error::ErrorMetadata;
use aws_sdk_s3::operation::delete_object::DeleteObjectOutput;
use aws_sdk_s3::operation::get_object::{GetObjectError, GetObjectOutput};
use aws_sdk_s3::operation::list_objects_v2::ListObjectsV2Output;
use aws_sdk_s3::operation::put_object::PutObjectOutput;
use aws_sdk_s3::operation::rename_object::{RenameObjectError, RenameObjectOutput};
use aws_sdk_s3::primitives::{ByteStream, DateTime as AwsDateTime};
use aws_sdk_s3::types::Object;
use aws_sdk_s3::types::error::NoSuchKey;
use aws_smithy_mocks::{MockResponse, Rule, RuleMode, mock};
use chrono::{DateTime, SubsecRound, Utc};

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

/// The objects of a [`fake_bucket`]: each key's body and the time of its write.
type ObjectMap = BTreeMap<String, (Vec<u8>, DateTime<Utc>)>;
type Objects = Arc<Mutex<ObjectMap>>;

fn lock(objects: &Objects) -> MutexGuard<'_, ObjectMap> {
    objects.lock().unwrap_or_else(PoisonError::into_inner)
}

fn aws_time(at: DateTime<Utc>) -> AwsDateTime {
    AwsDateTime::from_secs_and_nanos(at.timestamp(), at.timestamp_subsec_nanos())
}

/// A client for a bucket that keeps its objects in memory, for a test that
/// needs a bucket with state. It answers the five operations of
/// [`S3TodoStore`]:
///
/// - A put stores the body with the current time, in whole seconds as S3
///   reports it, as its `LastModified`.
/// - A get of an absent key is `NoSuchKey`.
/// - A delete of an absent key succeeds.
/// - A listing is one page, in reverse key order. A directory bucket lists in
///   no defined order. A store that depends on the order gets a wrong result.
/// - A rename moves the object and replaces the destination. A rename with a
///   missing source returns an error with only the `NoSuchKey` code, as S3
///   does.
pub(crate) fn fake_bucket() -> aws_sdk_s3::Client {
    let objects = Objects::default();
    let put = {
        let objects = objects.clone();
        mock!(aws_sdk_s3::Client::put_object).then_compute_response(move |req| {
            let body = req.body().bytes().unwrap_or_default().to_vec();
            let key = req.key().unwrap_or_default().to_string();
            lock(&objects).insert(key, (body, Utc::now().trunc_subsecs(0)));
            MockResponse::Output(PutObjectOutput::builder().build())
        })
    };
    let get = {
        let objects = objects.clone();
        mock!(aws_sdk_s3::Client::get_object).then_compute_response(move |req| {
            match lock(&objects).get(req.key().unwrap_or_default()) {
                Some((body, at)) => MockResponse::Output(
                    GetObjectOutput::builder()
                        .body(ByteStream::from(body.clone()))
                        .last_modified(aws_time(*at))
                        .build(),
                ),
                None => {
                    MockResponse::Error(GetObjectError::NoSuchKey(NoSuchKey::builder().build()))
                }
            }
        })
    };
    let delete = {
        let objects = objects.clone();
        mock!(aws_sdk_s3::Client::delete_object).then_compute_response(move |req| {
            lock(&objects).remove(req.key().unwrap_or_default());
            MockResponse::Output(DeleteObjectOutput::builder().build())
        })
    };
    let list = {
        let objects = objects.clone();
        mock!(aws_sdk_s3::Client::list_objects_v2).then_compute_response(move |req| {
            let prefix = req.prefix().unwrap_or_default();
            let contents: Vec<Object> = lock(&objects)
                .iter()
                .rev()
                .filter(|(key, _)| key.starts_with(prefix))
                .map(|(key, (_, at))| {
                    Object::builder()
                        .key(key)
                        .last_modified(aws_time(*at))
                        .build()
                })
                .collect();
            MockResponse::Output(
                ListObjectsV2Output::builder()
                    .set_contents(Some(contents))
                    .is_truncated(false)
                    .build(),
            )
        })
    };
    let rename = mock!(aws_sdk_s3::Client::rename_object).then_compute_response(move |req| {
        let mut objects = lock(&objects);
        match objects.remove(req.rename_source().unwrap_or_default()) {
            Some(object) => {
                objects.insert(req.key().unwrap_or_default().to_string(), object);
                MockResponse::Output(RenameObjectOutput::builder().build())
            }
            None => MockResponse::Error(RenameObjectError::generic(
                ErrorMetadata::builder()
                    .code("NoSuchKey")
                    .message("the rename source is gone")
                    .build(),
            )),
        }
    });
    aws_smithy_mocks::mock_client!(
        aws_sdk_s3,
        RuleMode::MatchAny,
        &[&put, &get, &delete, &list, &rename]
    )
}
