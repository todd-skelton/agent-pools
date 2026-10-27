import test from "node:test";
import assert from "node:assert/strict";
import http from "node:http";
import { once } from "node:events";
import { mkdtemp, mkdir, writeFile } from "node:fs/promises";
import os from "node:os";
import path from "node:path";
import { createBackendRelay, isBackendPath, loadRelayTls } from "./backend-relay.mjs";

function listen(server) {
  return new Promise((resolve) => server.listen(0, "127.0.0.1", () => resolve(server.address().port)));
}
// Fake chatgpt.com: usage JSON, an echo endpoint, a chunked stream, and a raw
// upgrade that echoes bytes.
async function fakeUpstream() {
  const requests = [];
  const server = http.createServer(async (req, res) => {
    const chunks = [];
    for await (const c of req) chunks.push(c);
    requests.push({ method: req.method, url: req.url, headers: req.headers, body: Buffer.concat(chunks).toString() });
    if (req.url === "/backend-api/wham/usage") {
      res.writeHead(200, { "content-type": "application/json", "x-upstream": "yes" });
      res.end(JSON.stringify({ rate_limit: { allowed: false } }));
    } else if (req.url === "/backend-api/broken-usage") {
      res.writeHead(200, { "content-type": "application/json" });
      res.end("not json");
    } else if (req.url.startsWith("/backend-api/stream")) {
      res.writeHead(200, { "content-type": "text/event-stream" });
      res.write("data: one\n\n");
      setTimeout(() => res.end("data: two\n\n"), 20);
    } else {
      res.writeHead(201, { "content-type": "text/plain" });
      res.end("echo");
    }
  });
  server.on("upgrade", (req, socket, head) => {
    requests.push({ method: req.method, url: req.url, headers: req.headers, upgrade: true });
    socket.write("HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\r\n");
    if (head.length) socket.write(head);
    socket.pipe(socket);
  });
  return { server, requests, port: await listen(server) };
}
function front(relay) {
  const server = http.createServer((req, res) => relay.handle(req, res, new URL(req.url, "http://localhost")));
  server.on("upgrade", relay.upgrade);
  return server;
}

test("backend path matching", () => {
  assert.ok(isBackendPath("/backend-api"));
  assert.ok(isBackendPath("/backend-api/wham/usage"));
  assert.ok(!isBackendPath("/backend-apix"));
  assert.ok(!isBackendPath("/api/status"));
});

test("relay TLS material is optional and only used as a complete pair", async () => {
  const dir = await mkdtemp(path.join(os.tmpdir(), "relay-tls-"));
  assert.equal(await loadRelayTls(dir), null, "no files: plain HTTP");
  await writeFile(path.join(dir, "desktop-relay.crt"), "CERT");
  assert.equal(await loadRelayTls(dir), null, "a lone certificate is not enough");
  await writeFile(path.join(dir, "desktop-relay.key"), "KEY");
  const material = await loadRelayTls(dir);
  assert.equal(material.cert.toString(), "CERT");
  assert.equal(material.key.toString(), "KEY");
  const unreadable = await mkdtemp(path.join(os.tmpdir(), "relay-tls-"));
  await mkdir(path.join(unreadable, "desktop-relay.crt"));
  await writeFile(path.join(unreadable, "desktop-relay.key"), "KEY");
  await assert.rejects(loadRelayTls(unreadable), { code: "EISDIR" }, "other read errors surface");
});

test("relay rewrites usage, forwards other calls verbatim, and streams", async () => {
  const up = await fakeUpstream();
  const events = [];
  const relay = createBackendRelay({
    upstream: `http://127.0.0.1:${up.port}/backend-api`,
    rewriteUsage: (u) => ({ ...u, pool: "rewritten" }),
    onEvent: (type, details) => events.push({ type, ...details }),
    maxBodyBytes: 64,
  });
  const server = front(relay);
  const port = await listen(server);
  const origin = `http://127.0.0.1:${port}`;
  try {
    const usage = await fetch(origin + "/backend-api/wham/usage", { headers: { authorization: "Bearer t", "chatgpt-account-id": "acct" } });
    assert.equal(usage.status, 200);
    assert.equal(usage.headers.get("x-upstream"), "yes");
    assert.deepEqual(await usage.json(), { rate_limit: { allowed: false }, pool: "rewritten" });
    assert.equal(up.requests.at(-1).headers.authorization, "Bearer t");
    assert.equal(up.requests.at(-1).headers["chatgpt-account-id"], "acct");
    assert.equal(up.requests.at(-1).headers["accept-encoding"], "identity");

    const posted = await fetch(origin + "/backend-api/conversation?x=1", { method: "POST", headers: { "content-type": "application/json" }, body: "{\"a\":1}" });
    assert.equal(posted.status, 201);
    assert.equal(await posted.text(), "echo");
    assert.equal(up.requests.at(-1).url, "/backend-api/conversation?x=1");
    assert.equal(up.requests.at(-1).body, "{\"a\":1}");

    const stream = await fetch(origin + "/backend-api/stream");
    assert.equal(stream.headers.get("content-type"), "text/event-stream");
    assert.equal(await stream.text(), "data: one\n\ndata: two\n\n");

    const broken = await fetch(origin + "/backend-api/broken-usage");
    assert.equal(await broken.text(), "not json", "non-usage paths are never rewritten");

    const big = await fetch(origin + "/backend-api/conversation", { method: "POST", body: "x".repeat(65) });
    assert.equal(big.status, 413);
    assert.equal(up.requests.filter((r) => r.url.startsWith("/backend-api/conversation")).length, 1, "oversized body never reaches upstream");
    assert.deepEqual(events, []);
  } finally {
    server.close(); up.server.close();
  }
});

test("relay fails open when the usage rewrite throws and reports it", async () => {
  const up = await fakeUpstream();
  const events = [];
  const relay = createBackendRelay({
    upstream: `http://127.0.0.1:${up.port}/backend-api`,
    rewriteUsage: () => { throw new Error("overlay broke"); },
    onEvent: (type, details) => events.push({ type, ...details }),
  });
  const server = front(relay);
  const port = await listen(server);
  try {
    const usage = await fetch(`http://127.0.0.1:${port}/backend-api/wham/usage`);
    assert.deepEqual(await usage.json(), { rate_limit: { allowed: false } });
    assert.deepEqual(events, [{ type: "pool-usage-overlay-error", message: "overlay broke" }]);
  } finally {
    server.close(); up.server.close();
  }
});

test("relay answers 502 when the upstream is down", async () => {
  const closed = await fakeUpstream();
  await new Promise((r) => closed.server.close(r));
  const relay = createBackendRelay({ upstream: `http://127.0.0.1:${closed.port}/backend-api` });
  const server = front(relay);
  const port = await listen(server);
  try {
    const response = await fetch(`http://127.0.0.1:${port}/backend-api/wham/usage`);
    assert.equal(response.status, 502);
    assert.match((await response.json()).error, /unreachable/);
  } finally {
    server.close();
  }
});

test("relay tunnels websocket upgrades and drops others", async () => {
  const up = await fakeUpstream();
  const relay = createBackendRelay({ upstream: `http://127.0.0.1:${up.port}/backend-api` });
  const server = front(relay);
  const port = await listen(server);
  try {
    const request = http.request({ host: "127.0.0.1", port, path: "/backend-api/wham/remote/control/server?x=1", headers: { connection: "Upgrade", upgrade: "websocket", authorization: "Bearer t" } });
    request.end();
    const [response, socket] = await once(request, "upgrade");
    assert.equal(response.statusCode, 101);
    assert.equal(up.requests.at(-1).url, "/backend-api/wham/remote/control/server?x=1");
    assert.equal(up.requests.at(-1).headers.authorization, "Bearer t");
    assert.equal(up.requests.at(-1).headers.host, `127.0.0.1:${up.port}`);
    socket.write("ping");
    const [echoed] = await once(socket, "data");
    assert.equal(echoed.toString(), "ping");
    socket.destroy();

    const other = http.request({ host: "127.0.0.1", port, path: "/other", headers: { connection: "Upgrade", upgrade: "websocket" } });
    other.end();
    const [error] = await once(other, "error");
    assert.equal(error.code, "ECONNRESET");
  } finally {
    server.closeAllConnections(); server.close(); up.server.closeAllConnections(); up.server.close();
  }
});
