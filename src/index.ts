import assert from 'assert';
import * as http from 'http';
import httpProxy from 'http-proxy';
import LRU from 'lru-cache';
import * as url from 'url';
import * as uuid from 'uuid';

const { PATH_PREFIX, TARGET_HOST } = process.env;

assert(TARGET_HOST, 'TARGET_HOST is defined');

const TARGET_URL = url.format({
  protocol: 'https:',
  slashes: true,
  host: TARGET_HOST,
});

const proxy = httpProxy.createProxyServer({
  changeOrigin: true,
});

const APPS_TO_ALIAS = ['slack', 'notion', 'notion dev'];

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

const missingSymbolCache = new LRU<string, boolean>({
  max: 10000,
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
      missingSymbolCache.set(proxyReq.path, true);
      args[0] = 404;
    } else {
      missingSymbolCache.set(proxyReq.path, false);
    }
    return originalWriteHead.apply(response, args);
  };
});

proxy.on('error', (err, req, res) => {
  const errorId = uuid.v4();

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
    return res.writeHead(302).end();
  }

  if (missingSymbolCache.get(cacheKey)) {
    return res.writeHead(404).end();
  }

  proxy.web(req, res, { target: TARGET_URL });
}).listen(process.env.PORT || 8080);

process.on('uncaughtException', (err) => {
  // Avoid process dieing on uncaughtException
  console.error(err);
});
