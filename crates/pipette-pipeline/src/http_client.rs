//! The `reqwest` builder the pipeline's own clients start from, and the check
//! on a path segment they build from catalog-written ids.

use reqwest::{ClientBuilder, Url};

/// A builder for a client that calls `endpoint`. A loopback endpoint skips the
/// system proxy lookup: no proxy reaches it, and on macOS the lookup lists the
/// executable's directory, which is slow when that directory is large.
pub(crate) fn builder_for(endpoint: &str) -> ClientBuilder {
    let builder = reqwest::Client::builder();
    match Url::parse(endpoint) {
        Ok(url) if is_loopback(&url) => builder.no_proxy(),
        _ => builder,
    }
}

/// Whether `s` stays one path segment: the url crate silently drops `.`/`..`
/// segments and strips tab/newline (so `.\t.` would become `..`).
pub(crate) fn is_plain_path_segment(s: &str) -> bool {
    !matches!(s, "" | "." | "..") && !s.chars().any(char::is_control)
}

fn is_loopback(url: &Url) -> bool {
    match url.host_str() {
        Some(host) if host.eq_ignore_ascii_case("localhost") => true,
        Some(host) => host
            .trim_start_matches('[')
            .trim_end_matches(']')
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback()),
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use rstest::rstest;

    use super::*;

    #[rstest]
    #[case::v4("http://127.0.0.1:9/", true)]
    #[case::v4_range("http://127.1.2.3/", true)]
    #[case::v6("http://[::1]:8080/", true)]
    #[case::name("http://LocalHost:9999/", true)]
    #[case::hub("https://huggingface.co", false)]
    #[case::lan("http://10.0.0.5/", false)]
    #[case::localhost_lookalike("http://localhost.example.com/", false)]
    fn only_a_loopback_endpoint_skips_the_proxy(
        #[case] endpoint: &str,
        #[case] expected: bool,
    ) -> anyhow::Result<()> {
        assert_eq!(is_loopback(&Url::parse(endpoint)?), expected);
        Ok(())
    }
}
