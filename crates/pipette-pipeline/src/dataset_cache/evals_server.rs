//! The probe of an `evals-server@1` upstream, which reads only
//! `/samples/digest`. The scores service packs the corpus itself.

use anyhow::Context;

use pipette_plan_types::{DatasetUpstream, Sha256, parse_samples_digest};

/// What a probe read off the evals server.
pub(super) enum Reached {
    Digest(Sha256),
    /// The evals server answers, but no longer serves these samples.
    Gone(String),
    Unavailable(String),
}

/// `{base}/evals/{eval}/datasets/{version}/samples/digest[?prompt_profile=]`.
fn samples_url(base: &str, upstream: &DatasetUpstream) -> anyhow::Result<reqwest::Url> {
    let DatasetUpstream::EvalsServer {
        eval_id,
        dataset_version,
        prompt_profile,
        ..
    } = upstream
    else {
        anyhow::bail!("{} is not an evals-server upstream", upstream.reference());
    };
    if let Some(bad) = [eval_id.as_ref(), dataset_version.as_ref()]
        .into_iter()
        .find(|s| !crate::http_client::is_plain_path_segment(s))
    {
        anyhow::bail!("{bad:?} cannot be an evals server url path segment");
    }
    let mut url = reqwest::Url::parse(base)
        .with_context(|| format!("evals_server_url {base:?} is not a URL"))?;
    url.path_segments_mut()
        .map_err(|()| anyhow::anyhow!("evals_server_url {base:?} cannot carry a path"))?
        .pop_if_empty()
        .extend([
            "evals",
            eval_id.as_ref(),
            "datasets",
            dataset_version.as_ref(),
            "samples",
            "digest",
        ]);
    if let Some(profile) = prompt_profile {
        url.query_pairs_mut()
            .append_pair("prompt_profile", profile.as_ref());
    }
    Ok(url)
}

/// Enough of an error body to recognise it, never a whole one.
fn excerpt(body: &str) -> String {
    body.chars().take(300).collect()
}

/// `GET …/samples/digest`'s body, of which the probe reads the digest.
#[derive(serde::Deserialize)]
struct DigestResponse {
    digest: String,
}

/// The digest the evals server reports for the pinned samples, in one small
/// request.
pub(super) async fn probe(
    http: &reqwest::Client,
    base: &str,
    upstream: &DatasetUpstream,
) -> Reached {
    let url = match samples_url(base, upstream) {
        Ok(url) => url,
        Err(e) => return Reached::Unavailable(format!("{e:#}")),
    };
    let response = match http.get(url).send().await {
        Ok(response) => response,
        Err(e) => return Reached::Unavailable(format!("evals server unreachable: {e}")),
    };
    let status = response.status();
    let body = response.text().await.unwrap_or_default();
    // 404: no such eval or dataset version; 400: no such prompt profile for it.
    // Either way the evals server no longer publishes what the pin names.
    if status == reqwest::StatusCode::NOT_FOUND || status == reqwest::StatusCode::BAD_REQUEST {
        return Reached::Gone(format!(
            "evals server answers {status} for {}: {}",
            upstream.reference(),
            excerpt(&body)
        ));
    }
    if !status.is_success() {
        return Reached::Unavailable(format!("evals server answers {status}: {}", excerpt(&body)));
    }
    match serde_json::from_str::<DigestResponse>(&body)
        .ok()
        .and_then(|r| parse_samples_digest(&r.digest))
    {
        Some(digest) => Reached::Digest(digest),
        None => Reached::Unavailable(format!(
            "evals server's digest response is unreadable: {}",
            excerpt(&body)
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[rstest::rstest]
    #[case::bare(
        "http://scores:8000",
        None,
        "http://scores:8000/evals/mmstar/datasets/2026.10.1/samples/digest"
    )]
    #[case::trailing_slash(
        "http://scores:8000/",
        None,
        "http://scores:8000/evals/mmstar/datasets/2026.10.1/samples/digest"
    )]
    #[case::profile(
        "http://scores:8000",
        Some("qwen3_5"),
        "http://scores:8000/evals/mmstar/datasets/2026.10.1/samples/digest?prompt_profile=qwen3_5"
    )]
    fn the_url_names_the_pinned_samples(
        #[case] base: &str,
        #[case] profile: Option<&str>,
        #[case] expected: &str,
    ) -> anyhow::Result<()> {
        let upstream: DatasetUpstream = serde_json::from_value(serde_json::json!({
            "source": "evals_server",
            "eval_id": "mmstar",
            "dataset_version": "2026.10.1",
            "prompt_profile": profile,
            "identity": "1".repeat(64),
        }))?;
        assert_eq!(samples_url(base, &upstream)?.as_str(), expected);
        Ok(())
    }

    #[rstest::rstest]
    #[case::dot_dot_eval("..", "2026.10.1")]
    #[case::tab_dot_dot_version("mmstar", ".\t.")]
    fn a_dot_dot_url_segment_is_refused(
        #[case] eval_id: &str,
        #[case] dataset_version: &str,
    ) -> anyhow::Result<()> {
        let upstream: DatasetUpstream = serde_json::from_value(serde_json::json!({
            "source": "evals_server",
            "eval_id": eval_id,
            "dataset_version": dataset_version,
            "identity": "1".repeat(64),
        }))?;
        assert!(samples_url("http://scores:8000", &upstream).is_err());
        Ok(())
    }
}
