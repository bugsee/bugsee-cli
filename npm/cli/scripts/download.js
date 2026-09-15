// Install-time HTTP helper for scripts/postinstall.js. Not part of the runtime
// surface — lib/index.js never requires it.
//
// Deliberately dependency-free (node:https only). The front package having
// zero runtime dependencies is the reason it can be installed from a locked-
// down mirror at all, and a postinstall that needed `node-fetch` would defeat
// the fallback it exists to provide. Redirect following is required because
// GitHub release downloads 302 to objects.githubusercontent.com; proxy support
// is required because the CI boxes that hit the fallback path (no access to
// the platform scopes) are usually the ones behind a proxy.

"use strict";

const http = require("node:http");
const https = require("node:https");

// How many redirect hops we will FOLLOW. GitHub release downloads take two
// (release -> objects.githubusercontent.com -> the CDN), so 5 is generous.
const MAX_REDIRECTS = 5;

// HTTPS only, with one narrow exception. The artifact and its `.sha256`
// sidecar come from the same base URL, so anyone who can rewrite a cleartext
// hop controls BOTH and the checksum verification proves nothing. This is the
// same stance installer/install.sh takes with `curl --proto '=https'
// --tlsv1.2`. The exception is a loopback host, which exists so a local mirror
// (and this repo's own checksum-mismatch test) can be served over plain HTTP
// without a certificate; loopback cannot be intercepted by a network attacker.
function isLoopback(hostname) {
  const host = hostname.toLowerCase().replace(/^\[/, "").replace(/\]$/, "");
  return (
    host === "localhost" ||
    host === "::1" ||
    host === "0:0:0:0:0:0:0:1" ||
    /^127(?:\.\d{1,3}){3}$/.test(host)
  );
}

function proxyFor(urlString) {
  const url = new URL(urlString);
  const isHttps = url.protocol === "https:";

  const noProxy = process.env.NO_PROXY || process.env.no_proxy || "";
  if (noProxy === "*") return null;
  if (noProxy) {
    const host = url.hostname.toLowerCase();
    for (const raw of noProxy.split(",")) {
      const entry = raw.trim().toLowerCase();
      if (entry && (host === entry || host.endsWith("." + entry))) return null;
    }
  }

  const env = isHttps
    ? process.env.HTTPS_PROXY || process.env.https_proxy
    : process.env.HTTP_PROXY || process.env.http_proxy;
  if (!env) return null;

  const proxy = new URL(env);
  return {
    hostname: proxy.hostname,
    port: proxy.port || (proxy.protocol === "https:" ? 443 : 80),
    auth:
      proxy.username || proxy.password
        ? `${decodeURIComponent(proxy.username)}:${decodeURIComponent(proxy.password)}`
        : null,
  };
}

function connectThroughProxy(proxy, target) {
  return new Promise((resolve, reject) => {
    const headers = {};
    if (proxy.auth) {
      headers["Proxy-Authorization"] =
        "Basic " + Buffer.from(proxy.auth).toString("base64");
    }
    const req = http.request({
      hostname: proxy.hostname,
      port: proxy.port,
      method: "CONNECT",
      path: `${target.hostname}:${target.port || 443}`,
      headers,
    });
    req.on("connect", (res, socket) => {
      if (res.statusCode === 200) resolve(socket);
      else
        reject(new Error(`proxy CONNECT failed with status ${res.statusCode}`));
    });
    req.on("error", reject);
    req.end();
  });
}

/**
 * GET a URL, following redirects; resolves with the response stream.
 *
 * `httpsOnly` is latched on the FIRST hop and carried through every redirect,
 * so an https origin can never be walked down to cleartext by a 302 — without
 * that, re-checking each hop independently would happily follow
 * `https://mirror/... -> http://attacker/...`.
 */
function get(urlString, redirectsFollowed = 0, httpsOnly = null) {
  return new Promise((resolve, reject) => {
    if (redirectsFollowed > MAX_REDIRECTS) {
      return reject(new Error(`too many redirects (> ${MAX_REDIRECTS})`));
    }

    const parsed = new URL(urlString);
    const isHttps = parsed.protocol === "https:";
    if (!isHttps && parsed.protocol !== "http:") {
      return reject(new Error(`unsupported protocol: ${parsed.protocol}`));
    }
    const secureRequired = httpsOnly === null ? isHttps : httpsOnly;
    if (!isHttps) {
      if (secureRequired) {
        return reject(
          new Error(
            `refusing to follow an https -> http redirect: ${urlString}`,
          ),
        );
      }
      if (!isLoopback(parsed.hostname)) {
        return reject(
          new Error(
            `refusing to fetch over plain HTTP from a non-loopback host: ${urlString}`,
          ),
        );
      }
    }
    const mod = isHttps ? https : http;
    const proxy = proxyFor(urlString);

    const send = (extra) => {
      const options = Object.assign(
        {
          hostname: parsed.hostname,
          port: parsed.port || (isHttps ? 443 : 80),
          path: parsed.pathname + parsed.search,
          method: "GET",
          headers: { "User-Agent": "bugsee-cli-npm-installer" },
          timeout: 60_000,
        },
        extra || {},
      );

      if (proxy && !isHttps) {
        // Plain HTTP through an HTTP proxy: absolute-URI request line.
        options.hostname = proxy.hostname;
        options.port = proxy.port;
        options.path = urlString;
        if (proxy.auth) {
          options.headers["Proxy-Authorization"] =
            "Basic " + Buffer.from(proxy.auth).toString("base64");
        }
      }

      const req = mod.request(options, (res) => {
        const { statusCode, headers } = res;
        if (statusCode >= 300 && statusCode < 400 && headers.location) {
          res.resume();
          const next = new URL(headers.location, urlString).toString();
          return get(next, redirectsFollowed + 1, secureRequired).then(
            resolve,
            reject,
          );
        }
        if (statusCode < 200 || statusCode >= 300) {
          res.resume();
          return reject(new Error(`HTTP ${statusCode} from ${urlString}`));
        }
        resolve(res);
      });
      req.on("timeout", () =>
        req.destroy(new Error(`timed out: ${urlString}`)),
      );
      req.on("error", reject);
      req.end();
    };

    if (proxy && isHttps) {
      connectThroughProxy(proxy, parsed).then(
        (socket) => send({ socket, agent: false }),
        reject,
      );
    } else {
      send();
    }
  });
}

/** GET a small URL fully into a string (used for the .sha256 sidecar). */
async function getText(urlString) {
  const res = await get(urlString);
  const chunks = [];
  for await (const chunk of res) chunks.push(chunk);
  return Buffer.concat(chunks).toString("utf8");
}

module.exports = { get, getText };
