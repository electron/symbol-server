'use strict';

const http = require('node:http');
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

test('negative-cache 404s carry a public Cache-Control header', async (t) => {
  const { server } = await startProxy(t, {
    handler: (req, res) => {
      res.writeHead(403);
      res.end();
    },
  });

  const first = await request(server.port, '/missing/foo.pdb/abc/foo.pdb');
  assert.equal(first.statusCode, 404);
  assert.equal(first.headers['cache-control'], 'public, max-age=3600');

  // Served from the negative cache without contacting upstream.
  const second = await request(server.port, '/missing/foo.pdb/abc/foo.pdb');
  assert.equal(second.statusCode, 404);
  assert.equal(second.headers['cache-control'], 'public, max-age=3600');
});

test('successful 200s get a long immutable Cache-Control when upstream sends none', async (t) => {
  const { server } = await startProxy(t, {
    handler: (req, res) => {
      res.writeHead(200);
      res.end('SYMBOL-DATA');
    },
  });

  const res = await request(server.port, '/foo/bar.pdb/abc/foo.pdb');
  assert.equal(res.statusCode, 200);
  assert.equal(res.headers['cache-control'], 'public, max-age=604800, immutable');
});

test('upstream Cache-Control on 200s is preserved', async (t) => {
  const { server } = await startProxy(t, {
    handler: (req, res) => {
      res.writeHead(200, { 'cache-control': 'public, max-age=60' });
      res.end('SYMBOL-DATA');
    },
  });

  const res = await request(server.port, '/foo/bar.pdb/abc/foo.pdb');
  assert.equal(res.statusCode, 200);
  assert.equal(res.headers['cache-control'], 'public, max-age=60');
});

test('redirect responses are edge-cacheable', async (t) => {
  const server = await startSymbolServer({ targetHost: 'symbols.example.test' });
  t.after(() => server.stop());

  const res = await request(server.port, '/Foo/Bar', {
    'user-agent': 'symbolicator/1.2.3',
  });
  assert.equal(res.statusCode, 302);
  assert.equal(res.headers['cache-control'], 'public, max-age=3600');
});

test('sheds load with 503 + Retry-After above the upstream concurrency cap', async (t) => {
  let releaseFirst;
  const firstHeld = new Promise((resolve) => { releaseFirst = resolve; });
  const { server, upstream } = await startProxy(t, {
    env: { MAX_UPSTREAM_CONCURRENCY: '1' },
    handler: async (req, res) => {
      await firstHeld;
      res.writeHead(200);
      res.end('ok');
    },
  });

  const first = request(server.port, '/held/foo.pdb/abc/foo.pdb');
  // Wait until the first request has actually reached upstream.
  while (upstream.requests.length === 0) {
    await new Promise((resolve) => setTimeout(resolve, 10));
  }

  const shed = await request(server.port, '/other/foo.pdb/abc/foo.pdb');
  assert.equal(shed.statusCode, 503);
  assert.equal(shed.headers['retry-after'], '30');
  assert.equal(upstream.requests.length, 1, 'shed request should not reach upstream');

  releaseFirst();
  const held = await first;
  assert.equal(held.statusCode, 200);
});

test('same-path dedup waiters cannot bypass the upstream concurrency cap', async (t) => {
  // Regression test: waiters queued behind an in-flight leader used to all
  // call proxyToUpstream when the leader settled. Each overwrote the same
  // in-flight map key, so the Map.size-based cap check saw 1 while N upstream
  // requests were actually active (observed: 6 with a cap of 2).
  let active = 0;
  let maxActive = 0;
  let phase = 'leader';
  let releaseLeader;
  const leaderHeld = new Promise((resolve) => { releaseLeader = resolve; });
  let releaseWaiters;
  const waitersHeld = new Promise((resolve) => { releaseWaiters = resolve; });

  const { server, upstream } = await startProxy(t, {
    env: { MAX_UPSTREAM_CONCURRENCY: '2' },
    handler: async (req, res) => {
      active += 1;
      maxActive = Math.max(maxActive, active);
      if (phase === 'leader') await leaderHeld; else await waitersHeld;
      res.writeHead(200);
      res.end('ok');
      active -= 1;
    },
  });

  const PATH = '/stampede/foo.pdb/abc/foo.pdb';
  const leader = request(server.port, PATH);
  while (upstream.requests.length === 0) {
    await new Promise((resolve) => setTimeout(resolve, 10));
  }

  // Queue six identical lookups; all should dedup-wait on the leader.
  phase = 'waiters';
  const waiters = [];
  for (let i = 0; i < 6; i++) waiters.push(request(server.port, PATH));
  await new Promise((resolve) => setTimeout(resolve, 200));
  assert.equal(upstream.requests.length, 1, 'waiters must not reach upstream while leader is in flight');

  // Leader succeeds; woken waiters re-check the cap, so only two may proxy.
  releaseLeader();
  const deadline = Date.now() + 2000;
  while (upstream.requests.length < 3 && Date.now() < deadline) {
    await new Promise((resolve) => setTimeout(resolve, 10));
  }
  // Grace period to catch any waiters that slipped past the cap.
  await new Promise((resolve) => setTimeout(resolve, 200));
  releaseWaiters();

  const leaderRes = await leader;
  const waiterRes = await Promise.all(waiters);

  assert.equal(leaderRes.statusCode, 200);
  assert.ok(maxActive <= 2, `at most 2 simultaneous upstream requests allowed, saw ${maxActive}`);
  assert.equal(upstream.requests.length, 3, 'leader + at most cap-many waiters may reach upstream');

  const okCount = waiterRes.filter((r) => r.statusCode === 200).length;
  const shed = waiterRes.filter((r) => r.statusCode === 503);
  assert.equal(okCount, 2, 'exactly cap-many waiters should be proxied');
  assert.equal(shed.length, 4, 'remaining waiters should be shed');
  for (const r of shed) assert.equal(r.headers['retry-after'], '30');
});

test('a dedup waiter whose client disconnects mid-wait does not leak an upstream slot', async (t) => {
  // Regression test: a same-path waiter used to call proxyToUpstream when the
  // leader settled even if its own client had already hung up. The response's
  // 'close' event had fired before the settle listeners were registered, so
  // settle never ran and the incremented activeUpstreamRequests slot leaked
  // forever. With a cap of 1 a single canceled waiter then turned every
  // subsequent distinct-path request into a 503 until process restart.
  const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));
  let phase = 'leader';
  let releaseLeader;
  const leaderHeld = new Promise((resolve) => { releaseLeader = resolve; });
  const { server, upstream } = await startProxy(t, {
    env: { MAX_UPSTREAM_CONCURRENCY: '1' },
    handler: async (req, res) => {
      if (phase === 'leader') await leaderHeld;
      res.writeHead(200);
      res.end('ok');
    },
  });

  const PATH = '/held/foo.pdb/abc/foo.pdb';
  const leader = request(server.port, PATH);
  while (upstream.requests.length === 0) await sleep(10);

  // Same-path waiter; destroy its client socket while it waits on the leader.
  const waiter = http.request({ host: '127.0.0.1', port: server.port, path: PATH, method: 'GET' });
  waiter.on('error', () => {});
  waiter.end();
  await sleep(200); // let the server register it as a dedup waiter
  waiter.destroy();
  await sleep(200); // let the server-side 'close' fire

  phase = 'done';
  releaseLeader();
  const leaderRes = await leader;
  assert.equal(leaderRes.statusCode, 200);
  await sleep(200); // let the canceled waiter wake and (previously) leak

  const probe = await request(server.port, '/distinct/bar.pdb/def/bar.pdb');
  assert.equal(probe.statusCode, 200, 'canceled waiter must not leak an upstream slot');
  assert.equal(upstream.requests.length, 2, 'only the leader and the probe should reach upstream');
});

test('client disconnect mid-proxy aborts upstream and frees the slot only after', async (t) => {
  // Regression test: the slot counter and dedup promise used to settle when
  // the DOWNSTREAM response closed, but http-proxy does not cancel the
  // UPSTREAM request on its own (its req 'aborted' hook never fires for
  // fully-received requests on modern Node). A client disconnecting mid-proxy
  // therefore freed its slot while the upstream fetch kept running: with a
  // cap of 1, a distinct second request then also reached upstream, which saw
  // 2 simultaneous active requests and never observed an abort.
  const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));
  let active = 0;
  let maxActive = 0;
  let aborts = 0;
  let release;
  const held = new Promise((resolve) => { release = resolve; });
  const { server, upstream } = await startProxy(t, {
    env: { MAX_UPSTREAM_CONCURRENCY: '1' },
    handler: async (req, res) => {
      active += 1;
      maxActive = Math.max(maxActive, active);
      res.on('close', () => {
        if (!res.writableEnded) aborts += 1;
        active -= 1;
      });
      await held;
      if (!res.destroyed) {
        res.writeHead(200);
        res.end('ok');
      }
    },
  });

  // First request reaches the held upstream, then its client disconnects.
  const first = http.request({
    host: '127.0.0.1', port: server.port, path: '/held/foo.pdb/abc/foo.pdb', method: 'GET',
  });
  first.on('error', () => {});
  first.end();
  while (upstream.requests.length === 0) await sleep(10);
  first.destroy();

  // The upstream request must actually be canceled, not left running.
  const abortDeadline = Date.now() + 2000;
  while (aborts === 0 && Date.now() < abortDeadline) await sleep(10);
  assert.equal(aborts, 1, 'upstream must observe the abort after the client disconnects');
  assert.equal(active, 0, 'upstream must have no active request left');
  await sleep(50); // let the freed slot settle server-side

  // A distinct second request may now use the freed slot — but must never
  // have overlapped with the first at upstream.
  const second = request(server.port, '/other/bar.pdb/def/bar.pdb');
  const reachDeadline = Date.now() + 2000;
  while (upstream.requests.length < 2 && Date.now() < reachDeadline) await sleep(10);
  assert.equal(upstream.requests.length, 2, 'second request should reach upstream after the abort');
  release();
  const res2 = await second;
  assert.equal(res2.statusCode, 200);
  assert.ok(maxActive <= 1, `upstream must never see 2 simultaneous active requests, saw ${maxActive}`);
});

test('concurrent requests for the same missing path only hit upstream once', async (t) => {
  const { server, upstream } = await startProxy(t, {
    handler: (req, res) => {
      setTimeout(() => {
        res.writeHead(403);
        res.end();
      }, 100);
    },
  });

  const [first, second] = await Promise.all([
    request(server.port, '/dup/foo.pdb/abc/foo.pdb'),
    request(server.port, '/dup/foo.pdb/abc/foo.pdb'),
  ]);
  assert.equal(first.statusCode, 404);
  assert.equal(second.statusCode, 404);
  assert.equal(upstream.requests.length, 1, 'duplicate lookup should not reach upstream');
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
