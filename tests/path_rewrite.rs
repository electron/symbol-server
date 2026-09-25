//! Ported case for case from the Node suite's test/path-rewrite.test.js.
//!
//! Path rewriting is tested through the redirect endpoint, which exposes the
//! rewritten path in the Location header without needing a working upstream.
//! Setting `x-electron-symbol-redirect: 1` triggers the redirect branch.

mod common;

use common::*;

const TARGET_HOST: &str = "symbols.example.test";

async fn server(path_prefix: Option<&str>) -> SymbolServer {
    start_symbol_server(ServerOptions {
        target_host: Some(TARGET_HOST),
        path_prefix,
        ..Default::default()
    })
    .await
    .unwrap()
}

/// Returns the raw path (and query) of the redirect Location, so callers can
/// assert on encoded characters.
async fn rewritten_path(server: &SymbolServer, path: &str) -> String {
    let res = request(server.port, path, &[("x-electron-symbol-redirect", "1")]).await;
    assert_eq!(res.status, 302, "expected 302, got {}", res.status);
    let location = res
        .header("location")
        .expect("Location header should be set");
    let rest = location
        .strip_prefix("https://")
        .unwrap_or_else(|| panic!("expected an https: location, got {location}"));
    let (host, path) = rest.split_at(rest.find('/').unwrap_or(rest.len()));
    assert_eq!(host, TARGET_HOST);
    path.to_string()
}

/// 'path rewriting (no PATH_PREFIX)'
mod no_path_prefix {
    use super::*;

    #[tokio::test]
    async fn lowercases_the_request_path() {
        let s = server(None).await;
        let out = rewritten_path(&s, "/Foo/BAR.PDB/ABCDEF/Foo.PDB").await;
        assert_eq!(out, "/foo/bar.pdb/abcdef/foo.pdb");
    }

    #[tokio::test]
    async fn replaces_2b_with_20() {
        let s = server(None).await;
        let out = rewritten_path(&s, "/foo%2bbar.pdb/abc/foo%2bbar.pdb").await;
        assert_eq!(out, "/foo%20bar.pdb/abc/foo%20bar.pdb");
    }

    #[tokio::test]
    async fn replaces_literal_plus_with_20() {
        let s = server(None).await;
        let out = rewritten_path(&s, "/foo+bar.pdb/abc/foo+bar.pdb").await;
        assert_eq!(out, "/foo%20bar.pdb/abc/foo%20bar.pdb");
    }

    #[tokio::test]
    async fn rewrites_slack_alias_to_electron_slash_form() {
        let s = server(None).await;
        let out = rewritten_path(&s, "/slack/electron.exe.pdb/ABC/file").await;
        assert_eq!(out, "/electron/electron.exe.pdb/abc/file");
    }

    #[tokio::test]
    async fn rewrites_notion_alias_to_electron_slash_form() {
        let s = server(None).await;
        let out = rewritten_path(&s, "/notion/electron.exe.pdb/ABC/file").await;
        assert_eq!(out, "/electron/electron.exe.pdb/abc/file");
    }

    #[tokio::test]
    async fn rewrites_claude_alias_to_electron() {
        let s = server(None).await;
        let out = rewritten_path(&s, "/claude/foo.pdb/ABC/file").await;
        assert_eq!(out, "/electron/foo.pdb/abc/file");
    }

    #[tokio::test]
    async fn encoded_multi_word_alias_is_partially_rewritten_via_single_word_prefix() {
        // The "notion dev" alias patterns only match literal spaces, which
        // can't arrive in a request target. The single-word "notion" alias's
        // %20 pattern (`/notion%20`) does match the encoded form, leaving the
        // "%20dev" suffix attached. This documents that current behavior.
        let s = server(None).await;
        let out = rewritten_path(&s, "/notion%20dev/foo.pdb/ABC/file").await;
        assert_eq!(out, "/electron%20dev/foo.pdb/abc/file");
    }

    #[tokio::test]
    async fn rewrites_20_space_form_slack_helper() {
        let s = server(None).await;
        let out = rewritten_path(&s, "/slack%20helper.exe.pdb/ABC/slack%20helper.exe.pdb").await;
        assert_eq!(
            out,
            "/electron%20helper.exe.pdb/abc/electron%20helper.exe.pdb"
        );
    }

    #[tokio::test]
    async fn rewrites_dot_form_slack_exe() {
        let s = server(None).await;
        let out = rewritten_path(&s, "/slack.exe.pdb/ABC/slack.exe.pdb").await;
        assert_eq!(out, "/electron.exe.pdb/abc/electron.exe.pdb");
    }

    #[tokio::test]
    async fn does_not_rewrite_alias_when_not_preceded_by_slash_space_or_dot() {
        // The pattern requires the alias to be flanked specifically. A SHA
        // that happens to contain "slack" should not be rewritten in place.
        let s = server(None).await;
        let out = rewritten_path(&s, "/abcslackdef/foo.pdb/abc/file").await;
        assert_eq!(out, "/abcslackdef/foo.pdb/abc/file");
    }

    #[tokio::test]
    async fn strips_windows_c_projects_prefix_url_encoded() {
        let s = server(None).await;
        let out = rewritten_path(
            &s,
            "/c%3a%5cprojects%5csrc%5cout%5cdefault%5cfoo.pdb/abc/foo.pdb",
        )
        .await;
        assert_eq!(out, "/foo.pdb/abc/foo.pdb");
    }
}

/// 'path rewriting (with PATH_PREFIX)'
mod with_path_prefix {
    use super::*;

    const PREFIX: Option<&str> = Some("/symbols/release");

    #[tokio::test]
    async fn prepends_path_prefix_to_the_rewritten_path() {
        let s = server(PREFIX).await;
        let out = rewritten_path(&s, "/Foo/Bar/Baz").await;
        assert_eq!(out, "/symbols/release/foo/bar/baz");
    }

    #[tokio::test]
    async fn path_prefix_is_added_after_rewrites_are_applied() {
        let s = server(PREFIX).await;
        let out = rewritten_path(&s, "/slack/foo.pdb/ABC/file").await;
        assert_eq!(out, "/symbols/release/electron/foo.pdb/abc/file");
    }
}
