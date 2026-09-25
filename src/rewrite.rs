//! Request-target parsing and the incoming-path to upstream-path mapping.
//!
//! The Node server looked at each request target in two slightly different
//! ways, and both are reproduced here so cache keys, redirects and upstream
//! paths come out byte-for-byte the same:
//!
//! * [`whatwg_path_and_search`] mirrors `new URL(\`http://localhost${req.url}\`)`,
//!   which the Node server used for the `/health` check, the redirect
//!   `Location`, and negative-cache lookups.
//! * [`legacy_proxy_path`] mirrors the path http-proxy sent upstream
//!   (`url.parse(req.url).path` joined onto the target), which is also the
//!   key negative-cache entries were stored under.

/// Apps that rename "Electron" / "Electron Helper" to their own name; their
/// symbol requests are aliased back to the Electron names.
const APPS_TO_ALIAS: &[&str] = &["slack", "notion", "notion dev", "claude", "claude nest"];

pub struct PathRewriter {
    replacements: Vec<(String, &'static str)>,
    prefix: String,
}

impl PathRewriter {
    pub fn new(prefix: impl Into<String>) -> Self {
        // Temporary hack for apps that rename Electron / Electron Helper to
        // My App / My App Helper. The alias must be preceded by "/" so an
        // app name that happens to appear inside a SHA isn't rewritten.
        let mut replacements = Vec::new();
        for app in APPS_TO_ALIAS {
            replacements.push((format!("/{app}/"), "/electron/"));
            replacements.push((format!("/{app}%20"), "/electron%20"));
            replacements.push((format!("/{app}."), "/electron."));
        }
        replacements.push((r"/c:\projects\src\out\default\".to_string(), "/"));
        replacements.push(("/c%3a%5cprojects%5csrc%5cout%5cdefault%5c".to_string(), "/"));
        Self {
            replacements,
            prefix: prefix.into(),
        }
    }

    /// Port of `incomingPathToProxyPath`.
    pub fn rewrite(&self, path: &str) -> String {
        // symstore.exe and symsrv.dll don't always agree on the case of a
        // symbol path, and the artifact store is case-sensitive. Symbols are
        // uploaded with all-lowercase keys, so lowercase every request.
        let mut path = path.to_lowercase();
        // Some symbol servers send + instead of " ".
        path = path.replace("%2b", "%20").replace('+', "%20");
        for (from, to) in &self.replacements {
            if path.contains(from.as_str()) {
                path = path.replace(from.as_str(), to);
            }
        }
        // The symbols may be hosted under a deeper path in the artifact store.
        format!("{}{}", self.prefix, path)
    }
}

/// Characters the WHATWG URL parser percent-encodes in a path (on top of C0
/// controls, DEL and non-ASCII).
const PATH_ENCODE: &[u8] = b" \"#<>?^`{}";
/// Characters the WHATWG URL parser percent-encodes in a special-scheme query.
const QUERY_ENCODE: &[u8] = b" \"#<>'";

/// Returns `(pathname, search)` as `new URL(\`http://localhost${target}\`)`
/// would: backslashes become slashes, dot segments are resolved, unsafe
/// characters are percent-encoded, the fragment is dropped, and an empty
/// query yields an empty `search`.
pub fn whatwg_path_and_search(target: &str) -> (String, String) {
    let target = target.split('#').next().unwrap_or_default();
    let (path, query) = match target.split_once('?') {
        Some((path, query)) => (path, Some(query)),
        None => (target, None),
    };

    let path = path.replace('\\', "/");
    let rest = path.strip_prefix('/').unwrap_or(&path);
    let segments: Vec<&str> = rest.split('/').collect();
    let mut out: Vec<String> = Vec::with_capacity(segments.len());
    for (i, segment) in segments.iter().enumerate() {
        let last = i + 1 == segments.len();
        if is_double_dot(segment) {
            out.pop();
            if last {
                out.push(String::new());
            }
        } else if is_single_dot(segment) {
            if last {
                out.push(String::new());
            }
        } else {
            out.push(percent_encode(segment, PATH_ENCODE));
        }
    }
    let pathname = format!("/{}", out.join("/"));

    let search = match query {
        Some(q) if !q.is_empty() => format!("?{}", percent_encode(q, QUERY_ENCODE)),
        _ => String::new(),
    };
    (pathname, search)
}

fn is_single_dot(s: &str) -> bool {
    s == "." || s.eq_ignore_ascii_case("%2e")
}

fn is_double_dot(s: &str) -> bool {
    matches!(
        s.to_ascii_lowercase().as_str(),
        ".." | ".%2e" | "%2e." | "%2e%2e"
    )
}

fn percent_encode(s: &str, set: &[u8]) -> String {
    let mut out = String::with_capacity(s.len());
    for &b in s.as_bytes() {
        if !(0x20..0x7f).contains(&b) || set.contains(&b) {
            out.push_str(&format!("%{b:02X}"));
        } else {
            out.push(b as char);
        }
    }
    out
}

/// The path http-proxy forwarded upstream for a request target:
/// `url.parse(target).path` (backslashes before the query become slashes,
/// the fragment is dropped) joined onto `/` with http-proxy's `urlJoin`,
/// which collapses runs of slashes in the path part.
pub fn legacy_proxy_path(target: &str) -> String {
    let split = target.find(['?', '#']).unwrap_or(target.len());
    let (before, after) = target.split_at(split);
    let parsed = format!("{}{}", before.replace('\\', "/"), after);
    let parsed = parsed.split('#').next().unwrap_or_default();

    let (path, query) = match parsed.split_once('?') {
        Some((path, query)) => (path, Some(query)),
        None => (parsed, None),
    };
    let mut joined = String::with_capacity(parsed.len() + 1);
    for c in format!("/{path}").chars() {
        if !(c == '/' && joined.ends_with('/')) {
            joined.push(c);
        }
    }
    let mut joined = joined
        .replacen("http:/", "http://", 1)
        .replacen("https:/", "https://", 1);
    if let Some(query) = query {
        joined.push('?');
        joined.push_str(query);
    }
    joined
}

/// The redirect target, as Node's legacy `url.format` built it from the
/// rewritten path. It escapes `?` and `#` in the pathname, so any query ends
/// up percent-encoded into the path.
pub fn redirect_location(target_host: &str, path: &str) -> String {
    let path = path.replace('#', "%23").replace('?', "%3F");
    let slash = if path.starts_with('/') { "" } else { "/" };
    format!("https://{target_host}{slash}{path}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn whatwg(target: &str) -> String {
        let (pathname, search) = whatwg_path_and_search(target);
        pathname + &search
    }

    // Expected values below were captured from Node 24's `new URL()` and
    // `url.parse()`.
    #[test]
    fn whatwg_matches_node_url() {
        let cases = [
            ("/Foo/Bar.PDB?Baz=QUUX", "/Foo/Bar.PDB?Baz=QUUX"),
            ("/a^b{c}|d`e'f\"g", "/a%5Eb%7Bc%7D|d%60e'f%22g"),
            ("/a\\b\\c?x\\y", "/a/b/c?x\\y"),
            ("//foo///bar", "//foo///bar"),
            ("/a/./b/../c", "/a/c"),
            ("/a/%2e/b/%2E%2e/c", "/a/c"),
            ("/foo?", "/foo"),
            ("/foo?A=B#frag", "/foo?A=B"),
            ("/a%2Fb", "/a%2Fb"),
            ("/health?x=1", "/health?x=1"),
            ("/./health", "/health"),
            ("/x/..", "/"),
            ("/foo#bar", "/foo"),
            ("///x", "///x"),
            ("/a@b", "/a@b"),
            ("/q?a b\"<>'", "/q?a%20b%22%3C%3E%27"),
        ];
        for (input, expected) in cases {
            assert_eq!(whatwg(input), expected, "{input:?}");
        }
    }

    #[test]
    fn legacy_proxy_path_matches_http_proxy() {
        let cases = [
            ("/Foo/Bar.PDB?Baz=QUUX", "/Foo/Bar.PDB?Baz=QUUX"),
            ("/a^b{c}|d`e'f\"g", "/a^b{c}|d`e'f\"g"),
            ("/a\\b\\c?x\\y", "/a/b/c?x\\y"),
            ("//foo///bar", "/foo/bar"),
            ("/a/./b/../c", "/a/./b/../c"),
            ("/foo?", "/foo?"),
            ("/foo?A=B#frag", "/foo?A=B"),
            ("/foo#bar", "/foo"),
            ("/x?a//b?c", "/x?a//b?c"),
            ("/http:/x", "/http://x"),
        ];
        for (input, expected) in cases {
            assert_eq!(legacy_proxy_path(input), expected, "{input:?}");
        }
    }

    #[test]
    fn rewrite_lowercases_and_aliases() {
        let r = PathRewriter::new("");
        assert_eq!(
            r.rewrite("/Foo/BAR.PDB/ABCDEF/Foo.PDB"),
            "/foo/bar.pdb/abcdef/foo.pdb"
        );
        assert_eq!(r.rewrite("/foo%2Bbar+baz"), "/foo%20bar%20baz");
        assert_eq!(r.rewrite("/Slack/x"), "/electron/x");
        assert_eq!(r.rewrite("/notion%20dev/x"), "/electron%20dev/x");
        assert_eq!(r.rewrite("/claude.exe.pdb"), "/electron.exe.pdb");
        assert_eq!(r.rewrite("/abcslackdef/x"), "/abcslackdef/x");
        assert_eq!(
            r.rewrite(r"/C:\Projects\src\out\Default\foo.pdb"),
            "/foo.pdb"
        );
        assert_eq!(
            r.rewrite("/c%3A%5Cprojects%5Csrc%5Cout%5Cdefault%5Cfoo.pdb"),
            "/foo.pdb"
        );
    }

    #[test]
    fn rewrite_applies_prefix_last() {
        let r = PathRewriter::new("/symbols/release");
        assert_eq!(r.rewrite("/Slack/Foo"), "/symbols/release/electron/foo");
        // The prefix itself is not lowercased or rewritten.
        let r = PathRewriter::new("/Upper");
        assert_eq!(r.rewrite("/Foo"), "/Upper/foo");
    }

    #[test]
    fn redirect_location_matches_url_format() {
        assert_eq!(redirect_location("h", "/foo/bar"), "https://h/foo/bar");
        assert_eq!(
            redirect_location("h", "/foo?baz=quux"),
            "https://h/foo%3Fbaz=quux"
        );
        assert_eq!(redirect_location("h", "prefix/foo"), "https://h/prefix/foo");
    }
}
