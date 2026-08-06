'use strict';

const { spawn } = require('child_process');
const fs = require('fs');
const http = require('http');
const https = require('https');
const net = require('net');
const path = require('path');

const SERVER_ENTRY = path.join(__dirname, '..', 'lib', 'index.js');
const TLS_CERT = fs.readFileSync(path.join(__dirname, 'fixtures', 'test-cert.pem'));
const TLS_KEY = fs.readFileSync(path.join(__dirname, 'fixtures', 'test-key.pem'));

const LISTEN_TIMEOUT_MS = 5000;
const LISTEN_POLL_MS = 50;
const SIGKILL_GRACE_MS = 2000;

function getFreePort() {
  return new Promise((resolve, reject) => {
    const server = net.createServer();
    server.on('error', reject);
    server.listen(0, '127.0.0.1', () => {
      const { port } = server.address();
      server.close(() => resolve(port));
    });
  });
}

function waitForListening(port) {
  const deadline = Date.now() + LISTEN_TIMEOUT_MS;
  return new Promise((resolve, reject) => {
    const tryConnect = () => {
      const socket = net.connect(port, '127.0.0.1');
      socket.once('connect', () => {
        socket.end();
        resolve();
      });
      socket.once('error', () => {
        socket.destroy();
        if (Date.now() > deadline) {
          reject(new Error(`Port ${port} did not open within ${LISTEN_TIMEOUT_MS}ms`));
        } else {
          setTimeout(tryConnect, LISTEN_POLL_MS);
        }
      });
    };
    tryConnect();
  });
}

// Fake upstream HTTPS server. Calls handler(req, res) for each request and
// records the request paths it received so tests can assert on them.
function startUpstream(handler) {
  return new Promise((resolve, reject) => {
    const requests = [];
    const server = https.createServer({ cert: TLS_CERT, key: TLS_KEY }, (req, res) => {
      requests.push({
        url: req.url,
        method: req.method,
        headers: req.headers,
      });
      handler(req, res);
    });
    server.on('error', reject);
    server.listen(0, '127.0.0.1', () => {
      const { port } = server.address();
      resolve({
        host: `127.0.0.1:${port}`,
        port,
        requests,
        close: () =>
          new Promise((res) => {
            server.close(() => res());
          }),
      });
    });
  });
}

async function startSymbolServer({ targetHost, pathPrefix } = {}) {
  const port = await getFreePort();

  const env = {
    ...process.env,
    TARGET_HOST: targetHost,
    PORT: String(port),
    // http-proxy uses the default https agent; NODE_EXTRA_CA_CERTS is the
    // out-of-band way to trust the upstream's self-signed cert without
    // modifying the symbol-server source.
    NODE_EXTRA_CA_CERTS: path.join(__dirname, 'fixtures', 'test-cert.pem'),
  };
  if (pathPrefix !== undefined) env.PATH_PREFIX = pathPrefix;
  else delete env.PATH_PREFIX;

  const stderrChunks = [];
  const child = spawn(process.execPath, [SERVER_ENTRY], { env });
  child.stdout.on('data', () => {});
  child.stderr.on('data', (d) => stderrChunks.push(d));

  let exited = false;
  const exitPromise = new Promise((resolve) => {
    child.on('exit', (code) => {
      exited = true;
      const stderr = Buffer.concat(stderrChunks).toString();
      resolve(new Error(
        `Symbol server exited with code ${code} before listening:\n${stderr}`,
      ));
    });
  });

  try {
    await Promise.race([
      waitForListening(port),
      exitPromise.then((err) => { throw err; }),
    ]);
  } catch (err) {
    if (!exited) child.kill('SIGKILL');
    throw err;
  }

  return {
    port,
    stop: () =>
      new Promise((resolve) => {
        if (exited) return resolve();
        child.once('exit', () => resolve());
        child.kill('SIGTERM');
        setTimeout(() => {
          if (!exited) child.kill('SIGKILL');
        }, SIGKILL_GRACE_MS).unref();
      }),
  };
}

// Spawn an upstream + symbol-server pair and register cleanup with the test
// context. Returns { server, upstream }.
async function startProxy(t, { handler, pathPrefix } = {}) {
  const upstream = await startUpstream(handler || ((req, res) => {
    res.writeHead(200);
    res.end('ok');
  }));
  t.after(() => upstream.close());

  const server = await startSymbolServer({ targetHost: upstream.host, pathPrefix });
  t.after(() => server.stop());

  return { server, upstream };
}

function request(port, requestPath, headers = {}) {
  return new Promise((resolve, reject) => {
    const req = http.request(
      { host: '127.0.0.1', port, path: requestPath, method: 'GET', headers },
      (res) => {
        const chunks = [];
        res.on('data', (chunk) => chunks.push(chunk));
        res.on('end', () => {
          resolve({
            statusCode: res.statusCode,
            headers: res.headers,
            body: Buffer.concat(chunks).toString(),
          });
        });
      },
    );
    req.on('error', reject);
    req.end();
  });
}

module.exports = {
  startUpstream,
  startSymbolServer,
  startProxy,
  request,
};
