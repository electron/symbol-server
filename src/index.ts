import assert from "assert";
import * as crypto from "crypto";
import * as fs from "fs-extra";
import * as http from "http";
import httpProxy from "http-proxy";
import * as path from "path";
import * as url from "url";
import * as uuid from "uuid";

const CACHE_DIR = path.resolve(__dirname, "..", ".cache");
// Cache misses for 15 minutes
const S3_MISS_TTL = 15 * 60 * 1000;
// Cache hits for 10 hours
const S3_HIT_TTL = 10 * 60 * 60 * 1000;

const MAX_ENTRIES_IN_CACHE = 400;

const { PATH_PREFIX, S3_BUCKET } = process.env;

assert(S3_BUCKET, "S3_BUCKET is defined");

const TARGET_HOST = `${S3_BUCKET}.s3.amazonaws.com`;

const TARGET_URL = url.format({
  protocol: "https:",
  slashes: true,
  host: TARGET_HOST,
});

const proxy = httpProxy.createProxyServer({
  changeOrigin: true,
});

const APPS_TO_ALIAS = ["slack"];

// temporary hack to handle apps that rename Electron / Electron Helper --> My App / My App Helper
// this should be removed once we have a proper solution for upstream crash
// servers to use.  We delibrately require this apps are prefixed by "/" or " " so
// that if the app name randomly appears in a SHA is won't break.
const REPLACEMENTS: [RegExp, string][] = [];
for (const appName of APPS_TO_ALIAS) {
  REPLACEMENTS.push([new RegExp(`/${appName}/`, "g"), "/electron/"]);
  REPLACEMENTS.push([new RegExp(`/${appName}%20`, "g"), "/electron%20"]);
  REPLACEMENTS.push([new RegExp(`/${appName}\\.`, "g"), "/electron."]);
}

if (fs.existsSync(CACHE_DIR)) {
  fs.removeSync(CACHE_DIR);
}
fs.mkdirpSync(CACHE_DIR);

function normalizeRequestPath(currentPath: string) {
  // symstore.exe and symsrv.dll don't always agree on the case of the path to a
  // given symbol file. Since S3 URLs are case-sensitive, this causes symbol
  // loads to fail. To get around this, we assume that the symbols were uploaded
  // to S3 with all-lowercase keys, and we lowercase all requests we receive to
  // match.
  let newPath = currentPath.toLowerCase();

  // Some symbol servers send + instead of " "
  // this hacks around that for now
  newPath = newPath.replace(/%2b/g, "%20");
  newPath = newPath.replace(/\+/g, "%20");

  for (const replacement of REPLACEMENTS) {
    newPath = newPath.replace(replacement[0], replacement[1]);
  }

  // The symbols may be hosted a deeper path in the S3 bucket
  // so we prefix the incoming path with that prefix
  return `${PATH_PREFIX || ""}${newPath}`;
}

const diskCacheTTLs: Record<string, number> = {};

function getCacheKey(currentPath: string) {
  const cacheKey = normalizeRequestPath(currentPath);
  const hashedKey = crypto
    .createHash("sha256")
    .update(cacheKey)
    .digest("hex")
    .toLowerCase();
  return hashedKey;
}

function getCachePath(cacheKey: string) {
  return path.resolve(CACHE_DIR, cacheKey);
}

function getHeadersCachePath(cacheKey: string) {
  return path.resolve(CACHE_DIR, `${cacheKey}.headers`);
}

function respondWithCacheOrCall(
  req: http.IncomingMessage,
  res: http.ServerResponse,
  miss: () => void
) {
  if (!req.url) return miss();
  const parsedUrl = url.parse(req.url);
  if (!parsedUrl.pathname) return miss();

  const cacheKey = getCacheKey(parsedUrl.pathname);
  const ttl = diskCacheTTLs[cacheKey];

  // No TTL saved or
  if (!ttl) return miss();

  const cachePath = getCachePath(cacheKey);
  const headersCachePath = getHeadersCachePath(cacheKey);

  // Cache entry has expired
  if (ttl <= Date.now()) {
    // Kill cache entry and call it a miss
    return fs.remove(cachePath, () => {
      delete diskCacheTTLs[cacheKey];
      miss();
    });
  }

  fs.exists(cachePath, (exists) => {
    if (!exists) {
      delete diskCacheTTLs[cacheKey];
      return miss();
    }

    fs.readJson(headersCachePath, (err, headers) => {
      if (err) {
        delete diskCacheTTLs[cacheKey];
        return miss();
      }

      const { status } = headers;
      for (const headerKey in headers) {
        res.setHeader(headerKey, headers[headerKey]);
      }
      res.statusCode = status;

      fs.createReadStream(cachePath).pipe(res);
    });
  });
}

proxy.on("proxyReq", (proxyReq, request, response, options) => {
  proxyReq.path = normalizeRequestPath(proxyReq.path);

  // S3 determines the bucket from the Host header
  proxyReq.setHeader("Host", TARGET_HOST);

  // const originalWriteHead = response.writeHead;
  // response.writeHead = (...args: [number, any]) => {
  //   if (args[0] == 403)
  //     args[0] = 404;
  //   return originalWriteHead.apply(response, args);
  // };
});

proxy.on("proxyRes", (proxyRes, req) => {
  // S3 returns 403 errors for files that don't exist. But when symsrv.dll sees a
  // 403 it blacklists the server for the rest of the debugging session. So we
  // convert 403s to 404s so symsrv.dll doesn't freak out.
  proxyRes.statusCode = proxyRes.statusCode === 403 ? 404 : proxyRes.statusCode;

  if (Object.keys(diskCacheTTLs).length > MAX_ENTRIES_IN_CACHE) return;

  if (!req.url) return;
  const parsedUrl = url.parse(req.url);
  if (!parsedUrl.pathname) return;

  const cacheKey = getCacheKey(parsedUrl.pathname);
  const cachePath = getCachePath(cacheKey);
  const headersCachePath = getHeadersCachePath(cacheKey);

  let written = 0;
  const finalize = () => {
    written++;
    if (written === 2) {
      diskCacheTTLs[cacheKey] =
        Date.now() + (proxyRes.statusCode === 200 ? S3_HIT_TTL : S3_MISS_TTL);
    }
  };

  const writeStream = fs.createWriteStream(cachePath);
  proxyRes.pipe(writeStream);
  writeStream.on("close", finalize);

  fs.writeJSON(
    headersCachePath,
    { ...proxyRes.headers, status: proxyRes.statusCode },
    (err) => {
      if (!err) finalize();
    }
  );
});

proxy.on("error", (err, req, res) => {
  const errorId = uuid.v4();

  console.error("Error:", errorId, "Request:", req.url, err);

  res.writeHead(500, {
    "Content-Type": "text/plain",
  });

  res.end(
    `Something went wrong. If this happens consistently please report to https://github.com/electron/symbol-server with this error ID: "${errorId}"`
  );
});

http
  .createServer((req, res) => {
    respondWithCacheOrCall(req, res, () => {
      proxy.web(req, res, { target: TARGET_URL });
    });
  })
  .listen(process.env.PORT || 8080);

process.on("uncaughtException", (err) => {
  // Avoid process dieing on uncaughtException
  console.error(err);
});
