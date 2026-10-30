//! A response is gzipped for a caller that asks for it, and only for one.

use std::io::Read;

use axum::http::header;
use http_body_util::BodyExt;
use rstest::rstest;

use pipette_web_testkit::{make_state, setup_benchmarks, unauthed_get, unauthed_get_with_headers};

/// The `/api/v1` answer also passes the test app's response check, which
/// cannot read a compressed body.
#[rstest]
#[case::catalog("/api/v2/benchmarks")]
#[case::operator_api("/api/v1/openapi.json")]
#[tokio::test]
async fn test_a_caller_accepting_gzip_gets_the_same_body_gzipped(
    #[case] path: &str,
) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    setup_benchmarks(dir.path())?;
    let state = make_state(dir.path()).await?;

    let plain = unauthed_get(&state, path).await?;
    assert!(plain.headers().get(header::CONTENT_ENCODING).is_none());
    let plain = plain.into_body().collect().await?.to_bytes();

    let gzip = [("accept-encoding", "gzip".to_owned())];
    let zipped = unauthed_get_with_headers(&state, path, &gzip).await?;
    assert_eq!(
        zipped.headers().get(header::CONTENT_ENCODING),
        Some(&header::HeaderValue::from_static("gzip"))
    );
    let zipped = zipped.into_body().collect().await?.to_bytes();
    let mut unzipped = Vec::new();
    flate2::read::GzDecoder::new(&zipped[..]).read_to_end(&mut unzipped)?;
    assert_eq!(unzipped, plain);
    Ok(())
}
