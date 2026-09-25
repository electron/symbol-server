//! Runtime configuration, read from the environment.

use std::path::PathBuf;
use std::time::Duration;

/// Used when `UPSTREAM_TIMEOUT_MS` is unset, empty, or not a number.
pub const DEFAULT_UPSTREAM_TIMEOUT: Duration = Duration::from_secs(10);

/// Used when `UPSTREAM_POOL_MAX_IDLE` is unset or empty. Enough idle
/// keep-alive connections to absorb a burst of a few thousand misses per
/// second without reconnecting, while staying far below the dyno's fd limit.
pub const DEFAULT_POOL_MAX_IDLE: usize = 256;

/// Used when `PORT` is unset or empty.
pub const DEFAULT_PORT: u16 = 8080;

#[derive(Debug, Clone, PartialEq)]
pub struct Config {
    /// `PORT`: the port to listen on (all IPv4 interfaces).
    pub port: u16,
    /// `TARGET_HOST`: the upstream host (and optional port) symbols are
    /// fetched from over HTTPS. Required.
    pub target_host: String,
    /// `PATH_PREFIX`: prepended to every rewritten upstream path.
    pub path_prefix: String,
    /// `UPSTREAM_TIMEOUT_MS`: how long an upstream fetch may go without
    /// progress before it is abandoned. `None` (an explicit `0`) disables it.
    pub upstream_timeout: Option<Duration>,
    /// `UPSTREAM_POOL_MAX_IDLE`: how many idle keep-alive connections to the
    /// upstream are kept open for reuse. `0` opens a new connection per fetch.
    pub pool_max_idle: usize,
    /// `EXTRA_CA_CERTS`: optional PEM file of extra CA certificates to trust
    /// for the upstream, in addition to the bundled Mozilla roots. The Rust
    /// counterpart of `NODE_EXTRA_CA_CERTS`; used by the tests.
    pub extra_ca_certs: Option<PathBuf>,
}

impl Config {
    pub fn from_env() -> Result<Self, String> {
        Self::from_lookup(|key| std::env::var(key).ok())
    }

    pub fn from_lookup(get: impl Fn(&str) -> Option<String>) -> Result<Self, String> {
        let non_empty = |key: &str| get(key).filter(|v| !v.is_empty());

        let target_host = non_empty("TARGET_HOST")
            .ok_or_else(|| "AssertionError: TARGET_HOST is defined".to_string())?;
        target_host
            .parse::<hyper::http::uri::Authority>()
            .map_err(|e| format!("TARGET_HOST {target_host:?} is not a valid host[:port]: {e}"))?;

        let port = match non_empty("PORT") {
            None => DEFAULT_PORT,
            Some(v) => v
                .trim()
                .parse()
                .map_err(|_| format!("PORT {v:?} is not a valid port number"))?,
        };

        let pool_max_idle = match non_empty("UPSTREAM_POOL_MAX_IDLE") {
            None => DEFAULT_POOL_MAX_IDLE,
            Some(v) => v.trim().parse().map_err(|_| {
                format!("UPSTREAM_POOL_MAX_IDLE {v:?} is not a non-negative integer")
            })?,
        };

        Ok(Self {
            port,
            target_host,
            path_prefix: get("PATH_PREFIX").unwrap_or_default(),
            upstream_timeout: parse_upstream_timeout(get("UPSTREAM_TIMEOUT_MS").as_deref()),
            pool_max_idle,
            extra_ca_certs: non_empty("EXTRA_CA_CERTS").map(PathBuf::from),
        })
    }
}

/// Mirrors the Node server's handling of `UPSTREAM_TIMEOUT_MS`: an explicit
/// `0` disables the timeout; unset, empty, or `Number()`-NaN values fall back
/// to the 10s default. Negative and infinite values (which made the Node
/// server throw on every request) also fall back to the default.
pub fn parse_upstream_timeout(raw: Option<&str>) -> Option<Duration> {
    let Some(raw) = raw.filter(|v| !v.is_empty()) else {
        return Some(DEFAULT_UPSTREAM_TIMEOUT);
    };
    match js_number(raw) {
        Some(0.0) => None,
        Some(ms) if ms > 0.0 && ms.is_finite() => Some(Duration::from_secs_f64(ms / 1000.0)),
        _ => Some(DEFAULT_UPSTREAM_TIMEOUT),
    }
}

/// A close-enough port of JavaScript's `Number(string)`, returning `None`
/// where JS would return `NaN`.
fn js_number(raw: &str) -> Option<f64> {
    let s = raw.trim_matches(|c: char| c.is_whitespace() || c == '\u{feff}');
    if s.is_empty() {
        return Some(0.0);
    }
    for (prefix, radix) in [("0x", 16), ("0o", 8), ("0b", 2)] {
        if s.len() > 2 && s[..2].eq_ignore_ascii_case(prefix) {
            return u64::from_str_radix(&s[2..], radix).ok().map(|n| n as f64);
        }
    }
    match s {
        "Infinity" | "+Infinity" => return Some(f64::INFINITY),
        "-Infinity" => return Some(f64::NEG_INFINITY),
        _ => {}
    }
    // Rust also accepts "inf", "nan" and friends, which JS does not.
    if !s
        .bytes()
        .all(|b| b.is_ascii_digit() || matches!(b, b'+' | b'-' | b'.' | b'e' | b'E'))
    {
        return None;
    }
    s.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn config(vars: &[(&str, &str)]) -> Result<Config, String> {
        let vars: HashMap<String, String> = vars
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        Config::from_lookup(|k| vars.get(k).cloned())
    }

    #[test]
    fn upstream_timeout_defaults_to_10s_when_unset_or_empty() {
        assert_eq!(parse_upstream_timeout(None), Some(DEFAULT_UPSTREAM_TIMEOUT));
        assert_eq!(
            parse_upstream_timeout(Some("")),
            Some(DEFAULT_UPSTREAM_TIMEOUT)
        );
    }

    #[test]
    fn upstream_timeout_defaults_to_10s_when_not_a_number() {
        for raw in ["abc", "10s", "nan", "inf", "1e", "--5"] {
            assert_eq!(
                parse_upstream_timeout(Some(raw)),
                Some(DEFAULT_UPSTREAM_TIMEOUT),
                "{raw:?}"
            );
        }
    }

    #[test]
    fn upstream_timeout_zero_disables_it() {
        assert_eq!(parse_upstream_timeout(Some("0")), None);
        assert_eq!(parse_upstream_timeout(Some("0.0")), None);
        // Number("  ") is 0 in JS, so whitespace-only also disables it.
        assert_eq!(parse_upstream_timeout(Some("  ")), None);
    }

    #[test]
    fn upstream_timeout_parses_like_js_number() {
        let ms = |n: u64| Some(Duration::from_millis(n));
        assert_eq!(parse_upstream_timeout(Some("500")), ms(500));
        assert_eq!(parse_upstream_timeout(Some(" 250 ")), ms(250));
        assert_eq!(parse_upstream_timeout(Some("1e3")), ms(1000));
        assert_eq!(parse_upstream_timeout(Some("0x10")), ms(16));
        assert_eq!(parse_upstream_timeout(Some("+20")), ms(20));
    }

    #[test]
    fn upstream_timeout_negative_or_infinite_falls_back_to_default() {
        assert_eq!(
            parse_upstream_timeout(Some("-5")),
            Some(DEFAULT_UPSTREAM_TIMEOUT)
        );
        assert_eq!(
            parse_upstream_timeout(Some("Infinity")),
            Some(DEFAULT_UPSTREAM_TIMEOUT)
        );
    }

    #[test]
    fn target_host_is_required() {
        assert!(config(&[]).is_err());
        assert!(config(&[("TARGET_HOST", "")]).is_err());
        assert!(config(&[("TARGET_HOST", "not a host")]).is_err());
    }

    #[test]
    fn defaults() {
        let c = config(&[("TARGET_HOST", "symbols.example.test")]).unwrap();
        assert_eq!(
            c,
            Config {
                port: 8080,
                target_host: "symbols.example.test".into(),
                path_prefix: String::new(),
                upstream_timeout: Some(DEFAULT_UPSTREAM_TIMEOUT),
                pool_max_idle: DEFAULT_POOL_MAX_IDLE,
                extra_ca_certs: None,
            }
        );
        // PORT="" falls back like `process.env.PORT || 8080`.
        let c = config(&[("TARGET_HOST", "h"), ("PORT", "")]).unwrap();
        assert_eq!(c.port, 8080);
    }

    #[test]
    fn reads_all_variables() {
        let c = config(&[
            ("TARGET_HOST", "127.0.0.1:8443"),
            ("PORT", "5000"),
            ("PATH_PREFIX", "/release/symbols"),
            ("UPSTREAM_TIMEOUT_MS", "0"),
            ("UPSTREAM_POOL_MAX_IDLE", "0"),
            ("EXTRA_CA_CERTS", "/tmp/ca.pem"),
        ])
        .unwrap();
        assert_eq!(c.port, 5000);
        assert_eq!(c.target_host, "127.0.0.1:8443");
        assert_eq!(c.path_prefix, "/release/symbols");
        assert_eq!(c.upstream_timeout, None);
        assert_eq!(c.pool_max_idle, 0);
        assert_eq!(c.extra_ca_certs, Some(PathBuf::from("/tmp/ca.pem")));
    }

    #[test]
    fn rejects_invalid_numbers() {
        assert!(config(&[("TARGET_HOST", "h"), ("PORT", "http")]).is_err());
        assert!(config(&[("TARGET_HOST", "h"), ("UPSTREAM_POOL_MAX_IDLE", "-1")]).is_err());
    }
}
