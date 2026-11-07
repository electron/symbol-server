//! Ported case for case from the Node suite's test/server.test.js, plus a few
//! new cases (marked "New:") for behavior the Node suite didn't pin down.

mod common;

use bytes::Bytes;
use common::*;
use hyper::Method;
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

/// New: `UPSTREAM_HOST` takes the upstream fetches (URI, TLS SNI and Host
/// header) while redirected clients are still sent to `TARGET_HOST`.
#[tokio::test]
async fn upstream_host_takes_fetches_while_redirects_keep_target_host() {
    let upstream = start_upstream(ok_handler).await;
    // The fixture certificate's DNS name, so the SNI is sent (rustls sends
    // none for an IP address).
    let upstream_host = format!("localhost:{}", upstream.port);
    let server = start_symbol_server(ServerOptions {
        // Does not resolve: a fetch sent here would fail with a 500.
        target_host: Some("symbols.example.test"),
        path_prefix: Some("/symbols"),
        extra_env: &[("UPSTREAM_HOST", &upstream_host)],
    })
    .await
    .unwrap();

    let res = request(server.port, "/Foo.PDB/ABC/foo.pdb", &[]).await;
    assert_eq!(res.status, 200);
    assert_eq!(res.body, "ok");
    let requests = upstream.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].url, "/symbols/foo.pdb/abc/foo.pdb");
    assert_eq!(requests[0].headers["host"], upstream_host.as_str());
    assert_eq!(requests[0].sni.as_deref(), Some("localhost"));

    for headers in [
        &[("x-electron-symbol-redirect", "1")][..],
        &[("user-agent", "symbolicator/1.2.3")][..],
    ] {
        let res = request(server.port, "/Foo.PDB/ABC/foo.pdb", headers).await;
        assert_eq!(res.status, 302);
        assert_eq!(
            res.header("location"),
            Some("https://symbols.example.test/symbols/foo.pdb/abc/foo.pdb")
        );
    }
    assert_eq!(upstream.request_count(), 1);
}

/// New: without `UPSTREAM_HOST`, fetches go to `TARGET_HOST` (with it as
/// the TLS SNI and Host header), as they always did.
#[tokio::test]
async fn upstream_host_defaults_to_target_host() {
    let upstream = start_upstream(ok_handler).await;
    let target_host = format!("localhost:{}", upstream.port);
    let server = server_for(&target_host).await;

    let res = request(server.port, "/foo/bar", &[]).await;
    assert_eq!(res.status, 200);
    let requests = upstream.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].headers["host"], target_host.as_str());
    assert_eq!(requests[0].sni.as_deref(), Some("localhost"));

    let res = request(
        server.port,
        "/foo/bar",
        &[("x-electron-symbol-redirect", "1")],
    )
    .await;
    assert_eq!(
        res.header("location"),
        Some(format!("https://{target_host}/foo/bar").as_str())
    );
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

/// New: only GET and HEAD are served. Anything else gets an uncacheable 405
/// without contacting upstream (whose egress proxy would refuse it with a
/// 403), and leaves nothing in the negative cache.
#[tokio::test]
async fn methods_other_than_get_and_head_get_405_without_contacting_upstream() {
    let (server, upstream) = start_proxy(ok_handler, None, &[]).await;

    for method in ["POST", "PUT", "DELETE", "PATCH", "OPTIONS"] {
        let method = Method::from_bytes(method.as_bytes()).unwrap();
        let res =
            request_method(server.port, method.clone(), "/foo/bar.pdb/abc/foo.pdb", &[]).await;
        assert_eq!(res.status, 405, "{method}");
        assert_eq!(res.header("allow"), Some("GET, HEAD"), "{method}");
        assert_eq!(res.header("cache-control"), Some("no-store"), "{method}");
        assert_eq!(
            res.header("access-control-allow-origin"),
            Some("*"),
            "{method}"
        );
    }
    // Not even on the redirect branch.
    let res = request_method(
        server.port,
        Method::POST,
        "/foo/bar.pdb/abc/foo.pdb",
        &[("x-electron-symbol-redirect", "1")],
    )
    .await;
    assert_eq!(res.status, 405);
    assert_eq!(upstream.request_count(), 0);

    let res = request(server.port, "/foo/bar.pdb/abc/foo.pdb", &[]).await;
    assert_eq!(res.status, 200);
    assert_eq!(res.body, "ok");
    assert_eq!(upstream.request_count(), 1);
}

/// New: an upstream path with an escaped slash or backslash (`%2F`/`%5C`,
/// either case) is answered with the usual miss 404 without contacting
/// upstream, whose egress proxy would reject it with a 400. Debuggers send
/// these when a Windows PDB path is escaped into the URL.
#[tokio::test]
async fn escaped_separators_in_the_path_are_misses_without_contacting_upstream() {
    let (server, upstream) = start_proxy(ok_handler, None, &[]).await;

    for path in [
        "/symbols/c:%5cactions-runner%5c_work%5celectron.exe.pdb/4dcfd43e1/c:%5celectron.exe.pdb",
        "/foo/bar.pdb%5Cabc/foo.pdb",
        "/foo%2Fbar.pdb/abc/foo.pdb",
        "/foo%2fbar.pdb/abc/foo.pdb",
    ] {
        let res = request(server.port, path, &[]).await;
        assert_eq!(res.status, 404, "{path}");
        assert_eq!(res.body, "", "{path}");
        assert_eq!(
            res.header("cache-control"),
            Some(MISSING_CACHE_CONTROL),
            "{path}"
        );
        assert_eq!(
            res.header("access-control-allow-origin"),
            Some("*"),
            "{path}"
        );
        let res = request_method(server.port, Method::HEAD, path, &[]).await;
        assert_eq!(res.status, 404, "HEAD {path}");
    }
    assert_eq!(upstream.request_count(), 0);

    // Other escapes, %2F/%5C in the query string, and the escaped
    // c:\projects\... prefix the rewriter strips all still go upstream.
    let res = request(server.port, "/foo/bar%20baz.pdb/abc/foo.pdb?x=%2F", &[]).await;
    assert_eq!(res.status, 200);
    assert_eq!(res.body, "ok");
    let res = request(
        server.port,
        "/c%3A%5Cprojects%5Csrc%5Cout%5Cdefault%5Cfoo.pdb/abc/foo.pdb",
        &[],
    )
    .await;
    assert_eq!(res.status, 200);
    let res = request(server.port, "/foo/bar.pdb/abc/foo.pdb", &[]).await;
    assert_eq!(res.status, 200);
    let requests = upstream.requests();
    assert_eq!(requests.len(), 3);
    assert_eq!(requests[1].url, "/foo.pdb/abc/foo.pdb");
}

/// New: HEAD is forwarded upstream as a HEAD.
#[tokio::test]
async fn head_is_forwarded_upstream() {
    let (server, upstream) = start_proxy(ok_handler, None, &[]).await;

    let res = request_method(server.port, Method::HEAD, "/Foo/Bar.PDB/ABC/foo.pdb", &[]).await;
    assert_eq!(res.status, 200);
    assert_eq!(res.body, "");
    assert_eq!(res.header("cache-control"), Some(HIT_CACHE_CONTROL));
    let requests = upstream.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].method, "HEAD");
    assert_eq!(requests[0].url, "/foo/bar.pdb/abc/foo.pdb");
}

/// New: a refusal from the egress proxy (marked `x-egress-proxy: deny`) is
/// not the CDN's "no such object" 403. It becomes an uncacheable 502 with an
/// error ID, is logged, and is not negative-cached, so the symbol is fetched
/// again once the allowlist is fixed.
#[tokio::test]
async fn egress_proxy_denials_are_uncached_502s_not_misses() {
    let (server, upstream) = start_proxy(
        |_| async {
            static CALLS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
            match CALLS.fetch_add(1, std::sync::atomic::Ordering::SeqCst) {
                0 => respond_with(403, &[("x-egress-proxy", "deny")], "egress denied\n"),
                1 => respond_with(400, &[("x-egress-proxy", "deny")], ""),
                _ => respond(200, "SYMBOL-DATA"),
            }
        },
        None,
        &[],
    )
    .await;

    for (i, method) in [Method::GET, Method::HEAD].into_iter().enumerate() {
        let res =
            request_method(server.port, method.clone(), "/foo/bar.pdb/abc/foo.pdb", &[]).await;
        assert_eq!(res.status, 502, "{method}");
        assert_eq!(res.header("cache-control"), Some("no-store"), "{method}");
        assert_eq!(res.header("x-egress-proxy"), None, "{method}");
        if method == Method::GET {
            assert_error_body(&res.body);
        }
        assert_eq!(upstream.request_count(), i + 1);
    }

    let res = request(server.port, "/foo/bar.pdb/abc/foo.pdb", &[]).await;
    assert_eq!(
        res.status, 200,
        "a proxy denial must not be negative-cached"
    );
    assert_eq!(res.body, "SYMBOL-DATA");
    assert_eq!(upstream.request_count(), 3);

    let stderr = server.stderr();
    assert!(
        stderr.contains("egress proxy refused GET /foo/bar.pdb/abc/foo.pdb with 403 Forbidden"),
        "{stderr}"
    );
    assert!(
        stderr.contains("egress proxy refused HEAD /foo/bar.pdb/abc/foo.pdb with 400 Bad Request"),
        "{stderr}"
    );
}

/// New: only the proxy's exact marker counts; a CDN 403 carrying some other
/// x-egress-proxy value is still a miss.
#[tokio::test]
async fn upstream_403_without_the_deny_marker_is_still_a_miss() {
    let (server, upstream) = start_proxy(
        |_| async { respond_with(403, &[("x-egress-proxy", "allow")], "") },
        None,
        &[],
    )
    .await;

    let res = request(server.port, "/missing/foo.pdb/abc/foo.pdb", &[]).await;
    assert_eq!(res.status, 404);
    assert_eq!(res.header("cache-control"), Some(MISSING_CACHE_CONTROL));
    let res = request(server.port, "/missing/foo.pdb/abc/foo.pdb", &[]).await;
    assert_eq!(res.status, 404);
    assert_eq!(upstream.request_count(), 1);
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

/// Upstream for the concurrency-cap tests. `/held/...` sends one chunk and
/// then holds the body open forever; `/finite/...` sends two chunks 500ms
/// apart and ends; `/endless/...` keeps sending chunks until the proxy goes
/// away; `/big/...` is a BIG_BODY_LEN body sent in one go; `/missing/...`
/// is a 404 and `/forbidden/...` a 403; anything else is a quick 200 "ok".
async fn streaming_handler(req: hyper::Request<hyper::body::Incoming>) -> MockResponse {
    let path = req.uri().path().to_owned();
    let kind = path.split('/').nth(1).unwrap_or_default().to_owned();
    if kind == "big" {
        return hyper::Response::new(full(vec![b'x'; BIG_BODY_LEN]));
    }
    if kind == "missing" {
        return respond(404, "not found");
    }
    if kind == "forbidden" {
        return respond(403, "");
    }
    if !matches!(kind.as_str(), "held" | "finite" | "endless") {
        return respond(200, "ok");
    }
    let (mut tx, body) = channel_body();
    tokio::spawn(async move {
        match kind.as_str() {
            "held" => {
                let _ = tx.send_data(Bytes::from_static(b"partial")).await;
                std::future::pending::<()>().await;
            }
            "finite" => {
                let _ = tx.send_data(Bytes::from_static(b"one")).await;
                tokio::time::sleep(Duration::from_millis(500)).await;
                let _ = tx.send_data(Bytes::from_static(b"two")).await;
            }
            _ => loop {
                if tx
                    .send_data(Bytes::from(vec![b'x'; 16 * 1024]))
                    .await
                    .is_err()
                {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            },
        }
    });
    hyper::Response::new(body)
}

const BIG_BODY_LEN: usize = 256 * 1024;

/// A raw client connection with a small receive buffer, so the proxy's
/// writes to it block quickly once it stops reading.
async fn small_buffer_client(port: u16, path: &str) -> tokio::net::TcpStream {
    use tokio::io::AsyncWriteExt;

    let socket = tokio::net::TcpSocket::new_v4().unwrap();
    socket.set_recv_buffer_size(4096).unwrap();
    let mut client = socket.connect(([127, 0, 0, 1], port).into()).await.unwrap();
    client
        .write_all(
            format!("GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
                .as_bytes(),
        )
        .await
        .unwrap();
    client
}

fn assert_busy(res: &TestResponse) {
    assert_eq!(res.status, 503, "expected a 503, got body {:?}", res.body);
    assert_eq!(res.header("retry-after"), Some("5"));
    assert_eq!(res.header("cache-control"), Some("no-store"));
}

/// New: with MAX_INFLIGHT_UPSTREAM=N and N 200 bodies still streaming, a
/// further 200 from the upstream is dropped and the client gets an
/// uncacheable 503 + Retry-After, and a burst of rejections logs a single
/// warning.
#[tokio::test]
async fn requests_beyond_max_inflight_upstream_get_503() {
    const N: usize = 3;
    let (server, upstream) =
        start_proxy(streaming_handler, None, &[("MAX_INFLIGHT_UPSTREAM", "3")]).await;

    let mut held = Vec::new();
    for i in 0..N {
        let res = send(server.port, &format!("/held/{i}/abc/file.pdb"), &[]).await;
        assert_eq!(res.status(), 200);
        held.push(res);
    }

    for i in 0..3 {
        let res = request(server.port, &format!("/fast/{i}/abc/file.pdb"), &[]).await;
        assert_busy(&res);
        assert_eq!(res.header("access-control-allow-origin"), Some("*"));
    }
    // The permit is only taken once the upstream has answered 2xx.
    assert_eq!(upstream.request_count(), N + 3);

    // Give the stderr reader thread a moment to catch up.
    tokio::time::sleep(Duration::from_millis(100)).await;
    let stderr = server.stderr();
    assert_eq!(
        stderr.matches("upstream slots busy").count(),
        1,
        "expected one rate-limited warning, got:\n{stderr}"
    );
    assert!(stderr.contains("all 3 upstream slots busy"), "{stderr}");
    drop(held);
}

/// New: /health and the redirect branch never take an upstream slot, so the
/// probes and symbolicator keep working while the cap is saturated.
#[tokio::test]
async fn health_and_redirects_answer_while_max_inflight_upstream_is_saturated() {
    let (server, upstream) =
        start_proxy(streaming_handler, None, &[("MAX_INFLIGHT_UPSTREAM", "1")]).await;

    let held = send(server.port, "/held/0/abc/file.pdb", &[]).await;
    assert_eq!(held.status(), 200);
    assert_busy(&request(server.port, "/fast/0/abc/file.pdb", &[]).await);

    let health = request(server.port, "/health", &[]).await;
    assert_eq!(health.status, 200);
    assert_eq!(health.body, "Alive");

    let redirect = request(
        server.port,
        "/Foo/Bar",
        &[("user-agent", "symbolicator/1.2.3")],
    )
    .await;
    assert_eq!(redirect.status, 302);
    // The held stream and the shed /fast/ request; nothing for /health or
    // the redirect.
    assert_eq!(upstream.request_count(), 2);
    drop(held);
}

/// New: misses and errors never count against MAX_INFLIGHT_UPSTREAM, so a
/// burst of parallel lookups for missing symbols still gets its 404s while
/// the cap is saturated by downloads.
#[tokio::test]
async fn non_2xx_responses_pass_through_while_max_inflight_upstream_is_saturated() {
    let (server, upstream) =
        start_proxy(streaming_handler, None, &[("MAX_INFLIGHT_UPSTREAM", "1")]).await;

    let held = send(server.port, "/held/0/abc/file.pdb", &[]).await;
    assert_eq!(held.status(), 200);
    assert_busy(&request(server.port, "/fast/0/abc/file.pdb", &[]).await);

    for i in 0..5 {
        let res = request(server.port, &format!("/missing/{i}/abc/file.pdb"), &[]).await;
        assert_eq!(res.status, 404);
        assert_eq!(res.body, "not found");
    }
    // An upstream 403 is still turned into a negative-cached 404.
    let res = request(server.port, "/forbidden/0/abc/file.pdb", &[]).await;
    assert_eq!(res.status, 404);
    assert_eq!(res.header("cache-control"), Some(MISSING_CACHE_CONTROL));
    assert_eq!(upstream.request_count(), 8);
    drop(held);
}

/// New: the upstream slot is held until the body has finished streaming, and
/// released once it has.
#[tokio::test]
async fn upstream_slot_is_released_when_the_body_completes() {
    let (server, _upstream) =
        start_proxy(streaming_handler, None, &[("MAX_INFLIGHT_UPSTREAM", "1")]).await;

    let first = send(server.port, "/finite/0/abc/file.pdb", &[]).await;
    assert_eq!(first.status(), 200);
    // Headers are out but the body is still streaming: the slot is taken.
    assert_busy(&request(server.port, "/fast/0/abc/file.pdb", &[]).await);

    let (body, end) = read_body(first.into_body()).await;
    assert!(matches!(end, BodyEnd::Complete));
    assert_eq!(body, "onetwo");

    let res = request(server.port, "/fast/1/abc/file.pdb", &[]).await;
    assert_eq!(res.status, 200);
    assert_eq!(res.body, "ok");
}

/// New: a client that disconnects mid-body releases its upstream slot (the
/// incident's shape: a client abandoning a large download partway through).
#[tokio::test]
async fn upstream_slot_is_released_when_the_client_disconnects_mid_body() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let (server, _upstream) =
        start_proxy(streaming_handler, None, &[("MAX_INFLIGHT_UPSTREAM", "1")]).await;

    let mut client = tokio::net::TcpStream::connect(("127.0.0.1", server.port))
        .await
        .unwrap();
    client
        .write_all(b"GET /endless/0/abc/file.pdb HTTP/1.1\r\nHost: localhost\r\n\r\n")
        .await
        .unwrap();
    let mut buf = vec![0u8; 64 * 1024];
    let mut received = Vec::new();
    while received.len() < 32 * 1024 {
        let n = client.read(&mut buf).await.unwrap();
        assert!(n > 0, "server closed the connection early");
        received.extend_from_slice(&buf[..n]);
    }
    assert!(received.starts_with(b"HTTP/1.1 200"));
    assert_busy(&request(server.port, "/fast/0/abc/file.pdb", &[]).await);

    // Hang up with the body still flowing.
    drop(client);

    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let res = request(server.port, "/fast/1/abc/file.pdb", &[]).await;
        if res.status == 200 {
            assert_eq!(res.body, "ok");
            break;
        }
        assert_busy(&res);
        assert!(
            Instant::now() < deadline,
            "upstream slot was not released after the client disconnected"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// New: a client that reads the headers and then stops reading, without
/// disconnecting, loses its upstream slot once writes to it have been
/// blocked for CLIENT_WRITE_TIMEOUT_MS. hyper stops polling the body while
/// the write is blocked, so nothing else would ever release it.
#[tokio::test]
async fn upstream_slot_is_released_when_the_client_stops_reading() {
    use tokio::io::AsyncReadExt;

    let (server, _upstream) = start_proxy(
        streaming_handler,
        None,
        &[
            ("MAX_INFLIGHT_UPSTREAM", "1"),
            ("CLIENT_SEND_BUFFER_BYTES", "4096"),
            ("CLIENT_WRITE_TIMEOUT_MS", "500"),
        ],
    )
    .await;

    let mut client = small_buffer_client(server.port, "/endless/0/abc/file.pdb").await;
    let mut head = [0u8; 12];
    client.read_exact(&mut head).await.unwrap();
    assert_eq!(&head, b"HTTP/1.1 200");
    // From here on the client never reads again but keeps the connection
    // open, so the proxy's writes to it soon block for good.
    assert_busy(&request(server.port, "/fast/0/abc/file.pdb", &[]).await);

    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let res = request(server.port, "/fast/1/abc/file.pdb", &[]).await;
        if res.status == 200 {
            assert_eq!(res.body, "ok");
            break;
        }
        assert_busy(&res);
        assert!(
            Instant::now() < deadline,
            "upstream slot was not released after the client stopped reading"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    drop(client);
}

/// New: the write timeout restarts on every completed write rather than
/// bounding the whole download, so a client that keeps reading comfortably
/// above the minimum drain rate (about a third of the doubled 4 KiB send
/// buffer per 300ms here) gets the whole body even though the download
/// takes several times CLIENT_WRITE_TIMEOUT_MS. Clients below that rate
/// are dropped; this test doesn't cover them.
#[tokio::test]
async fn a_client_reading_above_the_minimum_rate_outlasts_the_write_timeout() {
    use tokio::io::AsyncReadExt;

    let (server, _upstream) = start_proxy(
        streaming_handler,
        None,
        &[
            ("CLIENT_SEND_BUFFER_BYTES", "4096"),
            ("CLIENT_WRITE_TIMEOUT_MS", "300"),
        ],
    )
    .await;

    let started = Instant::now();
    let mut client = small_buffer_client(server.port, "/big/0/abc/file.pdb").await;
    let mut received = Vec::new();
    let mut buf = [0u8; 16 * 1024];
    loop {
        let n = client.read(&mut buf).await.unwrap();
        if n == 0 {
            break;
        }
        received.extend_from_slice(&buf[..n]);
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let elapsed = started.elapsed();

    let head_end = received
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .expect("no response head")
        + 4;
    assert!(received.starts_with(b"HTTP/1.1 200"));
    assert_eq!(received.len() - head_end, BIG_BODY_LEN, "body was cut off");
    assert!(
        elapsed > Duration::from_millis(600),
        "the download was too fast to exercise the timeout ({elapsed:?})"
    );
}
