import assert from 'assert';
import * as crypto from 'crypto';
import * as http from 'http';
import httpProxy from 'http-proxy';
import LRU from 'lru-cache';
import * as url from 'url';

const { PATH_PREFIX, TARGET_HOST, MAX_UPSTREAM_CONCURRENCY } = process.env;

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
// budget; we charge each entry the string length of its path key (the boolean
// value is negligible). Rewritten symbol paths are ~60-100 bytes, so 32 MiB is
// roughly 300-500k entries worst case, but bounded in bytes either way.
const MISSING_CACHE_MAX_BYTES = 32 * 1024 * 1024;

const missingSymbolCache = new LRU<string, boolean>({
  max: MISSING_CACHE_MAX_BYTES,
  length: (_value, key) => (key as string).length,
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
});

proxy.on('error', (err, req, res) => {
  const errorId = crypto.randomUUID();

  console.error('Error:', errorId, 'Request:', req.url, err);

  res.writeHead(500, {
    'Content-Type': 'text/plain'
  });
 
  res.end(`Something went wrong. If this happens consistently please report to https://github.com/electron/symbol-server with this error ID: "${errorId}"`);
});

http.createServer((req, res) => {
  const parsed = new url.URL(`http://localhost${req.url!}`);
  if (parsed.pathname === '/health') {
    return res.writeHead(200).end('Alive');
  }

  const cacheKey = incomingPathToProxyPath(parsed.pathname + parsed.search);
  const userAgent = req.headers['user-agent'];
  const isSentryRequest = userAgent && userAgent.startsWith('symbolicator/');

  if (isSentryRequest || req.headers['x-electron-symbol-redirect'] === '1') {
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
      if (missingSymbolCache.get(cacheKey)) {
        return res.writeHead(404, { 'Cache-Control': MISSING_CACHE_CONTROL }).end();
      }
      proxyToUpstream(req, res, cacheKey);
    });
    return;
  }

  proxyToUpstream(req, res, cacheKey);
}).listen(process.env.PORT || 8080);

function proxyToUpstream(req: http.IncomingMessage, res: http.ServerResponse, cacheKey: string) {
  if (activeUpstreamRequests >= UPSTREAM_CONCURRENCY_LIMIT) {
    // Shed load immediately instead of queueing behind a saturated upstream,
    // otherwise the router backlog fills up and everyone gets H11 503s.
    res.setHeader('Retry-After', String(RETRY_AFTER_SECONDS));
    return res.writeHead(503).end('Too many concurrent symbol requests, retry later');
  }

  activeUpstreamRequests++;
  const inFlight = new Promise<void>((resolve) => {
    // Both 'close' and 'error' can fire for the same response; settle exactly
    // once so the active count can never be decremented twice.
    let settled = false;
    const settle = () => {
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
    res.on('close', settle);
    res.on('error', settle);
  });

  if (!inFlightRequests.has(cacheKey)) {
    inFlightRequests.set(cacheKey, inFlight);
  }

  proxy.web(req, res, { target: TARGET_URL });
}

process.on('uncaughtException', (err) => {
  // Avoid process dieing on uncaughtException
  console.error(err);
});
