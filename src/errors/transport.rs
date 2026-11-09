use std::time::Duration;

use super::CliError;
use crate::env::{self, EnvSource};
use crate::response_sanitization::sanitize_untrusted_text;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum TransportFailure {
    Timeout,
    Refused,
    Dns,
    Reset,
    Tls,
    Unreachable,
}

/// Lowercase needles per failure, checked in order.
const FAILURES: &[(TransportFailure, &[&str])] = &[
    (
        TransportFailure::Timeout,
        &["timed out", "timeout", "deadline has elapsed"],
    ),
    (TransportFailure::Refused, &["connection refused"]),
    (
        TransportFailure::Dns,
        &[
            "dns error",
            "failed to lookup address",
            "name or service not known",
            "nodename nor servname",
            "no such host",
            "name resolution",
        ],
    ),
    (
        TransportFailure::Reset,
        &["connection reset", "broken pipe"],
    ),
    (TransportFailure::Tls, &["certificate", "tls", "ssl"]),
    (
        TransportFailure::Unreachable,
        &["network is unreachable", "no route to host"],
    ),
];

/// Needles for a failure that is certainly a transport problem but has no
/// more specific name. Only used on text, never on a `reqwest::Error` chain.
const GENERIC_CONNECTION_NEEDLES: &[&str] =
    &["error sending request", "connection", "service unavailable"];

impl TransportFailure {
    pub(super) fn detect(lower: &str) -> Option<Self> {
        FAILURES
            .iter()
            .find(|(_, needles)| needles.iter().any(|needle| lower.contains(needle)))
            .map(|(failure, _)| *failure)
    }

    fn describe(self, timeout: Option<Duration>) -> String {
        match self {
            Self::Timeout => match timeout {
                Some(limit) => format!("Timed out after {}", format_duration(limit)),
                None => "Timed out".to_string(),
            },
            Self::Refused => "Connection refused".to_string(),
            Self::Dns => "DNS lookup failed".to_string(),
            Self::Reset => "Connection reset".to_string(),
            Self::Tls => "TLS handshake failed".to_string(),
            Self::Unreachable => "Network is unreachable".to_string(),
        }
    }
}

/// Map locally generated error text that describes a transport failure to
/// `Couldn't reach the Hyperliquid API. <cause>.`
///
/// Text after `body=` is remote data and is never inspected.
pub(super) fn unreachable_from_text(text: &str) -> Option<CliError> {
    let lower = text.to_lowercase();
    let local = lower.split("body=").next().unwrap_or_default();
    let cause = match TransportFailure::detect(local) {
        Some(failure) => failure.describe(None),
        None if GENERIC_CONNECTION_NEEDLES
            .iter()
            .any(|needle| local.contains(needle)) =>
        {
            "Connection failed".to_string()
        }
        None => return None,
    };
    Some(unreachable("the Hyperliquid API", &cause))
}

fn unreachable(target: &str, cause: &str) -> CliError {
    CliError::Unavailable(format!("Couldn't reach {target}. {cause}."))
}

/// Map a `reqwest` transport failure to `Couldn't reach <host>. <cause>.`
///
/// The cause comes from walking the error's `source()` chain (refused,
/// timeout, DNS, TLS, reset). An active `HTTPS_PROXY`/`HTTP_PROXY` is named.
/// `timeout` is the request timeout that was in force, when known.
#[must_use]
pub fn network_error(err: &reqwest::Error, timeout: Option<Duration>) -> CliError {
    let target = err
        .url()
        .and_then(host_label)
        .unwrap_or_else(|| "the Hyperliquid API".to_string());
    let mut error = unreachable(&target, &transport_cause(err, timeout));
    if let (Some((name, host)), CliError::Unavailable(message)) =
        (active_proxy(err.url(), &EnvSource::Process), &mut error)
    {
        message.push_str(&format!(" The request used {name} ({host})."));
    }
    error
}

/// Describe why a transport request failed, without a trailing period.
#[must_use]
pub fn transport_cause(err: &reqwest::Error, timeout: Option<Duration>) -> String {
    let mut chain = vec![err.to_string().to_lowercase()];
    let mut deepest = None;
    let mut source = std::error::Error::source(err);
    while let Some(inner) = source {
        deepest = Some(inner.to_string());
        chain.push(inner.to_string().to_lowercase());
        source = inner.source();
    }
    let chain = chain.join(" | ");

    let failure = if err.is_timeout() {
        Some(TransportFailure::Timeout)
    } else {
        TransportFailure::detect(&chain)
    };
    match (failure, deepest) {
        (Some(failure), _) => failure.describe(timeout),
        (None, Some(text)) => sanitize_untrusted_text(&text)
            .trim_end_matches('.')
            .to_string(),
        (None, None) => "Connection failed".to_string(),
    }
}

fn format_duration(duration: Duration) -> String {
    if duration.subsec_millis() == 0 {
        format!("{}s", duration.as_secs())
    } else {
        format!("{}ms", duration.as_millis())
    }
}

fn host_label(url: &reqwest::Url) -> Option<String> {
    let host = url.host_str()?;
    Some(match url.port() {
        Some(port) => format!("{host}:{port}"),
        None => host.to_string(),
    })
}

/// The proxy variable that applies to this URL, with the proxy host only
/// (credentials in the proxy URL are never printed).
fn active_proxy(url: Option<&reqwest::Url>, source: &EnvSource) -> Option<(&'static str, String)> {
    let https = url.is_none_or(|url| url.scheme() == "https");
    let names: &[&'static str] = if https {
        &[
            env::HTTPS_PROXY,
            env::HTTPS_PROXY_LOWER,
            env::ALL_PROXY,
            env::ALL_PROXY_LOWER,
        ]
    } else {
        &[
            env::HTTP_PROXY,
            env::HTTP_PROXY_LOWER,
            env::ALL_PROXY,
            env::ALL_PROXY_LOWER,
        ]
    };
    for name in names {
        let Some(value) = source.get(name) else {
            continue;
        };
        let value = value.trim();
        if value.is_empty() {
            continue;
        }
        let parsed = reqwest::Url::parse(value)
            .ok()
            .filter(|url| url.host_str().is_some())
            .or_else(|| reqwest::Url::parse(&format!("http://{value}")).ok());
        let host = parsed
            .as_ref()
            .and_then(host_label)
            .unwrap_or_else(|| "a proxy".to_string());
        return Some((name, host));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_failures_name_their_cause_and_earlier_causes_win() {
        for (text, cause) in [
            ("operation timed out while connection refused", "Timed out"),
            ("connect: connection refused", "Connection refused"),
            ("dns error: failed to lookup address", "DNS lookup failed"),
            ("connection reset by peer", "Connection reset"),
            ("invalid peer certificate", "TLS handshake failed"),
            ("no route to host", "Network is unreachable"),
            ("error sending request", "Connection failed"),
        ] {
            let err = unreachable_from_text(text).unwrap_or_else(|| panic!("{text}"));
            assert_eq!(
                err.to_string(),
                format!("Couldn't reach the Hyperliquid API. {cause}."),
                "{text}"
            );
        }
        assert!(unreachable_from_text("something else").is_none());
    }

    #[test]
    fn remote_body_text_is_never_read_as_a_transport_failure() {
        assert!(unreachable_from_text("HTTP 418 body=connection timed out").is_none());
    }

    #[tokio::test]
    async fn refused_connection_is_named() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);
        let request_err = reqwest::Client::new()
            .get(format!("http://{addr}/info"))
            .send()
            .await
            .unwrap_err();
        assert_eq!(
            transport_cause(&request_err, Some(Duration::from_secs(10))),
            "Connection refused"
        );
    }

    #[test]
    fn the_proxy_named_is_the_one_the_source_sets_for_the_requests_scheme() {
        let https = reqwest::Url::parse("https://api.hyperliquid.xyz/info").unwrap();
        let http = reqwest::Url::parse("http://127.0.0.1:1/info").unwrap();
        let source = EnvSource::fixed([
            (env::HTTPS_PROXY_LOWER, "user:secret@proxy.example:3128"),
            (env::HTTP_PROXY, "http://plain.example:8080"),
            (env::ALL_PROXY, ""),
        ]);
        assert_eq!(
            active_proxy(Some(&https), &source),
            Some((env::HTTPS_PROXY_LOWER, "proxy.example:3128".to_string())),
            "credentials are never named"
        );
        assert_eq!(
            active_proxy(Some(&http), &source),
            Some((env::HTTP_PROXY, "plain.example:8080".to_string()))
        );
        let fallback = EnvSource::fixed([(env::ALL_PROXY_LOWER, "http://all.example:1")]);
        assert_eq!(
            active_proxy(Some(&https), &fallback).map(|(name, _)| name),
            Some(env::ALL_PROXY_LOWER)
        );
        assert_eq!(
            active_proxy(None, &EnvSource::fixed::<&str, &str>([])),
            None
        );
    }
}
