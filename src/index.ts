import assert from 'assert';
import * as crypto from 'crypto';
import * as http from 'http';
import httpProxy from 'http-proxy';
import LRU from 'lru-cache';
import * as url from 'url';

const { PATH_PREFIX, TARGET_HOST, MAX_UPSTREAM_CONCURRENCY, UA_LOG_SAMPLE_RATE } = process.env;

assert(TARGET_HOST, 'TARGET_HOST is defined');

// How long a "this symbol does not exist" answer stays valid, both in our
// in-memory cache and in any CDN/client honoring Cache-Control. Kept short-ish
// so symbols uploaded later (e.g. new releases) aren't hidden forever.
const MISSING_SYMBOL_TTL_SECONDS = 60 * 60;
const MISSING_CACHE_CONTROL = `public, max-age=${MISSING_SYMBOL_TTL_SECONDS}`;
// Symbol files are immutable for a given debug-id path, so hits can be cached
// aggressively by CDNs/clients.
const HIT_CACHE_CONTROL = 'public, max-age=604800, immutable';

// Cap on concurrent proxied upstream requests. Beyond this we shed load
// immediately with a 503 instead of queueing, so the dyno's backlog stays
// shallow during floods.
const UPSTREAM_CONCURRENCY_LIMIT = parseInt(MAX_UPSTREAM_CONCURRENCY || '', 10) || 100;
const RETRY_AFTER_SECONDS = 30;

// Log roughly 1 in N requests. The service fields ~13M requests/day, so
// logging every one would swamp the log drain; a sampled line is enough to
// see which user agents are hitting us and how their requests are answered.
// Sampling uses a modulo counter rather than Math.random so the cadence is
// deterministic and testable.
const REQUEST_LOG_SAMPLE_RATE = parseInt(UA_LOG_SAMPLE_RATE || '', 10) || 1000;

const TARGET_URL = url.format({
  protocol: 'https:',
  slashes: true,
  host: TARGET_HOST,
});

const proxy = httpProxy.createProxyServer({
  changeOrigin: true,
});

const APPS_TO_ALIAS = ['slack', 'notion', 'notion dev', 'claude', 'claude nest'];

// temporary hack to handle apps that rename Electron / Electron Helper --> My App / My App Helper
// this should be removed once we have a proper solution for upstream crash
// servers to use.  We delibrately require this apps are prefixed by "/" or " " so
// that if the app name randomly appears in a SHA is won't break.
const REPLACEMENTS: [RegExp, string][] = [];
for (const appName of APPS_TO_ALIAS) {
  REPLACEMENTS.push([new RegExp(`/${appName}/`, 'g'), '/electron/']);
  REPLACEMENTS.push([new RegExp(`/${appName}%20`, 'g'), '/electron%20']);
  REPLACEMENTS.push([new RegExp(`/${appName}\\.`, 'g'), '/electron.']);
}

REPLACEMENTS.push([/\/c:\\projects\\src\\out\\default\\/g, '/']);
REPLACEMENTS.push([/\/c%3a%5cprojects%5csrc%5cout%5cdefault%5c/g, '/']);

// Bound the negative cache by total bytes rather than entry count so its
// worst-case memory footprint stays predictable on a small dyno. In
// lru-cache@6, providing a `length` calculator makes `max` a total-length
// budget. Charging only the key's string length badly undercounts real heap:
// each entry also costs an lru-cache linked-list node, a Map entry, and V8
// string/object headers. Measured with node --expose-gc on lru-cache@6 using
// representative 96-char keys filled to steady-state eviction: ~480-510 bytes
// of heapUsed per entry, i.e. roughly 384 bytes of overhead beyond the key
// itself. Charging key.length alone allowed ~350k entries and ~160 MiB of
// real heap against this 32 MiB budget; charging the measured overhead keeps
// a full cache at ~70k entries and ~34 MiB of measured heap.
const MISSING_CACHE_MAX_BYTES = 32 * 1024 * 1024;
const MISSING_CACHE_ENTRY_OVERHEAD_BYTES = 384;

const missingSymbolCache = new LRU<string, boolean>({
  max: MISSING_CACHE_MAX_BYTES,
  length: (_value, key) => (key as string).length + MISSING_CACHE_ENTRY_OVERHEAD_BYTES,
  maxAge: MISSING_SYMBOL_TTL_SECONDS * 1000,
});

// Proxied requests currently awaiting an upstream response, keyed by rewritten
// path. Used only to dedupe identical concurrent lookups — NOT for the
// concurrency cap: multiple proxied requests for the same path share a single
// map entry, so Map.size undercounts.
const inFlightRequests = new Map<string, Promise<void>>();

// Number of proxied upstream requests actually in flight right now. This is
// what the concurrency cap is enforced against.
let activeUpstreamRequests = 0;

// Ties each proxied downstream request to the upstream request http-proxy
// opens for it. The shared 'proxyReq' hook below fires for every proxied
// request, so it must map each proxyReq back to the right downstream request;
// keying by the incoming request object does that without any cleanup
// bookkeeping (entries die with the request).
interface UpstreamLifecycle {
  proxyReq: http.ClientRequest | null;
  downstreamGone: boolean;
  settle: () => void;
}

const upstreamLifecycles = new WeakMap<http.IncomingMessage, UpstreamLifecycle>();

// Cancel the outgoing upstream request. destroy() on a request that already
// completed just tears down its (connection: close) socket, but be defensive:
// a throw here would bubble into an event handler and kill nothing gracefully.
function abortUpstreamRequest(proxyReq: http.ClientRequest) {
  if (proxyReq.destroyed) return;
  try {
    proxyReq.destroy();
  } catch (err) {
    console.error('Failed to abort upstream request:', err);
  }
}

function incomingPathToProxyPath(path: string): string {
  // symstore.exe and symsrv.dll don't always agree on the case of the path to a
  // given symbol file. Since our artifact URLs are case-sensitive, this causes symbol
  // loads to fail. To get around this, we assume that the symbols were uploaded
  // to the artifact store with all-lowercase keys, and we lowercase all requests we receive to
  // match.
  let newPath = path.toLowerCase();

  // Some symbol servers send + instead of " "
  // this hacks around that for now
  newPath = newPath.replace(/%2b/g, '%20');
  newPath = newPath.replace(/\+/g, '%20');

  for (const replacement of REPLACEMENTS) {
    newPath = newPath.replace(replacement[0], replacement[1]);
  }

  // The symbols may be hosted a deeper path in the artifact store
  // so we prefix the incoming path with that prefix
  return `${PATH_PREFIX || ''}${newPath}`;
}

proxy.on('proxyReq', (proxyReq, request, response, options) => {
  proxyReq.path = incomingPathToProxyPath(proxyReq.path);

  // AZ CDN determines the bucket from the Host header
  proxyReq.setHeader('Host', TARGET_HOST);
  
  response.setHeader('Access-Control-Allow-Origin', '*');
  response.setHeader('Access-Control-Allow-Methods', 'GET');

  // AZ CDN returns 403 errors for containers that don't exist. But when symsrv.dll sees a
  // 403 it blacklists the server for the rest of the debugging session. So we
  // convert 403s to 404s so symsrv.dll doesn't freak out.
  const originalWriteHead = response.writeHead;
  response.writeHead = (...args: [number, any]) => {
    if (args[0] == 403) {
      // Only genuine misses go in the negative cache. Hits and transport
      // errors used to be stored as `false`, which answered no query the
      // cache's absence wouldn't, but still consumed an LRU slot each.
      missingSymbolCache.set(proxyReq.path, true);
      args[0] = 404;
      response.setHeader('Cache-Control', MISSING_CACHE_CONTROL);
    } else if (args[0] == 200 && !response.getHeader('cache-control')) {
      response.setHeader('Cache-Control', HIT_CACHE_CONTROL);
    }
    return originalWriteHead.apply(response, args);
  };

  const lifecycle = upstreamLifecycles.get(request);
  if (lifecycle) {
    lifecycle.proxyReq = proxyReq;
    // The upstream slot and dedup promise settle on the UPSTREAM request's
    // lifecycle, not the downstream response's: 'close' fires both when the
    // proxied response has been fully read and when the request is destroyed
    // or errors, so normal completion and client-abort cancellation route to
    // the same idempotent settle.
    proxyReq.on('close', lifecycle.settle);
    // The client may have vanished between proxy.web() and the socket
    // assignment that fires this event; cancel the upstream work right away.
    if (lifecycle.downstreamGone) abortUpstreamRequest(proxyReq);
  }
});

proxy.on('error', (err, req, res) => {
  const errorId = crypto.randomUUID();

  console.error('Error:', errorId, 'Request:', req.url, err);

  // A deliberately canceled upstream request (client disconnected mid-proxy)
  // can surface its teardown error here; there is no one left to answer and
  // writing headers to a closed/finished response would throw.
  if (res.destroyed || res.writableEnded || res.headersSent) return;

  res.writeHead(500, {
    'Content-Type': 'text/plain'
  });
 
  res.end(`Something went wrong. If this happens consistently please report to https://github.com/electron/symbol-server with this error ID: "${errorId}"`);
});

// Called once per request at the point where its disposition (redirect,
// cached-404, shed, proxied) becomes cheaply known; every
// REQUEST_LOG_SAMPLE_RATE-th call emits a single greppable line. The
// User-Agent is stripped of newlines and has quotes escaped so the ua="..."
// field always stays on one line and cannot be broken out of.
let sampledRequestCount = 0;

function sampleRequestLog(req: http.IncomingMessage, disposition: string) {
  if (sampledRequestCount++ % REQUEST_LOG_SAMPLE_RATE !== 0) return;
  const userAgent = req.headers['user-agent'];
  const ua = userAgent ? userAgent.replace(/[\r\n]+/g, ' ').replace(/"/g, '\\"') : '-';
  console.log(`request-sample method=${req.method} path=${req.url} disposition=${disposition} ua="${ua}"`);
}

http.createServer((req, res) => {
  const parsed = new url.URL(`http://localhost${req.url!}`);
  if (parsed.pathname === '/health') {
    return res.writeHead(200).end('Alive');
  }

  const cacheKey = incomingPathToProxyPath(parsed.pathname + parsed.search);
  const userAgent = req.headers['user-agent'];
  const isSentryRequest = userAgent && userAgent.startsWith('symbolicator/');

  if (isSentryRequest || req.headers['x-electron-symbol-redirect'] === '1') {
    sampleRequestLog(req, 'redirect');
    res.setHeader('Location', url.format({
      protocol: 'https:',
      slashes: true,
      host: TARGET_HOST,
      pathname: cacheKey,
    }));
    res.setHeader('Cache-Control', 'no-store');
    return res.writeHead(302).end();
  }

  if (missingSymbolCache.get(cacheKey)) {
    sampleRequestLog(req, 'cached-404');
    return res.writeHead(404, { 'Cache-Control': MISSING_CACHE_CONTROL }).end();
  }

  // If an identical lookup is already being proxied, wait for it to settle
  // rather than launching a duplicate upstream fetch. If it negative-cached
  // the path we can answer 404 for free, otherwise proxy as usual —
  // proxyToUpstream re-checks the concurrency cap when we wake, so a stampede
  // of same-path waiters is shed instead of all proxying at once.
  const inFlight = inFlightRequests.get(cacheKey);
  if (inFlight) {
    inFlight.then(() => {
      // The client may have hung up while we waited on the leader. Its
      // response 'close' event has already fired by now, so proxying would
      // register settle listeners that never run and leak an upstream slot
      // forever (proxyToUpstream re-checks this, but bail early and skip the
      // cache lookup too). Dropped waiters touch no counters, so there is
      // nothing to clean up.
      if (clientGone(req, res)) return;
      if (missingSymbolCache.get(cacheKey)) {
        sampleRequestLog(req, 'cached-404');
        return res.writeHead(404, { 'Cache-Control': MISSING_CACHE_CONTROL }).end();
      }
      proxyToUpstream(req, res, cacheKey);
    });
    return;
  }

  proxyToUpstream(req, res, cacheKey);
}).listen(process.env.PORT || 8080);

// True when the client that issued this request can no longer receive a
// response: its socket is gone (disconnect — note 'close' fires on the
// response even before headers are written) or the response already ended.
function clientGone(req: http.IncomingMessage, res: http.ServerResponse): boolean {
  return (
    req.destroyed ||
    res.destroyed ||
    res.writableEnded ||
    !res.socket ||
    res.socket.destroyed
  );
}

function proxyToUpstream(req: http.IncomingMessage, res: http.ServerResponse, cacheKey: string) {
  // Never proxy on behalf of a client that already disconnected: its 'close'
  // event has already fired, so the settle listeners below would never run
  // and the upstream slot would leak until process restart.
  if (clientGone(req, res)) return;

  if (activeUpstreamRequests >= UPSTREAM_CONCURRENCY_LIMIT) {
    // Shed load immediately instead of queueing behind a saturated upstream,
    // otherwise the router backlog fills up and everyone gets H11 503s.
    sampleRequestLog(req, 'shed');
    res.setHeader('Retry-After', String(RETRY_AFTER_SECONDS));
    return res.writeHead(503).end('Too many concurrent symbol requests, retry later');
  }

  activeUpstreamRequests++;
  // settle() can be reached from several events (the upstream request's
  // 'close', the downstream 'close'/'error' fallback below, and by hand when
  // the client disconnected before the listeners were registered); settle
  // exactly once so the active count can never be decremented twice.
  let settled = false;
  let settle!: () => void;
  const inFlight = new Promise<void>((resolve) => {
    settle = () => {
      if (settled) return;
      settled = true;
      activeUpstreamRequests--;
      // Several proxied requests for the same path can coexist (dedup waiters
      // that woke below the cap); only the one registered in the map may
      // remove the entry, or a later request's dedup entry would be dropped
      // while it is still in flight.
      if (inFlightRequests.get(cacheKey) === inFlight) {
        inFlightRequests.delete(cacheKey);
      }
      resolve();
    };
  });

  // Tie teardown to the actual upstream request rather than the downstream
  // response alone: http-proxy does not cancel the outgoing request by itself
  // when the client disconnects mid-proxy (its req 'aborted' hook never fires
  // on modern Node for requests whose body was already fully received, i.e.
  // every GET), so settling on downstream 'close' freed the slot and woke
  // dedup waiters while the upstream fetch was still running — bypassing the
  // cap. Instead, downstream 'close'/'error' destroys the upstream request,
  // and the slot/dedup promise settle only once that request has ended or
  // been aborted (its 'close' listener, registered in the proxyReq hook), so
  // waiters can never wake into a still-occupied slot.
  const lifecycle: UpstreamLifecycle = { proxyReq: null, downstreamGone: false, settle };
  upstreamLifecycles.set(req, lifecycle);
  const onDownstreamGone = () => {
    if (lifecycle.downstreamGone) return;
    lifecycle.downstreamGone = true;
    if (lifecycle.proxyReq) {
      // Cancel the upstream work; settle fires when the destroyed request
      // emits 'close'. (After a normal completion this destroy is a no-op on
      // an already-finished request.)
      abortUpstreamRequest(lifecycle.proxyReq);
    } else {
      // No upstream request was captured for this response — either we never
      // reached proxy.web below, or http-proxy skipped the proxyReq event
      // (it does for Expect: 100-continue requests). Nothing to cancel;
      // settle now so the slot cannot leak. Should the capture still happen a
      // tick later, the downstreamGone flag above makes it destroy the
      // upstream request immediately.
      settle();
    }
  };
  res.on('close', onDownstreamGone);
  res.on('error', onDownstreamGone);

  if (!inFlightRequests.has(cacheKey)) {
    inFlightRequests.set(cacheKey, inFlight);
  }

  // 'close' fires at most once. If the client vanished between the clientGone
  // check at the top of this function and the listener registration above, it
  // has already fired and never will again — settle by hand and skip the
  // upstream fetch entirely.
  if (clientGone(req, res)) {
    settle();
    return;
  }

  sampleRequestLog(req, 'proxied');
  proxy.web(req, res, { target: TARGET_URL });
}

process.on('uncaughtException', (err) => {
  // Avoid process dieing on uncaughtException
  console.error(err);
});
