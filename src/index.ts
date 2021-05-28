import assert from 'assert';
import * as http from 'http';
import httpProxy from 'http-proxy';
import * as url from 'url';
import * as uuid from 'uuid';

const { PATH_PREFIX, S3_BUCKET } = process.env;

assert(S3_BUCKET, 'S3_BUCKET is defined');

const TARGET_HOST = `${S3_BUCKET}.s3.amazonaws.com`;

const TARGET_URL = url.format({
  protocol: 'https:',
  slashes: true,
  host: TARGET_HOST,
});

const proxy = httpProxy.createProxyServer({
  changeOrigin: true,
});

const APPS_TO_ALIAS = ['slack'];

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

proxy.on('proxyReq', (proxyReq, request, response, options) => {
  // symstore.exe and symsrv.dll don't always agree on the case of the path to a
  // given symbol file. Since S3 URLs are case-sensitive, this causes symbol
  // loads to fail. To get around this, we assume that the symbols were uploaded
  // to S3 with all-lowercase keys, and we lowercase all requests we receive to
  // match.
  let newPath = proxyReq.path.toLowerCase()

  // Some symbol servers send + instead of " "
  // this hacks around that for now
  newPath = newPath.replace(/%2b/g, '%20');
  newPath = newPath.replace(/\+/g, '%20');

  for (const replacement of REPLACEMENTS) {
    newPath = newPath.replace(replacement[0], replacement[1]);
  }

  // The symbols may be hosted a deeper path in the S3 bucket
  // so we prefix the incoming path with that prefix
  proxyReq.path = `${PATH_PREFIX || ''}${newPath}`;

  // S3 determines the bucket from the Host header
  proxyReq.setHeader('Host', TARGET_HOST);
  
  response.setHeader('Access-Control-Allow-Origin', '*');
  response.setHeader('Access-Control-Allow-Methods', 'GET');

  // S3 returns 403 errors for files that don't exist. But when symsrv.dll sees a
  // 403 it blacklists the server for the rest of the debugging session. So we
  // convert 403s to 404s so symsrv.dll doesn't freak out.
  const originalWriteHead = response.writeHead;
  response.writeHead = (...args: [number, any]) => {
    if (args[0] == 403)
      args[0] = 404;
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
  proxy.web(req, res, { target: TARGET_URL });
}).listen(process.env.PORT || 8080);

process.on('uncaughtException', (err) => {
  // Avoid process dieing on uncaughtException
  console.error(err);
});
