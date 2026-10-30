//! Serves the app over HTTP/1.1 with the timeouts `axum::serve` leaves out: it
//! gives hyper no timer, so a client that never finishes its request holds its
//! connection, and a graceful shutdown, forever.

use std::future::Future;
use std::num::NonZeroUsize;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::extract::ConnectInfo;
use axum::http::Request;
use axum::serve::Listener;
use hyper::body::Incoming;
use hyper_util::rt::{TokioIo, TokioTimer};
use hyper_util::server::graceful::GracefulShutdown;
use hyper_util::service::TowerToHyperService;
use tokio::sync::Semaphore;
use tower::ServiceExt;
use tower_http::timeout::RequestBodyTimeoutLayer;

pub struct Limits {
    /// For a request's headers (an idle keep-alive connection's next request
    /// included), and for each chunk of its body.
    pub read: Duration,
    /// For the connections still open once the shutdown signal fires.
    pub drain: Duration,
    /// Bounds the descriptors a trickled body or an unread answer can hold,
    /// which no timeout above does.
    pub connections: NonZeroUsize,
}

/// HTTP/1.1 only: hyper-util's auto builder does not time out the HTTP/2
/// preface sniff, and hyper bounds no HTTP/2 connection's idle time.
pub async fn serve<L: Listener>(
    mut listener: L,
    app: Router,
    shutdown: impl Future<Output = ()>,
    limits: Limits,
) where
    L::Addr: Clone + Sync + 'static,
{
    let app = app.layer(RequestBodyTimeoutLayer::new(limits.read));
    let mut http = hyper::server::conn::http1::Builder::new();
    http.timer(TokioTimer::new())
        .header_read_timeout(limits.read);
    let graceful = GracefulShutdown::new();
    let permits = Arc::new(Semaphore::new(limits.connections.get()));
    let mut shutdown = std::pin::pin!(shutdown);
    loop {
        // The permit comes first, so a connection past the cap waits in the
        // kernel's backlog instead of holding a descriptor.
        let accepted = async {
            let permit = permits.clone().acquire_owned().await;
            (permit, listener.accept().await)
        };
        let (permit, (io, peer)) = tokio::select! {
            accepted = accepted => accepted,
            () = &mut shutdown => break,
        };
        // Never closed, so never an error.
        let Ok(permit) = permit else { break };
        // As `axum::serve` does, so `ConnectInfo` extracts the peer.
        let app = app
            .clone()
            .map_request(move |mut request: Request<Incoming>| {
                request.extensions_mut().insert(ConnectInfo(peer.clone()));
                request
            });
        let conn = http.serve_connection(TokioIo::new(io), TowerToHyperService::new(app));
        let conn = graceful.watch(conn);
        tokio::spawn(async move {
            if let Err(error) = conn.await {
                tracing::debug!(%error, "connection closed");
            }
            drop(permit);
        });
    }
    drop(listener);
    if tokio::time::timeout(limits.drain, graceful.shutdown())
        .await
        .is_err()
    {
        tracing::warn!(
            wait_secs = limits.drain.as_secs(),
            "connections still open at shutdown are abandoned when the process exits"
        );
    }
}

#[cfg(test)]
mod tests {
    use std::net::SocketAddr;
    use std::num::NonZeroUsize;
    use std::sync::Arc;
    use std::time::Duration;

    use axum::Router;
    use axum::extract::ConnectInfo;
    use axum::routing::{get, post};
    use rstest::rstest;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{TcpListener, TcpStream};
    use tokio::sync::Notify;

    use super::{Limits, serve};

    /// Well past every timeout below, so a hang fails the test instead of
    /// stalling it.
    const GUARD: Duration = Duration::from_secs(5);

    const TRICKLED: &str = "POST /echo HTTP/1.1\r\nhost: x\r\ncontent-length: 10\r\n\r\nabc";
    const COMPLETE: &str = "GET / HTTP/1.1\r\nhost: x\r\nconnection: close\r\n\r\n";

    fn limits() -> Limits {
        Limits {
            read: Duration::from_millis(200),
            drain: Duration::from_millis(200),
            connections: NonZeroUsize::MIN,
        }
    }

    fn app() -> Router {
        Router::new()
            .route("/", get(|| async { "ok" }))
            .route("/echo", post(|body: String| async { body }))
    }

    /// The first line of what the server sent before it closed the connection.
    async fn answer(stream: &mut TcpStream) -> anyhow::Result<String> {
        let mut read = Vec::new();
        tokio::time::timeout(GUARD, stream.read_to_end(&mut read)).await??;
        Ok(String::from_utf8_lossy(&read)
            .lines()
            .next()
            .unwrap_or_default()
            .to_string())
    }

    #[rstest]
    #[case::silent("", "")]
    #[case::slow_headers("GET / HTTP/1.1\r\nhost: x\r\n", "")]
    #[case::slow_body(TRICKLED, "HTTP/1.1 400 Bad Request")]
    #[case::idle_keep_alive("GET / HTTP/1.1\r\nhost: x\r\n\r\n", "HTTP/1.1 200 OK")]
    #[case::complete(COMPLETE, "HTTP/1.1 200 OK")]
    #[tokio::test]
    async fn a_stalled_client_loses_its_connection(
        #[case] sent: &str,
        #[case] expected: &str,
    ) -> anyhow::Result<()> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let addr = listener.local_addr()?;
        tokio::spawn(serve(listener, app(), std::future::pending(), limits()));

        let mut stream = TcpStream::connect(addr).await?;
        stream.write_all(sent.as_bytes()).await?;
        let got = answer(&mut stream).await?;
        anyhow::ensure!(got == expected, "answered {got:?}, expected {expected:?}");
        Ok(())
    }

    #[tokio::test]
    async fn a_request_knows_its_peer() -> anyhow::Result<()> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let addr = listener.local_addr()?;
        let app =
            Router::new().route(
                "/",
                get(|ConnectInfo(peer): ConnectInfo<SocketAddr>| async move {
                    format!("{}", peer.ip())
                }),
            );
        tokio::spawn(serve(listener, app, std::future::pending(), limits()));

        let mut stream = TcpStream::connect(addr).await?;
        stream.write_all(COMPLETE.as_bytes()).await?;
        let mut read = Vec::new();
        tokio::time::timeout(GUARD, stream.read_to_end(&mut read)).await??;
        let read = String::from_utf8_lossy(&read);
        anyhow::ensure!(read.ends_with("127.0.0.1"), "answered {read:?}");
        Ok(())
    }

    #[tokio::test]
    async fn a_connection_past_the_cap_waits_for_a_free_one() -> anyhow::Result<()> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let addr = listener.local_addr()?;
        let limits = Limits {
            read: GUARD * 2,
            ..limits()
        };
        tokio::spawn(serve(listener, app(), std::future::pending(), limits));

        let mut trickle = TcpStream::connect(addr).await?;
        trickle.write_all(TRICKLED.as_bytes()).await?;
        let mut waiting = TcpStream::connect(addr).await?;
        waiting.write_all(COMPLETE.as_bytes()).await?;
        let mut byte = [0; 1];
        let early = tokio::time::timeout(Duration::from_millis(300), waiting.read(&mut byte)).await;
        anyhow::ensure!(early.is_err(), "served past the cap: {early:?}");

        drop(trickle);
        let got = answer(&mut waiting).await?;
        anyhow::ensure!(
            got == "HTTP/1.1 200 OK",
            "answered {got:?} once the cap freed"
        );
        Ok(())
    }

    #[tokio::test]
    async fn shutdown_does_not_wait_on_a_request_that_never_ends() -> anyhow::Result<()> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let addr = listener.local_addr()?;
        let entered = Arc::new(Notify::new());
        let stop = Arc::new(Notify::new());
        let app = Router::new().route(
            "/hang",
            get({
                let entered = entered.clone();
                move || async move {
                    entered.notify_one();
                    std::future::pending::<()>().await;
                }
            }),
        );
        let shutdown = {
            let stop = stop.clone();
            async move { stop.notified().await }
        };
        let server = tokio::spawn(serve(listener, app, shutdown, limits()));

        let mut stream = TcpStream::connect(addr).await?;
        stream
            .write_all(b"GET /hang HTTP/1.1\r\nhost: x\r\n\r\n")
            .await?;
        tokio::time::timeout(GUARD, entered.notified()).await?;
        stop.notify_one();
        tokio::time::timeout(GUARD, server).await??;
        Ok(())
    }
}
