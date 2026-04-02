'use strict';

const test = require('node:test');
const assert = require('node:assert/strict');
const url = require('url');

const { startSymbolServer, request } = require('./helpers');

// We test path rewriting through the redirect endpoint, which exposes the
// rewritten path in the Location header without requiring a working upstream.
// Setting `x-electron-symbol-redirect: 1` triggers the redirect branch.

const TARGET_HOST = 'symbols.example.test';

async function getRewrittenPath(server, requestPath) {
  const res = await request(server.port, requestPath, {
    'x-electron-symbol-redirect': '1',
  });
  assert.equal(res.statusCode, 302, `expected 302, got ${res.statusCode}`);
  const location = res.headers.location;
  assert.ok(location, 'Location header should be set');
  const parsed = new url.URL(location);
  assert.equal(parsed.protocol, 'https:');
  assert.equal(parsed.host, TARGET_HOST);
  // Return raw path+search so callers can assert on encoded characters.
  return parsed.pathname + parsed.search;
}

test('path rewriting (no PATH_PREFIX)', async (t) => {
  const server = await startSymbolServer({ targetHost: TARGET_HOST });
  t.after(() => server.stop());

  await t.test('lowercases the request path', async () => {
    const out = await getRewrittenPath(server, '/Foo/BAR.PDB/ABCDEF/Foo.PDB');
    assert.equal(out, '/foo/bar.pdb/abcdef/foo.pdb');
  });

  await t.test('replaces %2b with %20', async () => {
    const out = await getRewrittenPath(server, '/foo%2bbar.pdb/abc/foo%2bbar.pdb');
    assert.equal(out, '/foo%20bar.pdb/abc/foo%20bar.pdb');
  });

  await t.test('replaces literal + with %20', async () => {
    const out = await getRewrittenPath(server, '/foo+bar.pdb/abc/foo+bar.pdb');
    assert.equal(out, '/foo%20bar.pdb/abc/foo%20bar.pdb');
  });

  await t.test('rewrites slack alias to electron (slash form)', async () => {
    const out = await getRewrittenPath(server, '/slack/electron.exe.pdb/ABC/file');
    assert.equal(out, '/electron/electron.exe.pdb/abc/file');
  });

  await t.test('rewrites notion alias to electron (slash form)', async () => {
    const out = await getRewrittenPath(server, '/notion/electron.exe.pdb/ABC/file');
    assert.equal(out, '/electron/electron.exe.pdb/abc/file');
  });

  await t.test('rewrites claude alias to electron', async () => {
    const out = await getRewrittenPath(server, '/claude/foo.pdb/ABC/file');
    assert.equal(out, '/electron/foo.pdb/abc/file');
  });

  await t.test('encoded multi-word alias is partially rewritten via single-word prefix', async () => {
    // The "notion dev" alias regexes only match literal spaces (unreachable
    // through Node's HTTP parser). However, the single-word "notion" alias's
    // %20 pattern (`/notion%20`) DOES match the encoded form, leaving the
    // " dev" suffix attached. This documents that current behavior.
    const out = await getRewrittenPath(server, '/notion%20dev/foo.pdb/ABC/file');
    assert.equal(out, '/electron%20dev/foo.pdb/abc/file');
  });

  await t.test('rewrites %20 (space) form: "/slack%20helper..."', async () => {
    const out = await getRewrittenPath(
      server,
      '/slack%20helper.exe.pdb/ABC/slack%20helper.exe.pdb',
    );
    assert.equal(
      out,
      '/electron%20helper.exe.pdb/abc/electron%20helper.exe.pdb',
    );
  });

  await t.test('rewrites dot form: "/slack.exe..."', async () => {
    const out = await getRewrittenPath(server, '/slack.exe.pdb/ABC/slack.exe.pdb');
    assert.equal(out, '/electron.exe.pdb/abc/electron.exe.pdb');
  });

  await t.test('does not rewrite alias when not preceded by /, space, or .', async () => {
    // The pattern requires the alias to be flanked specifically. A SHA that
    // happens to contain "slack" should not be rewritten in-place.
    const out = await getRewrittenPath(server, '/abcslackdef/foo.pdb/abc/file');
    assert.equal(out, '/abcslackdef/foo.pdb/abc/file');
  });

  await t.test('strips windows c:\\projects\\... prefix (URL-encoded)', async () => {
    const out = await getRewrittenPath(
      server,
      '/c%3a%5cprojects%5csrc%5cout%5cdefault%5cfoo.pdb/abc/foo.pdb',
    );
    assert.equal(out, '/foo.pdb/abc/foo.pdb');
  });
});

test('path rewriting (with PATH_PREFIX)', async (t) => {
  const server = await startSymbolServer({
    targetHost: TARGET_HOST,
    pathPrefix: '/symbols/release',
  });
  t.after(() => server.stop());

  await t.test('prepends PATH_PREFIX to the rewritten path', async () => {
    const res = await request(server.port, '/Foo/Bar/Baz', {
      'x-electron-symbol-redirect': '1',
    });
    assert.equal(res.statusCode, 302);
    const parsed = new url.URL(res.headers.location);
    assert.equal(parsed.pathname, '/symbols/release/foo/bar/baz');
  });

  await t.test('PATH_PREFIX is added after rewrites are applied', async () => {
    const res = await request(server.port, '/slack/foo.pdb/ABC/file', {
      'x-electron-symbol-redirect': '1',
    });
    assert.equal(res.statusCode, 302);
    const parsed = new url.URL(res.headers.location);
    assert.equal(parsed.pathname, '/symbols/release/electron/foo.pdb/abc/file');
  });
});
