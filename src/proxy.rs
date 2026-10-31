//! The request handler: health check, redirect branch, negative cache, and
//! the streaming proxy to the upstream symbol store.

use crate::config::Config;
use crate::negative_cache::NegativeCache;
use crate::rewrite::{self, PathRewriter};
use bytes::Bytes;
use http_body::{Body, Frame, SizeHint};
use http_body_util::Full;
use hyper::body::Incoming;
use hyper::header::{self, HeaderMap, HeaderName, HeaderValue};
use hyper::{Request, Response, StatusCode, Uri, Version};
use hyper_rustls::HttpsConnector;
use hyper_util::client::legacy::Client;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::rt::{TokioExecutor, TokioTimer};
use std::convert::Infallible;
use std::error::Error as StdError;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, ready};
use std::time::Duration;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio::time::{Instant, Sleep};

pub type BoxError = Box<dyn StdError + Send + Sync>;

/// `Cache-Control` for "this symbol does not exist" answers. Kept short-ish
/// so symbols uploaded later (e.g. new releases) aren't hidden forever.
pub const MISSING_CACHE_CONTROL: &str = "public, max-age=3600";
/// Symbol files are immutable for a given debug-id path, so hits can be
/// cached aggressively by CDNs and clients.
pub const HIT_CACHE_CONTROL: &str = "public, max-age=604800, immutable";

/// How long an idle upstream connection is kept in the pool. Short enough to
/// stay under typical server-side keep-alive timeouts, so we rarely try to
/// reuse a connection the upstream is about to close.
const POOL_IDLE_TIMEOUT: Duration = Duration::from_secs(30);

/// How long a 2xx response waits for an upstream slot before being dropped
/// for a 503. Long enough to ride out a download finishing just as another
/// starts, short enough that a flood of rejected requests doesn't pile up
/// holding upstream responses.
const UPSTREAM_SLOT_WAIT: Duration = Duration::from_millis(100);
/// `Retry-After` (seconds) on the 503 sent when every upstream slot is busy.
const BUSY_RETRY_AFTER: &str = "5";
/// At most one "upstream slots exhausted" warning per interval.
const BUSY_WARN_INTERVAL: Duration = Duration::from_secs(10);

const CLOUDFLARE_CDN_CACHE_CONTROL: HeaderName =
    HeaderName::from_static("cloudflare-cdn-cache-control");
const X_ELECTRON_SYMBOL_REDIRECT: HeaderName =
    HeaderName::from_static("x-electron-symbol-redirect");

/// Connection-level headers that describe a single hop and must not be
/// forwarded in either direction (RFC 9110 section 7.6.1).
const HOP_BY_HOP: &[&str] = &[
    "connection",
    "keep-alive",
    "proxy-connection",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
];

type UpstreamClient = Client<HttpsConnector<HttpConnector>, Incoming>;

pub struct App {
    client: UpstreamClient,
    target_host: String,
    target_host_header: HeaderValue,
    rewriter: PathRewriter,
    missing: NegativeCache,
    upstream_timeout: Option<Duration>,
    /// One permit per in-flight 2xx response stream, held until the body
    /// has finished streaming (or the client goes away). Non-2xx responses
    /// (404s, 403s turned into 404s, errors) never take one.
    upstream_slots: Arc<Semaphore>,
    max_inflight_upstream: usize,
    busy_rejections: AtomicU64,
    busy_last_warn: Mutex<Option<Instant>>,
}

impl App {
    pub fn new(config: &Config) -> Result<Self, String> {
        let target_host_header = HeaderValue::from_str(&config.target_host)
            .map_err(|e| format!("TARGET_HOST is not a valid Host header: {e}"))?;
        let max_inflight_upstream = config.max_inflight_upstream.min(Semaphore::MAX_PERMITS);
        Ok(Self {
            client: build_client(config)?,
            target_host: config.target_host.clone(),
            target_host_header,
            rewriter: PathRewriter::new(config.path_prefix.clone()),
            missing: NegativeCache::default(),
            upstream_timeout: config.upstream_timeout,
            upstream_slots: Arc::new(Semaphore::new(max_inflight_upstream)),
            max_inflight_upstream,
            busy_rejections: AtomicU64::new(0),
            busy_last_warn: Mutex::new(None),
        })
    }

    pub async fn handle(
        self: Arc<Self>,
        req: Request<Incoming>,
    ) -> Result<Response<ResponseBody>, Infallible> {
        let target = req
            .uri()
            .path_and_query()
            .map(|p| p.as_str())
            .unwrap_or("/")
            .to_owned();

        let (pathname, search) = rewrite::whatwg_path_and_search(&target);
        if pathname == "/health" {
            return Ok(Response::new(ResponseBody::full("Alive")));
        }

        let cache_key = self.rewriter.rewrite(&format!("{pathname}{search}"));

        if wants_redirect(req.headers()) {
            let location = rewrite::redirect_location(&self.target_host, &cache_key);
            let Ok(location) = HeaderValue::from_str(&location) else {
                return Ok(self.error_response(&target, &"invalid redirect location", false));
            };
            // Only Cloudflare may cache these redirects: its cache key
            // (electron/infra cache ruleset) separates the redirect cohort on
            // both triggers of this branch, so a cached 302 cannot leak to
            // ordinary clients. Generic shared caches and browsers key on URL
            // alone, so they get no-store, while Cloudflare-CDN-Cache-Control
            // (preferred by Cloudflare over Cache-Control, and not forwarded
            // downstream) keeps the edge caching the redirect for an hour.
            let mut res = Response::new(ResponseBody::empty());
            *res.status_mut() = StatusCode::FOUND;
            let headers = res.headers_mut();
            headers.insert(header::LOCATION, location);
            headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
            headers.insert(
                CLOUDFLARE_CDN_CACHE_CONTROL,
                HeaderValue::from_static(MISSING_CACHE_CONTROL),
            );
            return Ok(res);
        }

        if self.missing.contains(&cache_key) {
            let mut res = Response::new(ResponseBody::empty());
            *res.status_mut() = StatusCode::NOT_FOUND;
            res.headers_mut().insert(
                header::CACHE_CONTROL,
                HeaderValue::from_static(MISSING_CACHE_CONTROL),
            );
            return Ok(res);
        }

        Ok(self.proxy(req, target).await)
    }

    async fn proxy(&self, req: Request<Incoming>, target: String) -> Response<ResponseBody> {
        let upstream_path = self.rewriter.rewrite(&rewrite::legacy_proxy_path(&target));
        let uri = match Uri::builder()
            .scheme("https")
            .authority(self.target_host.as_str())
            .path_and_query(upstream_path.as_str())
            .build()
        {
            Ok(uri) => uri,
            Err(e) => return self.error_response(&target, &e, false),
        };

        let (parts, body) = req.into_parts();
        let mut upstream_req = Request::new(body);
        *upstream_req.method_mut() = parts.method;
        *upstream_req.uri_mut() = uri;
        *upstream_req.version_mut() = Version::HTTP_11;
        let headers = upstream_req.headers_mut();
        *headers = parts.headers;
        strip_hop_by_hop(headers);
        headers.remove(header::EXPECT);
        // The upstream CDN picks the bucket from the Host header.
        headers.insert(header::HOST, self.target_host_header.clone());

        let fetch = self.client.request(upstream_req);
        let result = match self.upstream_timeout {
            Some(timeout) => match tokio::time::timeout(timeout, fetch).await {
                Ok(result) => result.map_err(UpstreamError::Client),
                Err(_) => Err(UpstreamError::Timeout),
            },
            None => fetch.await.map_err(UpstreamError::Client),
        };
        let upstream_res = match result {
            Ok(res) => res,
            Err(e) => {
                let is_timeout = e.is_timeout_like();
                return self.error_response(&target, &e, is_timeout);
            }
        };

        // Only 2xx bodies (the symbol files themselves, some hundreds of MB)
        // count against the cap; misses and errors are small and pass
        // through, so a burst of parallel lookups never turns 404s into
        // 503s. The permit moves into the response body below, so it is
        // released when the body finishes, fails, or is dropped because the
        // client disconnected or stopped reading for CLIENT_WRITE_TIMEOUT_MS
        // (see write_timeout.rs). On a 503, dropping `upstream_res` drops
        // its unread body, so hyper closes that upstream connection rather
        // than returning it to the pool.
        let permit = if upstream_res.status().is_success() {
            let slot = self.upstream_slots.clone().acquire_owned();
            match tokio::time::timeout(UPSTREAM_SLOT_WAIT, slot).await {
                Ok(Ok(permit)) => Some(permit),
                _ => return self.busy_response(&target),
            }
        } else {
            None
        };

        let (mut parts, body) = upstream_res.into_parts();
        strip_hop_by_hop(&mut parts.headers);
        // Upstream values win, as they did when http-proxy copied the
        // upstream headers over the ones set here.
        add_cors_headers(&mut parts.headers);

        if parts.status == StatusCode::FORBIDDEN {
            // The CDN returns 403 for objects that don't exist, but symsrv.dll
            // blacklists a server for the rest of the debugging session when
            // it sees a 403, so answer 404 instead and remember the miss.
            self.missing.insert(upstream_path);
            parts.status = StatusCode::NOT_FOUND;
            parts.headers.insert(
                header::CACHE_CONTROL,
                HeaderValue::from_static(MISSING_CACHE_CONTROL),
            );
        } else if parts.status == StatusCode::OK && lacks_cache_control(&parts.headers) {
            parts.headers.insert(
                header::CACHE_CONTROL,
                HeaderValue::from_static(HIT_CACHE_CONTROL),
            );
        }

        let body = UpstreamBody::new(body, self.upstream_timeout, target, permit);
        Response::from_parts(parts, ResponseBody::Upstream(body))
    }

    /// Every upstream slot is taken: shed the request instead of streaming
    /// yet another body, so a download flood can't push the pod past its
    /// memory limit.
    fn busy_response(&self, target: &str) -> Response<ResponseBody> {
        self.busy_rejections.fetch_add(1, Ordering::Relaxed);
        {
            let mut last = self
                .busy_last_warn
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            let now = Instant::now();
            if last.is_none_or(|t| now.duration_since(t) >= BUSY_WARN_INTERVAL) {
                *last = Some(now);
                let rejected = self.busy_rejections.swap(0, Ordering::Relaxed);
                eprintln!(
                    "Warning: all {} upstream slots busy (MAX_INFLIGHT_UPSTREAM); \
                     answered {rejected} request(s) with 503 since the last warning. \
                     Latest: {target}",
                    self.max_inflight_upstream
                );
            }
        }

        let mut res = Response::new(ResponseBody::full("Server busy, please retry shortly."));
        *res.status_mut() = StatusCode::SERVICE_UNAVAILABLE;
        let headers = res.headers_mut();
        headers.insert(header::CONTENT_TYPE, HeaderValue::from_static("text/plain"));
        headers.insert(
            header::RETRY_AFTER,
            HeaderValue::from_static(BUSY_RETRY_AFTER),
        );
        headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
        add_cors_headers(headers);
        res
    }

    fn error_response(
        &self,
        target: &str,
        err: &dyn std::fmt::Display,
        is_timeout: bool,
    ) -> Response<ResponseBody> {
        let error_id = uuid::Uuid::new_v4();
        eprintln!("Error: {error_id} Request: {target} {err}");

        // A stalled upstream answers 504; anything else is a generic 500.
        let status = if is_timeout {
            StatusCode::GATEWAY_TIMEOUT
        } else {
            StatusCode::INTERNAL_SERVER_ERROR
        };
        let mut res = Response::new(ResponseBody::full(format!(
            "Something went wrong. If this happens consistently please report to \
             https://github.com/electron/symbol-server with this error ID: \"{error_id}\""
        )));
        *res.status_mut() = status;
        let headers = res.headers_mut();
        headers.insert(header::CONTENT_TYPE, HeaderValue::from_static("text/plain"));
        // Transient upstream failures must never be cached by Cloudflare,
        // shared caches, or clients.
        headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
        add_cors_headers(headers);
        res
    }
}

fn build_client(config: &Config) -> Result<UpstreamClient, String> {
    let mut roots = rustls::RootCertStore {
        roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
    };
    if let Some(path) = &config.extra_ca_certs {
        use rustls::pki_types::CertificateDer;
        use rustls::pki_types::pem::PemObject;
        let certs = CertificateDer::pem_file_iter(path)
            .map_err(|e| format!("EXTRA_CA_CERTS {}: {e}", path.display()))?;
        for cert in certs {
            let cert = cert.map_err(|e| format!("EXTRA_CA_CERTS {}: {e}", path.display()))?;
            roots
                .add(cert)
                .map_err(|e| format!("EXTRA_CA_CERTS {}: {e}", path.display()))?;
        }
    }

    let tls = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .map_err(|e| e.to_string())?
    .with_root_certificates(roots)
    .with_no_client_auth();

    let mut http = HttpConnector::new();
    http.enforce_http(false);
    http.set_nodelay(true);
    // Set before connect, so it also bounds the advertised receive window.
    http.set_recv_buffer_size(config.upstream_recv_buffer);

    let https = hyper_rustls::HttpsConnectorBuilder::new()
        .with_tls_config(tls)
        .https_only()
        .enable_http1()
        .wrap_connector(http);

    Ok(Client::builder(TokioExecutor::new())
        .timer(TokioTimer::new())
        .pool_timer(TokioTimer::new())
        .pool_idle_timeout(POOL_IDLE_TIMEOUT)
        .pool_max_idle_per_host(config.pool_max_idle)
        .build(https))
}

/// The redirect branch serves Sentry's symbolicator and anything that opts
/// in with `x-electron-symbol-redirect: 1`.
fn wants_redirect(headers: &HeaderMap) -> bool {
    let is_symbolicator = headers
        .get(header::USER_AGENT)
        .is_some_and(|ua| ua.as_bytes().starts_with(b"symbolicator/"));
    // Node joined repeated headers with ", ", so only a single "1" matched.
    let mut opt_in = headers.get_all(X_ELECTRON_SYMBOL_REDIRECT).iter();
    let opted_in = opt_in.next().is_some_and(|v| v == "1") && opt_in.next().is_none();
    is_symbolicator || opted_in
}

fn strip_hop_by_hop(headers: &mut HeaderMap) {
    let listed: Vec<HeaderName> = headers
        .get_all(header::CONNECTION)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .filter_map(|name| HeaderName::from_bytes(name.trim().as_bytes()).ok())
        .collect();
    for name in HOP_BY_HOP {
        headers.remove(*name);
    }
    for name in listed {
        headers.remove(name);
    }
}

fn add_cors_headers(headers: &mut HeaderMap) {
    headers
        .entry(header::ACCESS_CONTROL_ALLOW_ORIGIN)
        .or_insert(HeaderValue::from_static("*"));
    headers
        .entry(header::ACCESS_CONTROL_ALLOW_METHODS)
        .or_insert(HeaderValue::from_static("GET"));
}

/// Node checked `!res.getHeader('cache-control')`, so a single empty value
/// counts as absent.
fn lacks_cache_control(headers: &HeaderMap) -> bool {
    let mut values = headers.get_all(header::CACHE_CONTROL).iter();
    match (values.next(), values.next()) {
        (None, _) => true,
        (Some(v), None) => v.is_empty(),
        _ => false,
    }
}

#[derive(Debug)]
enum UpstreamError {
    Timeout,
    Client(hyper_util::client::legacy::Error),
}

impl UpstreamError {
    /// Node answered 504 for ECONNRESET and ETIMEDOUT: our own timeout, a
    /// connect that timed out, or an upstream that dropped the connection
    /// before responding. Everything else (refused, DNS, TLS) was a 500.
    fn is_timeout_like(&self) -> bool {
        let UpstreamError::Client(err) = self else {
            return true;
        };
        let mut source: Option<&(dyn StdError + 'static)> = Some(err);
        while let Some(e) = source {
            if let Some(e) = e.downcast_ref::<hyper::Error>()
                && (e.is_incomplete_message() || e.is_timeout())
            {
                return true;
            }
            if let Some(e) = e.downcast_ref::<std::io::Error>() {
                use std::io::ErrorKind::*;
                if matches!(
                    e.kind(),
                    TimedOut | ConnectionReset | ConnectionAborted | UnexpectedEof
                ) {
                    return true;
                }
            }
            source = e.source();
        }
        false
    }
}

impl std::fmt::Display for UpstreamError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            UpstreamError::Timeout => f.write_str("upstream timed out before responding"),
            UpstreamError::Client(err) => {
                // Include the whole cause chain; the top-level legacy client
                // error alone is just "client error (Connect)".
                write!(f, "{err}")?;
                let mut source = err.source();
                while let Some(e) = source {
                    write!(f, ": {e}")?;
                    source = e.source();
                }
                Ok(())
            }
        }
    }
}

/// The body of every response this server sends.
pub enum ResponseBody {
    Full(Full<Bytes>),
    Upstream(UpstreamBody),
}

impl ResponseBody {
    fn full(data: impl Into<Bytes>) -> Self {
        ResponseBody::Full(Full::new(data.into()))
    }

    fn empty() -> Self {
        ResponseBody::full(Bytes::new())
    }
}

impl Body for ResponseBody {
    type Data = Bytes;
    type Error = BoxError;

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, BoxError>>> {
        match self.get_mut() {
            ResponseBody::Full(body) => Pin::new(body).poll_frame(cx).map_err(|e| match e {}),
            ResponseBody::Upstream(body) => Pin::new(body).poll_frame(cx),
        }
    }

    fn is_end_stream(&self) -> bool {
        match self {
            ResponseBody::Full(body) => body.is_end_stream(),
            ResponseBody::Upstream(body) => body.is_end_stream(),
        }
    }

    fn size_hint(&self) -> SizeHint {
        match self {
            ResponseBody::Full(body) => body.size_hint(),
            ResponseBody::Upstream(body) => body.size_hint(),
        }
    }
}

/// Streams the upstream body to the client, failing if the upstream goes
/// quiet for longer than the timeout. Flowing data resets the timer, so
/// large downloads that take longer than the timeout in total are fine.
///
/// Headers have already been sent when this fails, so no error status can
/// follow. Returning an error makes hyper tear down the client connection,
/// so the client sees an aborted transfer instead of hanging on a truncated
/// body.
pub struct UpstreamBody {
    inner: Incoming,
    timeout: Option<Duration>,
    idle: Option<Pin<Box<Sleep>>>,
    target: String,
    /// This stream's upstream slot (`None` for non-2xx responses, which
    /// don't count against the cap). Released as soon as the body ends or
    /// fails, and otherwise when hyper drops the body: the client
    /// disconnected, or stopped reading and hit CLIENT_WRITE_TIMEOUT_MS.
    /// The idle timer above can't catch a stalled client, because hyper
    /// stops polling the body while its writes are blocked.
    permit: Option<OwnedSemaphorePermit>,
}

impl UpstreamBody {
    fn new(
        inner: Incoming,
        timeout: Option<Duration>,
        target: String,
        permit: Option<OwnedSemaphorePermit>,
    ) -> Self {
        Self {
            inner,
            timeout,
            idle: timeout.map(|t| Box::pin(tokio::time::sleep(t))),
            target,
            permit,
        }
    }
}

#[derive(Debug)]
struct IdleTimeout;

impl std::fmt::Display for IdleTimeout {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("upstream stalled mid-response")
    }
}

impl StdError for IdleTimeout {}

impl Body for UpstreamBody {
    type Data = Bytes;
    type Error = BoxError;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, BoxError>>> {
        let this = &mut *self;
        match Pin::new(&mut this.inner).poll_frame(cx) {
            Poll::Ready(Some(Ok(frame))) => {
                if let (Some(idle), Some(timeout)) = (this.idle.as_mut(), this.timeout) {
                    idle.as_mut().reset(Instant::now() + timeout);
                }
                Poll::Ready(Some(Ok(frame)))
            }
            Poll::Ready(Some(Err(e))) => {
                eprintln!(
                    "Upstream aborted mid-response. Request: {} {e}",
                    this.target
                );
                this.permit = None;
                Poll::Ready(Some(Err(e.into())))
            }
            Poll::Ready(None) => {
                this.permit = None;
                Poll::Ready(None)
            }
            Poll::Pending => {
                if let Some(idle) = this.idle.as_mut() {
                    ready!(idle.as_mut().poll(cx));
                    eprintln!(
                        "Upstream aborted mid-response. Request: {} {IdleTimeout}",
                        this.target
                    );
                    this.permit = None;
                    return Poll::Ready(Some(Err(Box::new(IdleTimeout))));
                }
                Poll::Pending
            }
        }
    }

    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }

    fn size_hint(&self) -> SizeHint {
        self.inner.size_hint()
    }
}
