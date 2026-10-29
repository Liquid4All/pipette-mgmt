//! A ceiling on the registrations no pre-auth key admits, so an anonymous caller
//! cannot fill the stores and the approval queue at request rate.

use std::collections::HashMap;
use std::convert::Infallible;
use std::net::{IpAddr, Ipv6Addr, SocketAddr};
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

use axum::extract::{ConnectInfo, FromRequestParts};
use axum::http::HeaderMap;
use axum::http::request::Parts;

use crate::error::AppError;
use crate::state::AppState;

/// Keyless registrations, devices and accounts together, admitted per [`WINDOW`].
pub const KEYLESS_REGISTRATIONS_PER_WINDOW: u32 = 20;

/// The share of [`KEYLESS_REGISTRATIONS_PER_WINDOW`] one client address may
/// take: a device or an account registers once.
pub const KEYLESS_REGISTRATIONS_PER_CLIENT: u32 = 1;

const WINDOW: Duration = Duration::from_secs(60);

/// The address a request came from, as `trusted_proxy_hops` says to read it.
/// `None` when it names none, as for a request that never crossed a socket.
///
/// Sound only when exactly `trusted_proxy_hops` proxies stand in front of
/// `serve` and each appends the address it received from to
/// `X-Forwarded-For`; entries further left are whatever the caller sent. Our
/// load balancer appends because its deployment template sets it to, which
/// nothing here detects or requires: another proxy chain needs its own count,
/// or this limit falls on one of the proxies and refuses every keyless
/// registration behind it. Clients behind one NAT share one address, and so
/// one share.
pub struct ClientIp(pub Option<IpAddr>);

impl FromRequestParts<AppState> for ClientIp {
    type Rejection = Infallible;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let peer = parts
            .extensions
            .get::<ConnectInfo<SocketAddr>>()
            .map(|ConnectInfo(addr)| addr.ip());
        Ok(Self(client_ip(
            &parts.headers,
            peer,
            state.config.trusted_proxy_hops,
        )))
    }
}

/// The `X-Forwarded-For` entry `hops` from the right, across every such
/// header in order, or the peer when `hops` is `0`.
fn client_ip(headers: &HeaderMap, peer: Option<IpAddr>, hops: usize) -> Option<IpAddr> {
    let Some(nth_from_right) = hops.checked_sub(1) else {
        return peer;
    };
    let entries: Vec<&str> = headers
        .get_all("x-forwarded-for")
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .collect();
    entries
        .iter()
        .rev()
        .nth(nth_from_right)?
        .trim()
        .parse()
        .ok()
}

/// A fixed window over this process's keyless registrations, with a share for
/// each client address.
///
/// shortcut: the [`KEYLESS_REGISTRATIONS_PER_WINDOW`] ceiling is per process,
/// so N instances admit N times it; move the count to the shared store once a
/// deployment runs more than the two tasks behind the load balancer.
pub struct RegistrationThrottle {
    window: Mutex<Window>,
}

struct Window {
    start: Instant,
    admitted: u32,
    /// Holds only admitted clients, so it never outgrows the window's ceiling.
    by_client: HashMap<IpAddr, u32>,
}

impl RegistrationThrottle {
    pub fn new() -> Self {
        Self {
            window: Mutex::new(Window {
                start: Instant::now(),
                admitted: 0,
                by_client: HashMap::new(),
            }),
        }
    }

    /// Count one keyless registration from `client`, or refuse it with a `429`
    /// whose `Retry-After` is the rest of the window.
    pub fn admit(&self, client: Option<IpAddr>) -> Result<(), AppError> {
        self.admit_at(client, Instant::now())
    }

    fn admit_at(&self, client: Option<IpAddr>, now: Instant) -> Result<(), AppError> {
        let mut window = self.window.lock().unwrap_or_else(PoisonError::into_inner);
        if now.saturating_duration_since(window.start) >= WINDOW {
            *window = Window {
                start: now,
                admitted: 0,
                by_client: HashMap::new(),
            };
        }
        let key = client.map(client_key);
        let from_client = key.map_or(0, |key| window.by_client.get(&key).copied().unwrap_or(0));
        if window.admitted >= KEYLESS_REGISTRATIONS_PER_WINDOW
            || from_client >= KEYLESS_REGISTRATIONS_PER_CLIENT
        {
            tracing::warn!(
                client_ip = ?client,
                counted_as = ?key,
                "refused: too many registrations without a pre-auth key"
            );
            let wait = WINDOW.saturating_sub(now.saturating_duration_since(window.start));
            return Err(AppError::RateLimited(
                "too many registrations without a pre-auth key; retry later, or register with \
                 a pre-auth key"
                    .into(),
                // A second past the window's end, so a retry at the wait lands in the next.
                Duration::from_secs(wait.as_secs() + 1),
            ));
        }
        window.admitted += 1;
        if let Some(key) = key {
            *window.by_client.entry(key).or_default() += 1;
        }
        Ok(())
    }
}

/// An IPv6 host picks its own address within a `/64`, so the `/64` is one client.
fn client_key(ip: IpAddr) -> IpAddr {
    match ip.to_canonical() {
        IpAddr::V6(v6) => IpAddr::V6(Ipv6Addr::from(u128::from(v6) & (u128::MAX << 64))),
        v4 @ IpAddr::V4(_) => v4,
    }
}

impl Default for RegistrationThrottle {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use axum::http::Request;
    use rstest::rstest;

    use super::*;

    /// A throttle and the start of its first window, so waits come out exact.
    fn throttle() -> (RegistrationThrottle, Instant) {
        let throttle = RegistrationThrottle::new();
        let start = throttle
            .window
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .start;
        (throttle, start)
    }

    fn ip(addr: &str) -> anyhow::Result<Option<IpAddr>> {
        Ok(Some(addr.parse()?))
    }

    #[test]
    fn a_full_window_refuses_until_the_next_one() -> anyhow::Result<()> {
        let (throttle, start) = throttle();
        (0..KEYLESS_REGISTRATIONS_PER_WINDOW).try_for_each(|_| throttle.admit_at(None, start))?;
        let refused = throttle.admit_at(None, start + WINDOW - Duration::from_secs(1));
        assert!(
            matches!(refused, Err(AppError::RateLimited(_, wait)) if wait == Duration::from_secs(2)),
            "{refused:?}"
        );
        throttle.admit_at(None, start + WINDOW)?;
        Ok(())
    }

    #[test]
    fn one_client_gets_only_its_share() -> anyhow::Result<()> {
        let (throttle, start) = throttle();
        let flood = ip("203.0.113.7")?;
        (0..KEYLESS_REGISTRATIONS_PER_CLIENT).try_for_each(|_| throttle.admit_at(flood, start))?;
        assert!(matches!(
            throttle.admit_at(flood, start),
            Err(AppError::RateLimited(_, wait)) if wait == WINDOW + Duration::from_secs(1)
        ));
        // A refusal is not counted against the budget the others share.
        (KEYLESS_REGISTRATIONS_PER_CLIENT..KEYLESS_REGISTRATIONS_PER_WINDOW)
            .try_for_each(|_| throttle.admit_at(None, start))?;
        assert!(throttle.admit_at(ip("198.51.100.1")?, start).is_err());
        throttle.admit_at(flood, start + WINDOW)?;
        Ok(())
    }

    #[test]
    fn a_v6_client_is_its_64() -> anyhow::Result<()> {
        let (throttle, start) = throttle();
        (0..KEYLESS_REGISTRATIONS_PER_CLIENT).try_for_each(|n| {
            anyhow::Ok(throttle.admit_at(ip(&format!("2001:db8::{n}"))?, start)?)
        })?;
        assert!(throttle.admit_at(ip("2001:db8::ffff")?, start).is_err());
        throttle.admit_at(ip("2001:db8:0:1::1")?, start)?;
        Ok(())
    }

    #[rstest]
    #[case::alb_appended(1, &["203.0.113.7"], Some("203.0.113.7"))]
    #[case::spoofed_left_of_alb(1, &["1.2.3.4, 203.0.113.7"], Some("203.0.113.7"))]
    #[case::last_of_several_headers(1, &["1.2.3.4", "2001:db8::1"], Some("2001:db8::1"))]
    #[case::behind_two_proxies(2, &["1.2.3.4, 203.0.113.7, 10.0.0.8"], Some("203.0.113.7"))]
    #[case::two_proxies_across_headers(2, &["203.0.113.7", "10.0.0.8"], Some("203.0.113.7"))]
    #[case::fewer_entries_than_hops(2, &["203.0.113.7"], None)]
    #[case::no_header(1, &[], None)]
    #[case::unparsable(1, &["unknown"], None)]
    #[case::no_proxy(0, &["1.2.3.4"], Some("10.0.0.9"))]
    fn the_client_is_the_entry_hops_from_the_right(
        #[case] hops: usize,
        #[case] forwarded: &[&str],
        #[case] expected: Option<&str>,
    ) -> anyhow::Result<()> {
        let request = forwarded
            .iter()
            .fold(Request::builder(), |req, value| {
                req.header("x-forwarded-for", *value)
            })
            .body(())?;
        let got = client_ip(request.headers(), ip("10.0.0.9")?, hops);
        assert_eq!(got, expected.map(str::parse).transpose()?);
        Ok(())
    }
}
