//! A case-insensitive symbol server for Electron: a thin HTTP proxy in front
//! of the artifact store that fixes up symsrv.dll/symstore.exe path quirks,
//! turns the store's 403s into 404s, and keeps a pool of keep-alive
//! connections to the upstream.

pub mod config;
pub mod negative_cache;
pub mod proxy;
pub mod rewrite;

use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper_util::rt::{TokioIo, TokioTimer};
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;
use tokio::net::TcpListener;

pub use config::Config;

/// Binds `0.0.0.0:$PORT` and serves until the process is killed.
pub async fn run(config: Config) -> Result<(), String> {
    let app = Arc::new(proxy::App::new(&config)?);
    let addr = SocketAddr::from((Ipv4Addr::UNSPECIFIED, config.port));
    let listener = TcpListener::bind(addr)
        .await
        .map_err(|e| format!("failed to listen on {addr}: {e}"))?;
    eprintln!("symbol-server listening on {addr}");

    loop {
        let (stream, _) = match listener.accept().await {
            Ok(conn) => conn,
            Err(e) => {
                // Usually EMFILE; back off briefly instead of spinning.
                eprintln!("Error: accept failed: {e}");
                tokio::time::sleep(Duration::from_millis(50)).await;
                continue;
            }
        };
        let _ = stream.set_nodelay(true);
        let app = app.clone();
        tokio::spawn(async move {
            let service = service_fn(move |req| app.clone().handle(req));
            // Errors here are client-side (disconnects, malformed requests,
            // or the deliberate abort of a stalled upstream body).
            let _ = http1::Builder::new()
                .timer(TokioTimer::new())
                .serve_connection(TokioIo::new(stream), service)
                .await;
        });
    }
}
