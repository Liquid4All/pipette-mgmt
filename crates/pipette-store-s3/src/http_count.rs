//! Count each HTTP attempt (retries too) of both S3 clients in [`S3_HTTP`], and
//! log the host and wait of one with no response, which retry logs omit. Tests
//! read process-wide counters, so each needs its own process (`cargo nextest`).

use std::time::Duration;

use async_trait::async_trait;
use aws_sdk_s3::config::interceptors::{
    BeforeDeserializationInterceptorContextRef, BeforeTransmitInterceptorContextRef,
    FinalizerInterceptorContextRef,
};
use aws_sdk_s3::config::{ConfigBag, Intercept, RuntimeComponents};
use aws_smithy_types::config_bag::{Storable, StoreReplace};
use object_store::ClientOptions;
use object_store::client::{
    HttpClient, HttpConnector, HttpError, HttpRequest, HttpResponse, HttpService, ReqwestConnector,
};

use pipette_store::http_stats::{HttpOutcome, InFlight, S3_HTTP};

/// The `object_store` connector that counts each attempt.
#[derive(Debug, Default)]
pub(crate) struct CountingConnector;

impl HttpConnector for CountingConnector {
    fn connect(&self, options: &ClientOptions) -> object_store::Result<HttpClient> {
        let inner = ReqwestConnector::default().connect(options)?;
        Ok(HttpClient::new(CountingService { inner }))
    }
}

#[derive(Debug)]
struct CountingService {
    inner: HttpClient,
}

#[async_trait]
impl HttpService for CountingService {
    async fn call(&self, req: HttpRequest) -> Result<HttpResponse, HttpError> {
        let method = req.method().clone();
        let uri = req.uri().clone();
        let in_flight = S3_HTTP.start();
        let response = self.inner.execute(req).await;
        match &response {
            Ok(response) => in_flight.finish(HttpOutcome::Status(response.status().as_u16())),
            Err(error) => {
                log_transport_error(
                    "object_store",
                    method.as_str(),
                    uri.host().unwrap_or_default(),
                    uri.path(),
                    in_flight.elapsed(),
                    &error_chain(error),
                );
                in_flight.finish(HttpOutcome::TransportError);
            }
        }
        response
    }
}

/// The `aws-sdk-s3` interceptor that counts each attempt.
#[derive(Debug, Default)]
pub(crate) struct CountingInterceptor;

/// Kept in the config bag between hooks. An operation that stops early drops
/// it, and the drop takes it out of the in-flight count.
#[derive(Debug)]
struct Attempt(Option<AttemptState>);

#[derive(Debug)]
struct AttemptState {
    in_flight: InFlight<'static>,
    method: String,
    uri: String,
}

impl Storable for Attempt {
    type Storer = StoreReplace<Self>;
}

impl Intercept for CountingInterceptor {
    fn name(&self) -> &'static str {
        "CountingInterceptor"
    }

    fn read_before_transmit(
        &self,
        context: &BeforeTransmitInterceptorContextRef<'_>,
        _runtime_components: &RuntimeComponents,
        cfg: &mut ConfigBag,
    ) -> Result<(), aws_sdk_s3::error::BoxError> {
        let request = context.request();
        cfg.interceptor_state()
            .store_put(Attempt(Some(AttemptState {
                in_flight: S3_HTTP.start(),
                method: request.method().to_owned(),
                uri: request.uri().to_owned(),
            })));
        Ok(())
    }

    /// The response headers arrived, and the client has not read the body.
    fn read_after_transmit(
        &self,
        context: &BeforeDeserializationInterceptorContextRef<'_>,
        _runtime_components: &RuntimeComponents,
        cfg: &mut ConfigBag,
    ) -> Result<(), aws_sdk_s3::error::BoxError> {
        if let Some(state) = take_attempt(cfg) {
            let status = context.response().status().as_u16();
            state.in_flight.finish(HttpOutcome::Status(status));
        }
        Ok(())
    }

    /// An attempt that is still in flight here got no response.
    fn read_after_attempt(
        &self,
        context: &FinalizerInterceptorContextRef<'_>,
        _runtime_components: &RuntimeComponents,
        cfg: &mut ConfigBag,
    ) -> Result<(), aws_sdk_s3::error::BoxError> {
        let Some(state) = take_attempt(cfg) else {
            return Ok(());
        };
        let (host, path) = split_uri(&state.uri);
        let error = context
            .output_or_error()
            .and_then(Result::err)
            .map(|error| error_chain(error))
            .unwrap_or_default();
        log_transport_error(
            "aws_sdk_s3",
            &state.method,
            host,
            path,
            state.in_flight.elapsed(),
            &error,
        );
        state.in_flight.finish(HttpOutcome::TransportError);
        Ok(())
    }
}

fn take_attempt(cfg: &mut ConfigBag) -> Option<AttemptState> {
    cfg.get_mut_from_interceptor_state::<Attempt>()
        .and_then(|attempt| attempt.0.take())
}

fn split_uri(uri: &str) -> (&str, &str) {
    let rest = uri.split_once("://").map_or(uri, |(_, rest)| rest);
    let rest = rest.split_once('?').map_or(rest, |(before, _)| before);
    rest.find('/')
        .map_or((rest, "/"), |slash| rest.split_at(slash))
}

/// The message of `error` and of each cause under it, joined with `: `. The
/// top error of each client names only the kind of failure, such as `timeout`.
fn error_chain(error: &(dyn std::error::Error + 'static)) -> String {
    std::iter::successors(Some(error), |error| error.source())
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(": ")
}

fn log_transport_error(
    client: &'static str,
    method: &str,
    host: &str,
    path: &str,
    waited: Duration,
    error: &str,
) {
    tracing::warn!(
        client,
        method,
        host,
        path,
        waited_ms = u64::try_from(waited.as_millis()).unwrap_or(u64::MAX),
        error,
        "S3 request attempt got no response"
    );
}

#[cfg(test)]
mod tests {
    use aws_sdk_s3::operation::get_object::GetObjectError;
    use aws_sdk_s3::types::error::NoSuchKey;
    use aws_smithy_mocks::{RuleMode, mock};
    use object_store::ObjectStoreExt;
    use object_store::aws::AmazonS3Builder;
    use object_store::path::Path as ObjPath;
    use object_store::{ObjectStore, RetryConfig};
    use rstest::rstest;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    use super::*;

    fn client_for(addr: std::net::SocketAddr) -> anyhow::Result<impl ObjectStore> {
        Ok(builder_for(addr)
            .with_client_options(
                ClientOptions::new()
                    .with_allow_http(true)
                    .with_timeout(Duration::from_millis(200)),
            )
            .build()?)
    }

    fn builder_for(addr: std::net::SocketAddr) -> AmazonS3Builder {
        AmazonS3Builder::new()
            .with_bucket_name("bucket")
            .with_region("us-east-1")
            .with_access_key_id("key")
            .with_secret_access_key("secret")
            .with_endpoint(format!("http://{addr}"))
            .with_allow_http(true)
            .with_http_connector(CountingConnector)
            .with_retry(RetryConfig {
                max_retries: 0,
                ..RetryConfig::default()
            })
    }

    /// A server that accepts each connection and never answers.
    async fn silent_server() -> anyhow::Result<std::net::SocketAddr> {
        let silent = TcpListener::bind("127.0.0.1:0").await?;
        let addr = silent.local_addr()?;
        tokio::spawn(async move {
            let mut held = Vec::new();
            while let Ok((socket, _)) = silent.accept().await {
                held.push(socket);
            }
        });
        Ok(addr)
    }

    /// The connector counts an answered attempt by its status, and an attempt
    /// that times out as a transport error.
    #[tokio::test]
    async fn the_connector_counts_each_attempt() -> anyhow::Result<()> {
        // Answers each request with a 404.
        let answers = TcpListener::bind("127.0.0.1:0").await?;
        let answers_addr = answers.local_addr()?;
        tokio::spawn(async move {
            while let Ok((mut socket, _)) = answers.accept().await {
                let mut request = [0u8; 4096];
                if socket.read(&mut request).await.is_ok() {
                    let _ = socket
                        .write_all(b"HTTP/1.1 404 Not Found\r\ncontent-length: 0\r\n\r\n")
                        .await;
                }
            }
        });
        let silent_addr = silent_server().await?;

        let start = S3_HTTP.checkpoint();
        let missing = client_for(answers_addr)?.get(&ObjPath::from("key")).await;
        assert!(
            matches!(missing, Err(object_store::Error::NotFound { .. })),
            "{missing:?}"
        );
        let timed_out = client_for(silent_addr)?.get(&ObjPath::from("key")).await;
        assert!(timed_out.is_err());

        let counts = S3_HTTP.checkpoint().since(start);
        assert_eq!(counts.requests, 2);
        assert_eq!(counts.not_found, 1);
        assert_eq!(counts.transport_errors, 1);
        assert!(counts.wait >= Duration::from_millis(200), "{counts:?}");
        Ok(())
    }

    /// `read_timeout_secs` fails a request that gets no bytes, well before the
    /// 30 s request timeout of the client.
    #[tokio::test]
    async fn the_read_timeout_stops_a_silent_request() -> anyhow::Result<()> {
        let addr = silent_server().await?;
        let (key, value) = crate::read_timeout(std::num::NonZeroU64::MIN);
        let client = builder_for(addr).with_config(key, value).build()?;

        let start = S3_HTTP.checkpoint();
        let read = tokio::time::timeout(Duration::from_secs(10), client.get(&ObjPath::from("key")))
            .await?;
        assert!(read.is_err());

        let counts = S3_HTTP.checkpoint().since(start);
        assert_eq!(counts.transport_errors, 1);
        assert!(counts.wait >= Duration::from_secs(1), "{counts:?}");
        assert!(counts.wait < Duration::from_secs(5), "{counts:?}");
        Ok(())
    }

    /// The mock HTTP client answers each modeled response with a 418, so the
    /// attempt counts as a client error.
    #[tokio::test]
    async fn the_interceptor_counts_each_attempt() -> anyhow::Result<()> {
        let not_found = mock!(aws_sdk_s3::Client::get_object)
            .then_error(|| GetObjectError::NoSuchKey(NoSuchKey::builder().build()));
        let client = aws_smithy_mocks::mock_client!(
            aws_sdk_s3,
            RuleMode::Sequential,
            &[&not_found],
            |conf| conf.interceptor(CountingInterceptor)
        );

        let start = S3_HTTP.checkpoint();
        let response = client.get_object().bucket("bucket").key("key").send().await;
        assert!(response.is_err());

        let counts = S3_HTTP.checkpoint().since(start);
        assert_eq!(counts.requests, 1);
        assert_eq!(counts.client_errors, 1);
        assert_eq!(counts.peak_in_flight, 1);
        Ok(())
    }

    #[rstest]
    #[case::virtual_host(
        "https://bucket.s3.us-east-1.amazonaws.com/todo/avail/job-1?x-id=GetObject",
        "bucket.s3.us-east-1.amazonaws.com",
        "/todo/avail/job-1"
    )]
    #[case::path_style("http://localhost:9000/bucket/key", "localhost:9000", "/bucket/key")]
    #[case::no_path("https://bucket.example.com?list-type=2", "bucket.example.com", "/")]
    fn split_uri_gives_the_host_and_the_path(
        #[case] uri: &str,
        #[case] host: &str,
        #[case] path: &str,
    ) {
        assert_eq!(split_uri(uri), (host, path));
    }

    #[tokio::test]
    async fn the_interceptor_counts_an_attempt_with_no_response() -> anyhow::Result<()> {
        let addr = silent_server().await?;
        let config = aws_sdk_s3::Config::builder()
            .behavior_version(aws_sdk_s3::config::BehaviorVersion::latest())
            .region(aws_sdk_s3::config::Region::new("us-east-1"))
            .credentials_provider(aws_sdk_s3::config::Credentials::for_tests())
            .endpoint_url(format!("http://{addr}"))
            .force_path_style(true)
            .retry_config(aws_sdk_s3::config::retry::RetryConfig::disabled())
            .timeout_config(
                aws_sdk_s3::config::timeout::TimeoutConfig::builder()
                    .read_timeout(Duration::from_millis(200))
                    .build(),
            )
            .interceptor(CountingInterceptor)
            .build();
        let client = aws_sdk_s3::Client::from_conf(config);

        let start = S3_HTTP.checkpoint();
        let response = client.get_object().bucket("bucket").key("key").send().await;
        assert!(response.is_err());

        let counts = S3_HTTP.checkpoint().since(start);
        assert_eq!(counts.requests, 1);
        assert_eq!(counts.transport_errors, 1);
        assert_eq!(
            S3_HTTP.checkpoint().peak_in_flight,
            0,
            "nothing is in flight"
        );
        Ok(())
    }
}
