'use strict';

const test = require('node:test');
const assert = require('node:assert/strict');

const { startSymbolServer, startProxy, request } = require('./helpers');

test('GET /health responds 200 with "Alive"', async (t) => {
  const server = await startSymbolServer({ targetHost: '127.0.0.1:1' });
  t.after(() => server.stop());

  const res = await request(server.port, '/health');
  assert.equal(res.statusCode, 200);
  assert.equal(res.body, 'Alive');
});

test('GET /health is not affected by missing-symbol cache or rewrites', async (t) => {
  const server = await startSymbolServer({
    targetHost: '127.0.0.1:1',
    pathPrefix: '/some/prefix',
  });
  t.after(() => server.stop());

  const res = await request(server.port, '/health');
  assert.equal(res.statusCode, 200);
  assert.equal(res.body, 'Alive');
});

test('symbolicator/* user-agent gets a 302 redirect', async (t) => {
  const server = await startSymbolServer({ targetHost: 'symbols.example.test' });
  t.after(() => server.stop());

  const res = await request(server.port, '/Foo/Bar', {
    'user-agent': 'symbolicator/1.2.3',
  });
  assert.equal(res.statusCode, 302);
  assert.ok(
    res.headers.location?.startsWith('https://symbols.example.test/'),
    `unexpected location: ${res.headers.location}`,
  );
  assert.equal(new URL(res.headers.location).pathname, '/foo/bar');
});

test('non-symbolicator user-agents do NOT get redirected', async (t) => {
  const { server } = await startProxy(t, {
    handler: (req, res) => {
      res.writeHead(200, { 'content-type': 'text/plain' });
      res.end('hello');
    },
  });

  const res = await request(server.port, '/Foo/Bar', {
    'user-agent': 'Microsoft-Symbol-Server/10.0',
  });
  assert.equal(res.statusCode, 200);
  assert.equal(res.body, 'hello');
});

test('symbolicator UA without trailing slash is not treated as redirect', async (t) => {
  // The check is `userAgent.startsWith('symbolicator/')` — bare "symbolicator"
  // (no slash) should fall through to the proxy path.
  const { server } = await startProxy(t, {
    handler: (req, res) => {
      res.writeHead(200);
      res.end('proxied');
    },
  });

  const res = await request(server.port, '/foo/bar', {
    'user-agent': 'symbolicator',
  });
  assert.equal(res.statusCode, 200);
  assert.equal(res.body, 'proxied');
});

test('proxy forwards request to upstream with rewritten path', async (t) => {
  const { server, upstream } = await startProxy(t, {
    handler: (req, res) => {
      res.writeHead(200, { 'content-type': 'application/octet-stream' });
      res.end('SYMBOL-DATA');
    },
  });

  const res = await request(server.port, '/Foo/Bar.PDB/ABC/foo.pdb');
  assert.equal(res.statusCode, 200);
  assert.equal(res.body, 'SYMBOL-DATA');

  assert.equal(upstream.requests.length, 1);
  assert.equal(upstream.requests[0].url, '/foo/bar.pdb/abc/foo.pdb');
  assert.equal(upstream.requests[0].headers.host, upstream.host);
});

test('proxy applies PATH_PREFIX before forwarding', async (t) => {
  const { server, upstream } = await startProxy(t, { pathPrefix: '/release/symbols' });

  const res = await request(server.port, '/Foo/Bar.PDB/ABC/foo.pdb');
  assert.equal(res.statusCode, 200);
  assert.equal(upstream.requests.length, 1);
  assert.equal(upstream.requests[0].url, '/release/symbols/foo/bar.pdb/abc/foo.pdb');
});

test('proxy preserves and lowercases query strings', async (t) => {
  const { server, upstream } = await startProxy(t);

  const res = await request(server.port, '/Foo/Bar.PDB?Baz=QUUX');
  assert.equal(res.statusCode, 200);
  assert.equal(upstream.requests.length, 1);
  assert.equal(upstream.requests[0].url, '/foo/bar.pdb?baz=quux');
});

test('proxy applies app aliasing before forwarding', async (t) => {
  const { server, upstream } = await startProxy(t);

  const res = await request(server.port, '/slack/foo.pdb/ABC/file');
  assert.equal(res.statusCode, 200);
  assert.equal(upstream.requests.length, 1);
  assert.equal(upstream.requests[0].url, '/electron/foo.pdb/abc/file');
});

test('upstream 403 is converted to 404 (and CORS headers set)', async (t) => {
  const { server } = await startProxy(t, {
    handler: (req, res) => {
      res.writeHead(403, { 'content-type': 'text/plain' });
      res.end('forbidden');
    },
  });

  const res = await request(server.port, '/missing/foo.pdb/ABC/foo.pdb');
  assert.equal(res.statusCode, 404, 'expected 403 to be rewritten to 404');
  assert.equal(res.headers['access-control-allow-origin'], '*');
  assert.equal(res.headers['access-control-allow-methods'], 'GET');
});

test('subsequent requests for known-missing symbols are served from cache as 404', async (t) => {
  let calls = 0;
  const { server } = await startProxy(t, {
    handler: (req, res) => {
      calls += 1;
      res.writeHead(403);
      res.end();
    },
  });

  const first = await request(server.port, '/some/Path/abc/file.pdb');
  assert.equal(first.statusCode, 404);
  assert.equal(calls, 1);

  const second = await request(server.port, '/some/Path/abc/file.pdb');
  assert.equal(second.statusCode, 404);
  assert.equal(calls, 1, 'cached miss should NOT contact upstream again');

  const third = await request(server.port, '/some/Other/abc/file.pdb');
  assert.equal(third.statusCode, 404);
  assert.equal(calls, 2);
});

test('upstream non-403 errors are passed through and not cached as missing', async (t) => {
  let response = 500;
  let calls = 0;
  const { server } = await startProxy(t, {
    handler: (req, res) => {
      calls += 1;
      res.writeHead(response);
      res.end(response === 200 ? 'data' : '');
    },
  });

  const first = await request(server.port, '/some/path/abc/file.pdb');
  assert.equal(first.statusCode, 500);
  assert.equal(calls, 1);

  // Same path should hit upstream again, not be served from the missing cache.
  response = 200;
  const second = await request(server.port, '/some/path/abc/file.pdb');
  assert.equal(second.statusCode, 200);
  assert.equal(second.body, 'data');
  assert.equal(calls, 2);
});

test('proxy returns 500 with error ID when upstream is unreachable', async (t) => {
  const server = await startSymbolServer({ targetHost: '127.0.0.1:1' });
  t.after(() => server.stop());

  const res = await request(server.port, '/foo/bar/abc/file.pdb');
  assert.equal(res.statusCode, 500);
  assert.equal(res.headers['content-type'], 'text/plain');
  assert.match(res.body, /Something went wrong.*error ID: "[0-9a-f-]+"/i);
});

test('asserts when TARGET_HOST is missing', async () => {
  await assert.rejects(
    () => startSymbolServer({ targetHost: undefined }),
    /exited with code .* before listening/,
  );
});
