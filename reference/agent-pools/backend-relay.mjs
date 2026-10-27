import net from "node:net";
import tls from "node:tls";
import path from "node:path";
import { readFile } from "node:fs/promises";
import { Readable } from "node:stream";

// ChatGPT backend relay behind both desktop listeners. Everything under
// /backend-api/ is forwarded to the upstream with the caller's own credentials
// and streamed back; only GET /wham/usage is handed to rewriteUsage so the
// desktop banner and usage panel reflect the pool. WebSocket upgrades (remote
// control host and client links, dictation) are tunneled raw: the handshake is
// forwarded, then bytes are piped both ways.
export const BACKEND_PREFIX = "/backend-api";
export const MAX_BODY_BYTES = 32 * 1024 * 1024;
export const CONNECT_TIMEOUT_MS = 15000;
const HOP_HEADERS = new Set(["host", "connection", "content-length", "transfer-encoding", "accept-encoding", "keep-alive", "proxy-connection", "te", "upgrade"]);
const DROPPED_RESPONSE_HEADERS = new Set(["content-encoding", "content-length", "transfer-encoding", "connection"]);

export const isBackendPath = (pathname) => pathname === BACKEND_PREFIX || pathname.startsWith(BACKEND_PREFIX + "/");

// TLS material for the desktop relay listener, written by Set-Desktop-Relay.ps1
// -Enable: a leaf for localhost issued by a local CA the user trusts. Codex
// 0.155 accepts only an https chatgpt_base_url (workspace routing discovery),
// and the Electron process then routes its own backend calls to that origin,
// so the listener must speak TLS. Both files are required; with either missing
// the caller falls back to plain HTTP.
export async function loadRelayTls(dir, { certFile = "desktop-relay.crt", keyFile = "desktop-relay.key" } = {}) {
  try {
    const [cert, key] = await Promise.all([readFile(path.join(dir, certFile)), readFile(path.join(dir, keyFile))]);
    return { cert, key };
  } catch (e) {
    if (e.code === "ENOENT") return null;
    throw e;
  }
}

function readBody(req, limit) {
  return new Promise((resolve, reject) => {
    const chunks = [];
    let size = 0;
    req.on("data", (chunk) => {
      if ((size += chunk.length) > limit) {
        // Stop reading; the 413 goes out and Node drains the rest when the response ends.
        req.pause();
        reject(Object.assign(new Error(`Request body exceeds ${limit} bytes`), { status: 413 }));
        return;
      }
      chunks.push(chunk);
    });
    req.on("end", () => resolve(chunks.length ? Buffer.concat(chunks) : undefined));
    req.on("error", reject);
  });
}

export function createBackendRelay({
  upstream = "https://chatgpt.com/backend-api",
  rewriteUsage = null,
  onEvent = () => {},
  maxBodyBytes = MAX_BODY_BYTES,
  connectTimeoutMs = CONNECT_TIMEOUT_MS,
} = {}) {
  const base = new URL(upstream);
  const basePath = base.pathname.replace(/\/$/, "");
  const upstreamPath = (url) => basePath + url.pathname.slice(BACKEND_PREFIX.length) + url.search;
  const reply = (res, status, data) => {
    res.writeHead(status, { "Content-Type": "application/json", "Cache-Control": "no-store" });
    res.end(JSON.stringify(data));
  };

  async function handle(req, res, url) {
    const headers = {};
    for (const [k, v] of Object.entries(req.headers)) if (!HOP_HEADERS.has(k)) headers[k] = v;
    headers["accept-encoding"] = "identity";
    let body, response;
    try {
      body = await readBody(req, maxBodyBytes);
    } catch (e) {
      reply(res, e.status ?? 400, { error: e.message });
      return;
    }
    try {
      response = await fetch(base.origin + upstreamPath(url), { method: req.method, headers, body, redirect: "manual" });
    } catch (e) {
      reply(res, 502, { error: `Backend unreachable: ${e.message}` });
      return;
    }
    const out = {};
    response.headers.forEach((v, k) => { if (!DROPPED_RESPONSE_HEADERS.has(k)) out[k] = v; });
    if (rewriteUsage && req.method === "GET" && url.pathname === BACKEND_PREFIX + "/wham/usage" && response.status === 200) {
      let payload = Buffer.from(await response.arrayBuffer());
      try {
        payload = Buffer.from(JSON.stringify(rewriteUsage(JSON.parse(payload.toString("utf8")))));
        out["content-type"] = "application/json";
      } catch (e) {
        // Fail open to the account's real numbers rather than break the poll.
        onEvent("pool-usage-overlay-error", { message: e.message });
      }
      out["content-length"] = String(payload.length);
      res.writeHead(response.status, out);
      res.end(payload);
      return;
    }
    res.writeHead(response.status, out);
    if (!response.body) { res.end(); return; }
    const stream = Readable.fromWeb(response.body);
    res.on("close", () => stream.destroy());
    stream.on("error", () => res.destroy());
    stream.pipe(res);
  }

  function upgrade(req, socket, head) {
    const url = new URL(req.url, "http://localhost");
    if (!isBackendPath(url.pathname)) { socket.destroy(); return; }
    const secure = base.protocol === "https:";
    const port = Number(base.port) || (secure ? 443 : 80);
    const link = secure
      ? tls.connect({ host: base.hostname, port, servername: base.hostname })
      : net.connect({ host: base.hostname, port });
    const drop = () => { clearTimeout(timer); socket.destroy(); link.destroy(); };
    const timer = setTimeout(drop, connectTimeoutMs);
    link.once(secure ? "secureConnect" : "connect", () => {
      clearTimeout(timer);
      const lines = [`${req.method} ${upstreamPath(url)} HTTP/1.1`, `host: ${base.host}`];
      for (let i = 0; i < req.rawHeaders.length; i += 2)
        if (req.rawHeaders[i].toLowerCase() !== "host") lines.push(`${req.rawHeaders[i]}: ${req.rawHeaders[i + 1]}`);
      link.write(lines.join("\r\n") + "\r\n\r\n");
      if (head.length) link.write(head);
      socket.pipe(link).pipe(socket);
    });
    link.on("error", drop);
    link.on("close", drop);
    socket.on("error", drop);
    socket.on("close", drop);
  }

  return { handle, upgrade };
}
