//! Ported case for case from the Node suite's test/server.test.js, plus a few
//! new cases (marked "New:") for behavior the Node suite didn't pin down.

mod common;

use bytes::Bytes;
use common::*;
use std::time::{Duration, Instant};

const MISSING_CACHE_CONTROL: &str = "public, max-age=3600";
const HIT_CACHE_CONTROL: &str = "public, max-age=604800, immutable";

async fn server_for(target_host: &str) -> SymbolServer {
    start_symbol_server(ServerOptions {
        target_host: Some(target_host),
        ..Default::default()
    })
    .await
    .unwrap()
}

/// 'GET /health responds 200 with "Alive"'
#[tokio::test]
async fn health_responds_200_with_alive() {
    let server = server_for("127.0.0.1:1").await;

    let res = request(server.port, "/health", &[]).await;
    assert_eq!(res.status, 200);
    assert_eq!(res.body, "Alive");
}

/// 'GET /health is not affected by missing-symbol cache or rewrites'
#[tokio::test]
async fn health_is_not_affected_by_missing_symbol_cache_or_rewrites() {
    let server = start_symbol_server(ServerOptions {
        target_host: Some("127.0.0.1:1"),
        path_prefix: Some("/some/prefix"),
        ..Default::default()
    })
    .await
    .unwrap();

    let res = request(server.port, "/health", &[]).await;
    assert_eq!(res.status, 200);
    assert_eq!(res.body, "Alive");
}

/// 'symbolicator/* user-agent gets a 302 redirect'
#[tokio::test]
async fn symbolicator_user_agent_gets_a_302_redirect() {
    let server = server_for("symbols.example.test").await;

    let res = request(
        server.port,
        "/Foo/Bar",
        &[("user-agent", "symbolicator/1.2.3")],
    )
    .await;
    assert_eq!(res.status, 302);
    let location = res.header("location").unwrap();
    assert!(
        location.starts_with("https://symbols.example.test/"),
        "unexpected location: {location}"
    );
    assert_eq!(location, "https://symbols.example.test/foo/bar");
}

/// New: the second redirect trigger, `x-electron-symbol-redirect: 1`, gets
/// the same 302 and cache headers. (The Node suite only exercised it
/// indirectly through the path-rewrite tests.)
#[tokio::test]
async fn x_electron_symbol_redirect_header_gets_a_302_redirect() {
    let server = server_for("symbols.example.test").await;

    let res = request(
        server.port,
        "/Foo/Bar",
        &[("x-electron-symbol-redirect", "1")],
    )
    .await;
    assert_eq!(res.status, 302);
    assert_eq!(
        res.header("location"),
        Some("https://symbols.example.test/foo/bar")
    );
    assert_eq!(res.header("cache-control"), Some("no-store"));
    assert_eq!(
        res.header("cloudflare-cdn-cache-control"),
        Some(MISSING_CACHE_CONTROL)
    );
}

/// New: only the exact value "1" opts in to the redirect.
#[tokio::test]
async fn x_electron_symbol_redirect_other_values_are_proxied() {
    let (server, upstream) = start_proxy(ok_handler, None, &[]).await;

    for value in ["0", "true", "11"] {
        let res = request(
            server.port,
            "/foo/bar",
            &[("x-electron-symbol-redirect", value)],
        )
        .await;
        assert_eq!(res.status, 200, "x-electron-symbol-redirect: {value}");
        assert_eq!(res.body, "ok");
    }
    assert_eq!(upstream.request_count(), 3);
}

/// 'non-symbolicator user-agents do NOT get redirected'
#[tokio::test]
async fn non_symbolicator_user_agents_do_not_get_redirected() {
    let (server, _upstream) = start_proxy(
        |_| async { respond_with(200, &[("content-type", "text/plain")], "hello") },
        None,
        &[],
    )
    .await;

    let res = request(
        server.port,
        "/Foo/Bar",
        &[("user-agent", "Microsoft-Symbol-Server/10.0")],
    )
    .await;
    assert_eq!(res.status, 200);
    assert_eq!(res.body, "hello");
}

/// 'symbolicator UA without trailing slash is not treated as redirect'
#[tokio::test]
async fn symbolicator_ua_without_trailing_slash_is_not_treated_as_redirect() {
    // The check is `starts_with("symbolicator/")`; bare "symbolicator" (no
    // slash) should fall through to the proxy path.
    let (server, _upstream) = start_proxy(|_| async { respond(200, "proxied") }, None, &[]).await;

    let res = request(server.port, "/foo/bar", &[("user-agent", "symbolicator")]).await;
    assert_eq!(res.status, 200);
    assert_eq!(res.body, "proxied");
}

/// 'proxy forwards request to upstream with rewritten path'
#[tokio::test]
async fn proxy_forwards_request_to_upstream_with_rewritten_path() {
    let (server, upstream) = start_proxy(
        |_| async {
            respond_with(
                200,
                &[("content-type", "application/octet-stream")],
                "SYMBOL-DATA",
            )
        },
        None,
        &[],
    )
    .await;

    let res = request(server.port, "/Foo/Bar.PDB/ABC/foo.pdb", &[]).await;
    assert_eq!(res.status, 200);
    assert_eq!(res.body, "SYMBOL-DATA");

    let requests = upstream.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].url, "/foo/bar.pdb/abc/foo.pdb");
    assert_eq!(requests[0].headers["host"], upstream.host.as_str());
}

/// 'proxy applies PATH_PREFIX before forwarding'
#[tokio::test]
async fn proxy_applies_path_prefix_before_forwarding() {
    let (server, upstream) = start_proxy(ok_handler, Some("/release/symbols"), &[]).await;

    let res = request(server.port, "/Foo/Bar.PDB/ABC/foo.pdb", &[]).await;
    assert_eq!(res.status, 200);
    let requests = upstream.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].url, "/release/symbols/foo/bar.pdb/abc/foo.pdb");
}

/// 'proxy preserves and lowercases query strings'
#[tokio::test]
async fn proxy_preserves_and_lowercases_query_strings() {
    let (server, upstream) = start_proxy(ok_handler, None, &[]).await;

    let res = request(server.port, "/Foo/Bar.PDB?Baz=QUUX", &[]).await;
    assert_eq!(res.status, 200);
    let requests = upstream.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].url, "/foo/bar.pdb?baz=quux");
}

/// 'proxy applies app aliasing before forwarding'
#[tokio::test]
async fn proxy_applies_app_aliasing_before_forwarding() {
    let (server, upstream) = start_proxy(ok_handler, None, &[]).await;

    let res = request(server.port, "/slack/foo.pdb/ABC/file", &[]).await;
    assert_eq!(res.status, 200);
    let requests = upstream.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].url, "/electron/foo.pdb/abc/file");
}

/// 'upstream 403 is converted to 404 (and CORS headers set)'
#[tokio::test]
async fn upstream_403_is_converted_to_404_and_cors_headers_set() {
    let (server, _upstream) = start_proxy(
        |_| async { respond_with(403, &[("content-type", "text/plain")], "forbidden") },
        None,
        &[],
    )
    .await;

    let res = request(server.port, "/missing/foo.pdb/ABC/foo.pdb", &[]).await;
    assert_eq!(res.status, 404, "expected 403 to be rewritten to 404");
    assert_eq!(res.header("access-control-allow-origin"), Some("*"));
    assert_eq!(res.header("access-control-allow-methods"), Some("GET"));
}

/// 'subsequent requests for known-missing symbols are served from cache as 404'
#[tokio::test]
async fn subsequent_requests_for_known_missing_symbols_are_served_from_cache_as_404() {
    let (server, upstream) = start_proxy(|_| async { respond(403, "") }, None, &[]).await;

    let first = request(server.port, "/some/Path/abc/file.pdb", &[]).await;
    assert_eq!(first.status, 404);
    assert_eq!(upstream.request_count(), 1);

    let second = request(server.port, "/some/Path/abc/file.pdb", &[]).await;
    assert_eq!(second.status, 404);
    assert_eq!(
        upstream.request_count(),
        1,
        "cached miss should NOT contact upstream again"
    );

    let third = request(server.port, "/some/Other/abc/file.pdb", &[]).await;
    assert_eq!(third.status, 404);
    assert_eq!(upstream.request_count(), 2);
}

/// 'upstream non-403 errors are passed through and not cached as missing'
#[tokio::test]
async fn upstream_non_403_errors_are_passed_through_and_not_cached_as_missing() {
    let (server, upstream) = start_proxy(
        |_| async {
            // The first call fails, later ones succeed.
            static CALLS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
            if CALLS.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0 {
                respond(500, "")
            } else {
                respond(200, "data")
            }
        },
        None,
        &[],
    )
    .await;

    let first = request(server.port, "/some/path/abc/file.pdb", &[]).await;
    assert_eq!(first.status, 500);
    assert_eq!(upstream.request_count(), 1);

    // Same path should hit upstream again, not be served from the missing cache.
    let second = request(server.port, "/some/path/abc/file.pdb", &[]).await;
    assert_eq!(second.status, 200);
    assert_eq!(second.body, "data");
    assert_eq!(upstream.request_count(), 2);
}

/// New: pins current behavior. Only upstream 403s are negative-cached; an
/// upstream 404 is passed through untouched (no Cache-Control added) and the
/// next request goes upstream again.
#[tokio::test]
async fn upstream_404_is_passed_through_and_not_negative_cached() {
    let (server, upstream) = start_proxy(|_| async { respond(404, "not found") }, None, &[]).await;

    let first = request(server.port, "/missing/foo.pdb/abc/foo.pdb", &[]).await;
    assert_eq!(first.status, 404);
    assert_eq!(first.body, "not found");
    assert_eq!(first.header("cache-control"), None);

    let second = request(server.port, "/missing/foo.pdb/abc/foo.pdb", &[]).await;
    assert_eq!(second.status, 404);
    assert_eq!(upstream.request_count(), 2);
}

/// 'negative-cache 404s carry a public Cache-Control header'
#[tokio::test]
async fn negative_cache_404s_carry_a_public_cache_control_header() {
    let (server, upstream) = start_proxy(|_| async { respond(403, "") }, None, &[]).await;

    let first = request(server.port, "/missing/foo.pdb/abc/foo.pdb", &[]).await;
    assert_eq!(first.status, 404);
    assert_eq!(first.header("cache-control"), Some(MISSING_CACHE_CONTROL));

    // Served from the negative cache without contacting upstream.
    let second = request(server.port, "/missing/foo.pdb/abc/foo.pdb", &[]).await;
    assert_eq!(second.status, 404);
    assert_eq!(second.header("cache-control"), Some(MISSING_CACHE_CONTROL));
    assert_eq!(upstream.request_count(), 1);
}

/// 'successful 200s get a long immutable Cache-Control when upstream sends none'
#[tokio::test]
async fn successful_200s_get_a_long_immutable_cache_control_when_upstream_sends_none() {
    let (server, _upstream) =
        start_proxy(|_| async { respond(200, "SYMBOL-DATA") }, None, &[]).await;

    let res = request(server.port, "/foo/bar.pdb/abc/foo.pdb", &[]).await;
    assert_eq!(res.status, 200);
    assert_eq!(res.header("cache-control"), Some(HIT_CACHE_CONTROL));
}

/// 'upstream Cache-Control on 200s is preserved'
#[tokio::test]
async fn upstream_cache_control_on_200s_is_preserved() {
    let (server, _upstream) = start_proxy(
        |_| async {
            respond_with(
                200,
                &[("cache-control", "public, max-age=60")],
                "SYMBOL-DATA",
            )
        },
        None,
        &[],
    )
    .await;

    let res = request(server.port, "/foo/bar.pdb/abc/foo.pdb", &[]).await;
    assert_eq!(res.status, 200);
    assert_eq!(res.header("cache-control"), Some("public, max-age=60"));
}

/// 'redirect responses are cacheable by Cloudflare only'
#[tokio::test]
async fn redirect_responses_are_cacheable_by_cloudflare_only() {
    let server = server_for("symbols.example.test").await;

    let res = request(
        server.port,
        "/Foo/Bar",
        &[("user-agent", "symbolicator/1.2.3")],
    )
    .await;
    assert_eq!(res.status, 302);
    // Generic shared caches and browsers must never store the redirect...
    assert_eq!(res.header("cache-control"), Some("no-store"));
    // ...while Cloudflare (whose cache key separates the redirect cohort) may.
    assert_eq!(
        res.header("cloudflare-cdn-cache-control"),
        Some(MISSING_CACHE_CONTROL)
    );
}

/// 'proxy returns 500 with error ID when upstream is unreachable'
#[tokio::test]
async fn proxy_returns_500_with_error_id_when_upstream_is_unreachable() {
    let server = server_for("127.0.0.1:1").await;

    let res = request(server.port, "/foo/bar/abc/file.pdb", &[]).await;
    assert_eq!(res.status, 500);
    assert_eq!(res.header("content-type"), Some("text/plain"));
    assert_eq!(res.header("cache-control"), Some("no-store"));
    assert_error_body(&res.body);
}

/// 'a stalled upstream times out and returns an uncacheable 504'
#[tokio::test]
async fn a_stalled_upstream_times_out_and_returns_an_uncacheable_504() {
    let (server, _upstream) = start_proxy(
        // Accept the request, then never respond: no error, the socket just
        // sits idle (the H12 hang shape seen in production).
        |_| std::future::pending::<MockResponse>(),
        None,
        &[("UPSTREAM_TIMEOUT_MS", "500")],
    )
    .await;

    let started = Instant::now();
    let res = request(server.port, "/foo/bar.pdb/abc/foo.pdb", &[]).await;
    let elapsed = started.elapsed();

    assert_eq!(res.status, 504);
    assert_eq!(res.header("cache-control"), Some("no-store"));
    assert_error_body(&res.body);
    assert!(
        elapsed >= Duration::from_millis(400) && elapsed < Duration::from_secs(5),
        "expected the request to fail at the ~500ms timeout, took {elapsed:?}"
    );
}

/// New: UPSTREAM_TIMEOUT_MS=0 disables the timeout rather than meaning "time
/// out immediately", so a slow-to-respond upstream is still waited for.
#[tokio::test]
async fn upstream_timeout_ms_0_disables_the_timeout() {
    let (server, _upstream) = start_proxy(
        |_| async {
            tokio::time::sleep(Duration::from_millis(700)).await;
            respond(200, "slow")
        },
        None,
        &[("UPSTREAM_TIMEOUT_MS", "0")],
    )
    .await;

    let res = request(server.port, "/foo/bar.pdb/abc/foo.pdb", &[]).await;
    assert_eq!(res.status, 200);
    assert_eq!(res.body, "slow");
}

/// New: with the timeout disabled, a stalled upstream is not answered with a
/// 504 at all (the request stays pending).
#[tokio::test]
async fn upstream_timeout_ms_0_leaves_a_stalled_upstream_pending() {
    let (server, upstream) = start_proxy(
        |_| std::future::pending::<MockResponse>(),
        None,
        &[("UPSTREAM_TIMEOUT_MS", "0")],
    )
    .await;

    let pending = tokio::time::timeout(
        Duration::from_millis(1500),
        request(server.port, "/foo/bar.pdb/abc/foo.pdb", &[]),
    )
    .await;
    assert!(
        pending.is_err(),
        "expected no response with the timeout disabled"
    );
    assert_eq!(upstream.request_count(), 1);
}

/// 'slowly flowing responses are not killed by the inactivity timeout'
#[tokio::test]
async fn slowly_flowing_responses_are_not_killed_by_the_inactivity_timeout() {
    // The timeout is inactivity-based: each chunk resets it, so a download
    // that takes longer than UPSTREAM_TIMEOUT_MS in total still completes.
    let (server, _upstream) = start_proxy(
        |_| async {
            let (mut tx, body) = channel_body();
            tokio::spawn(async move {
                for _ in 0..4 {
                    tokio::time::sleep(Duration::from_millis(300)).await;
                    if tx.send_data(Bytes::from_static(b"chunk")).await.is_err() {
                        return;
                    }
                }
            });
            hyper::Response::new(body)
        },
        None,
        &[("UPSTREAM_TIMEOUT_MS", "500")],
    )
    .await;

    let res = request(server.port, "/foo/bar.pdb/abc/foo.pdb", &[]).await;
    assert_eq!(res.status, 200);
    assert_eq!(res.body, "chunk".repeat(4));
}

/// 'an upstream that stalls mid-body terminates the client connection'
#[tokio::test]
async fn an_upstream_that_stalls_mid_body_terminates_the_client_connection() {
    // Headers and part of the body have already been forwarded when the
    // upstream goes quiet, so a 504 can't follow; the client connection must
    // be torn down rather than left hanging on a truncated body.
    let (server, _upstream) = start_proxy(
        |_| async {
            let (mut tx, body) = channel_body();
            tokio::spawn(async move {
                let _ = tx.send_data(Bytes::from_static(b"partial")).await;
                // ...then stall forever, holding the body open.
                std::future::pending::<()>().await;
                drop(tx);
            });
            hyper::Response::new(body)
        },
        None,
        &[("UPSTREAM_TIMEOUT_MS", "500")],
    )
    .await;

    let started = Instant::now();
    let outcome = tokio::time::timeout(Duration::from_secs(5), async {
        let res = send(server.port, "/foo/bar.pdb/abc/foo.pdb", &[]).await;
        let status = res.status().as_u16();
        let (body, end) = read_body(res.into_body()).await;
        (status, body, end)
    })
    .await
    .expect("client response hung");
    let elapsed = started.elapsed();

    let (status, body, end) = outcome;
    assert!(
        matches!(end, BodyEnd::Aborted),
        "response ended cleanly; expected an aborted transfer"
    );
    assert_eq!(status, 200);
    assert_eq!(body, "partial");
    assert!(
        elapsed >= Duration::from_millis(400) && elapsed < Duration::from_secs(5),
        "expected the connection to be terminated at the ~500ms timeout, took {elapsed:?}"
    );
}

/// 'asserts when TARGET_HOST is missing'
#[tokio::test]
async fn asserts_when_target_host_is_missing() {
    let err = start_symbol_server(ServerOptions::default())
        .await
        .err()
        .expect("server should refuse to start");
    assert!(
        err.contains("exited with code") && err.contains("before listening"),
        "{err}"
    );
    assert!(err.contains("TARGET_HOST is defined"), "{err}");
}

/// New: upstream connections are pooled. Sequential unique misses (the
/// Cherry Servers flood shape) reuse keep-alive connections instead of
/// opening a new TCP+TLS connection each.
#[tokio::test]
async fn upstream_connections_are_reused_across_misses() {
    const N: usize = 20;
    let (server, upstream) = start_proxy(|_| async { respond(403, "") }, None, &[]).await;

    for i in 0..N {
        let res = request(server.port, &format!("/reuse/{i}/abc/file.pdb"), &[]).await;
        assert_eq!(res.status, 404);
    }
    assert_eq!(upstream.request_count(), N);
    let connections = upstream.connection_count();
    assert!(
        connections <= N / 4,
        "expected pooled connections, but {N} misses opened {connections} upstream connections"
    );
}

/// New: UPSTREAM_POOL_MAX_IDLE=0 turns pooling off (one connection per miss),
/// which also shows the reuse test above is measuring something real.
#[tokio::test]
async fn upstream_pool_max_idle_0_opens_a_connection_per_miss() {
    const N: usize = 5;
    let (server, upstream) = start_proxy(
        |_| async { respond(403, "") },
        None,
        &[("UPSTREAM_POOL_MAX_IDLE", "0")],
    )
    .await;

    for i in 0..N {
        let res = request(server.port, &format!("/nopool/{i}/abc/file.pdb"), &[]).await;
        assert_eq!(res.status, 404);
    }
    assert_eq!(upstream.request_count(), N);
    assert_eq!(upstream.connection_count(), N);
}
