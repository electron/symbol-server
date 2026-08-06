import assert from 'assert';
import * as crypto from 'crypto';
import * as http from 'http';
import httpProxy from 'http-proxy';
import LRU from 'lru-cache';
import * as url from 'url';

const { PATH_PREFIX, TARGET_HOST } = process.env;

assert(TARGET_HOST, 'TARGET_HOST is defined');

// How long a "this symbol does not exist" answer stays valid, both in our
// in-memory cache and in any CDN/client honoring Cache-Control. Kept short-ish
// so symbols uploaded later (e.g. new releases) aren't hidden forever.
const MISSING_SYMBOL_TTL_SECONDS = 60 * 60;
const MISSING_CACHE_CONTROL = `public, max-age=${MISSING_SYMBOL_TTL_SECONDS}`;
// Symbol files are immutable for a given debug-id path, so hits can be cached
// aggressively by CDNs/clients.
const HIT_CACHE_CONTROL = 'public, max-age=604800, immutable';

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

  // The client may already be gone (disconnected mid-proxy) by the time the
  // upstream request fails; writing headers to a closed/finished response
  // would throw.
  if (res.destroyed || res.writableEnded || res.headersSent) return;

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
    // Cloudflare caches these 302s at the edge per UA cohort; Location depends only on the path.
    res.setHeader('Cache-Control', MISSING_CACHE_CONTROL);
    return res.writeHead(302).end();
  }

  if (missingSymbolCache.get(cacheKey)) {
    return res.writeHead(404, { 'Cache-Control': MISSING_CACHE_CONTROL }).end();
  }

  proxy.web(req, res, { target: TARGET_URL });
}).listen(process.env.PORT || 8080);

process.on('uncaughtException', (err) => {
  // Avoid process dieing on uncaughtException
  console.error(err);
});
