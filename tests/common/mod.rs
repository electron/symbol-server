//! Test helpers: a fake HTTPS upstream, a spawned symbol-server process, and
//! a minimal HTTP client. Ported from the Node suite's test/helpers.js.
#![allow(dead_code)]

use bytes::Bytes;
use http_body_util::combinators::BoxBody;
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper::header::HeaderMap;
use hyper::{Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use std::future::Future;
use std::io::Read;
use std::path::PathBuf;
use std::pin::Pin;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::net::{TcpListener, TcpStream};

const LISTEN_TIMEOUT: Duration = Duration::from_secs(5);
const LISTEN_POLL: Duration = Duration::from_millis(50);

pub type BoxError = Box<dyn std::error::Error + Send + Sync>;
pub type MockBody = BoxBody<Bytes, BoxError>;
pub type MockResponse = Response<MockBody>;
type HandlerFuture = Pin<Box<dyn Future<Output = MockResponse> + Send>>;
type Handler = Arc<dyn Fn(Request<Incoming>) -> HandlerFuture + Send + Sync>;

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join(name)
}

/// A response with the given status and body.
pub fn respond(status: u16, body: &'static str) -> MockResponse {
    respond_with(status, &[], body)
}

/// A response with the given status, headers and body.
pub fn respond_with(status: u16, headers: &[(&str, &str)], body: &'static str) -> MockResponse {
    let mut res = Response::new(full(body));
    *res.status_mut() = StatusCode::from_u16(status).unwrap();
    for (name, value) in headers {
        res.headers_mut().append(
            hyper::header::HeaderName::from_bytes(name.as_bytes()).expect("header name"),
            value.parse().expect("header value"),
        );
    }
    res
}

pub fn full(body: impl Into<Bytes>) -> MockBody {
    Full::new(body.into()).map_err(|e| match e {}).boxed()
}

/// A streaming body: the returned sender writes chunks; the body stays open
/// until the sender is dropped.
pub fn channel_body() -> (http_body_util::channel::Sender<Bytes, BoxError>, MockBody) {
    let (tx, body) = http_body_util::channel::Channel::<Bytes, BoxError>::new(8);
    (tx, body.boxed())
}

#[derive(Debug, Clone)]
pub struct RecordedRequest {
    pub url: String,
    pub method: String,
    pub headers: HeaderMap,
}

/// Fake upstream HTTPS server. Calls the handler for each request and
/// records the requests it received (and the TCP connections it accepted)
/// so tests can assert on them.
pub struct Upstream {
    pub host: String,
    pub port: u16,
    requests: Arc<Mutex<Vec<RecordedRequest>>>,
    connections: Arc<AtomicUsize>,
    task: tokio::task::JoinHandle<()>,
}

impl Upstream {
    pub fn requests(&self) -> Vec<RecordedRequest> {
        self.requests.lock().unwrap().clone()
    }

    pub fn request_count(&self) -> usize {
        self.requests.lock().unwrap().len()
    }

    /// TCP connections accepted so far.
    pub fn connection_count(&self) -> usize {
        self.connections.load(Ordering::SeqCst)
    }
}

impl Drop for Upstream {
    fn drop(&mut self) {
        self.task.abort();
    }
}

pub async fn start_upstream<F, Fut>(handler: F) -> Upstream
where
    F: Fn(Request<Incoming>) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = MockResponse> + Send + 'static,
{
    let handler: Handler = Arc::new(move |req| Box::pin(handler(req)));
    let tls = tls_acceptor();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let requests = Arc::new(Mutex::new(Vec::new()));
    let connections = Arc::new(AtomicUsize::new(0));

    let task = {
        let requests = requests.clone();
        let connections = connections.clone();
        tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    continue;
                };
                connections.fetch_add(1, Ordering::SeqCst);
                let tls = tls.clone();
                let handler = handler.clone();
                let requests = requests.clone();
                tokio::spawn(async move {
                    let Ok(stream) = tls.accept(stream).await else {
                        return;
                    };
                    let service = hyper::service::service_fn(move |req: Request<Incoming>| {
                        requests.lock().unwrap().push(RecordedRequest {
                            url: req.uri().to_string(),
                            method: req.method().to_string(),
                            headers: req.headers().clone(),
                        });
                        let fut = handler(req);
                        async move { Ok::<_, std::convert::Infallible>(fut.await) }
                    });
                    let _ = hyper::server::conn::http1::Builder::new()
                        .serve_connection(TokioIo::new(stream), service)
                        .await;
                });
            }
        })
    };

    Upstream {
        host: format!("127.0.0.1:{port}"),
        port,
        requests,
        connections,
        task,
    }
}

fn tls_acceptor() -> tokio_rustls::TlsAcceptor {
    use tokio_rustls::rustls::pki_types::pem::PemObject;
    use tokio_rustls::rustls::pki_types::{CertificateDer, PrivateKeyDer};

    let certs = CertificateDer::pem_file_iter(fixture("test-cert.pem"))
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    let key = PrivateKeyDer::from_pem_file(fixture("test-key.pem")).unwrap();
    let config = tokio_rustls::rustls::ServerConfig::builder_with_provider(Arc::new(
        tokio_rustls::rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .unwrap()
    .with_no_client_auth()
    .with_single_cert(certs, key)
    .unwrap();
    tokio_rustls::TlsAcceptor::from(Arc::new(config))
}

#[derive(Default)]
pub struct ServerOptions<'a> {
    pub target_host: Option<&'a str>,
    pub path_prefix: Option<&'a str>,
    pub extra_env: &'a [(&'a str, &'a str)],
}

/// A running symbol-server process, killed on drop.
pub struct SymbolServer {
    pub port: u16,
    child: Child,
    stderr: Arc<Mutex<Vec<u8>>>,
}

impl SymbolServer {
    pub fn stderr(&self) -> String {
        String::from_utf8_lossy(&self.stderr.lock().unwrap()).into_owned()
    }
}

impl Drop for SymbolServer {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

pub async fn start_symbol_server(opts: ServerOptions<'_>) -> Result<SymbolServer, String> {
    let port = free_port();
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_symbol-server"));
    for var in [
        "TARGET_HOST",
        "PATH_PREFIX",
        "UPSTREAM_TIMEOUT_MS",
        "UPSTREAM_POOL_MAX_IDLE",
        "EXTRA_CA_CERTS",
    ] {
        cmd.env_remove(var);
    }
    cmd.env("PORT", port.to_string())
        // The out-of-band way to trust the upstream's self-signed cert, as
        // NODE_EXTRA_CA_CERTS was for the Node server.
        .env("EXTRA_CA_CERTS", fixture("test-ca.pem"));
    if let Some(host) = opts.target_host {
        cmd.env("TARGET_HOST", host);
    }
    if let Some(prefix) = opts.path_prefix {
        cmd.env("PATH_PREFIX", prefix);
    }
    for (key, value) in opts.extra_env {
        cmd.env(key, value);
    }
    let mut child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("failed to spawn symbol-server: {e}"))?;

    let stderr = Arc::new(Mutex::new(Vec::new()));
    {
        let mut pipe = child.stderr.take().unwrap();
        let stderr = stderr.clone();
        std::thread::spawn(move || {
            let mut buf = [0u8; 4096];
            while let Ok(n) = pipe.read(&mut buf) {
                if n == 0 {
                    break;
                }
                stderr.lock().unwrap().extend_from_slice(&buf[..n]);
            }
        });
    }

    let mut server = SymbolServer {
        port,
        child,
        stderr,
    };
    let deadline = Instant::now() + LISTEN_TIMEOUT;
    loop {
        if let Some(status) = server.child.try_wait().unwrap() {
            // Give the reader thread a moment to drain the pipe.
            tokio::time::sleep(Duration::from_millis(50)).await;
            return Err(format!(
                "Symbol server exited with code {:?} before listening:\n{}",
                status.code(),
                server.stderr()
            ));
        }
        if TcpStream::connect(("127.0.0.1", port)).await.is_ok() {
            return Ok(server);
        }
        if Instant::now() > deadline {
            return Err(format!(
                "Port {port} did not open within {LISTEN_TIMEOUT:?}:\n{}",
                server.stderr()
            ));
        }
        tokio::time::sleep(LISTEN_POLL).await;
    }
}

/// Spawns an upstream + symbol-server pair.
pub async fn start_proxy<F, Fut>(
    handler: F,
    path_prefix: Option<&str>,
    extra_env: &[(&str, &str)],
) -> (SymbolServer, Upstream)
where
    F: Fn(Request<Incoming>) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = MockResponse> + Send + 'static,
{
    let upstream = start_upstream(handler).await;
    let server = start_symbol_server(ServerOptions {
        target_host: Some(&upstream.host),
        path_prefix,
        extra_env,
    })
    .await
    .unwrap();
    (server, upstream)
}

/// An upstream that answers every request with 200 "ok".
pub async fn ok_handler(_req: Request<Incoming>) -> MockResponse {
    respond(200, "ok")
}

pub struct TestResponse {
    pub status: u16,
    pub headers: HeaderMap,
    pub body: String,
}

impl TestResponse {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).map(|v| v.to_str().unwrap())
    }
}

/// How a streamed response ended.
pub enum BodyEnd {
    Complete,
    Aborted,
}

/// Sends a GET on a fresh connection and returns the head plus a body that
/// can be read incrementally.
pub async fn send(port: u16, path: &str, headers: &[(&str, &str)]) -> Response<Incoming> {
    let stream = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    let (mut sender, conn) = hyper::client::conn::http1::handshake(TokioIo::new(stream))
        .await
        .unwrap();
    tokio::spawn(conn);
    let mut req = Request::get(path).header("host", format!("127.0.0.1:{port}"));
    for (name, value) in headers {
        req = req.header(*name, *value);
    }
    sender
        .send_request(req.body(http_body_util::Empty::<Bytes>::new()).unwrap())
        .await
        .unwrap()
}

/// Reads a body to the end, reporting whether it completed or was cut off.
pub async fn read_body(mut body: Incoming) -> (String, BodyEnd) {
    let mut data = Vec::new();
    loop {
        match body.frame().await {
            Some(Ok(frame)) => {
                if let Ok(chunk) = frame.into_data() {
                    data.extend_from_slice(&chunk);
                }
            }
            Some(Err(_)) => return (String::from_utf8_lossy(&data).into(), BodyEnd::Aborted),
            None => return (String::from_utf8_lossy(&data).into(), BodyEnd::Complete),
        }
    }
}

pub async fn request(port: u16, path: &str, headers: &[(&str, &str)]) -> TestResponse {
    let res = send(port, path, headers).await;
    let status = res.status().as_u16();
    let (parts, body) = res.into_parts();
    let (body, end) = read_body(body).await;
    assert!(
        matches!(end, BodyEnd::Complete),
        "response body was cut off"
    );
    TestResponse {
        status,
        headers: parts.headers,
        body,
    }
}

/// Asserts the body matches /Something went wrong.*error ID: "[0-9a-f-]+"/i.
pub fn assert_error_body(body: &str) {
    let lower = body.to_lowercase();
    let start = lower
        .find("something went wrong")
        .unwrap_or_else(|| panic!("unexpected error body: {body}"));
    let rest = &lower[start..];
    let id_start = rest
        .find("error id: \"")
        .unwrap_or_else(|| panic!("no error ID in: {body}"))
        + "error id: \"".len();
    let id: String = rest[id_start..]
        .chars()
        .take_while(|c| c.is_ascii_hexdigit() || *c == '-')
        .collect();
    assert!(!id.is_empty(), "empty error ID in: {body}");
    assert!(
        rest[id_start + id.len()..].starts_with('"'),
        "unterminated error ID in: {body}"
    );
}
