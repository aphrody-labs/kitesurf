// Copyright (c) 2026 Cloudflare, Inc.
// Licensed under the Apache 2.0 license found in the LICENSE file or at:
// https://opensource.org/licenses/Apache-2.0

// The Docker image's front Worker (scripts/workerd-config.mjs). It routes requests to public/ or
// the engine as Wrangler's assets config does, serves public/ to the engine as FONT_ASSETS, and
// warms the engine up on the first request. /health fails until the warm-up has finished.

import { WorkerEntrypoint } from "cloudflare:workers";

// Wrangler's `run_worker_first` routes, which scripts/workerd-config.mjs checks.
const workerFirst = (pathname) => pathname === "/" || pathname.startsWith("/rbi/");

// workerd's disk service sends every file as application/octet-stream.
const CONTENT_TYPES = {
  avif: "image/avif",
  css: "text/css; charset=utf-8",
  gif: "image/gif",
  html: "text/html; charset=utf-8",
  ico: "image/x-icon",
  jpg: "image/jpeg",
  js: "text/javascript; charset=utf-8",
  json: "application/json",
  map: "application/json",
  mjs: "text/javascript; charset=utf-8",
  otf: "font/otf",
  png: "image/png",
  svg: "image/svg+xml",
  ttf: "font/ttf",
  txt: "text/plain; charset=utf-8",
  wasm: "application/wasm",
  webp: "image/webp",
  woff2: "font/woff2",
};

/** A file from public/, or 404. Only names with an extension are files: the disk service answers
 * a directory with its listing. */
const serveFile = async function (dir, request) {
  const { pathname } = new URL(request.url);
  const extension = /\.([a-z0-9]+)$/i.exec(pathname)?.[1].toLowerCase();
  if (extension === undefined || (request.method !== "GET" && request.method !== "HEAD")) {
    return new Response("not found", { status: 404 });
  }
  const file = await dir.fetch(new URL(pathname, "http://public/"), { method: request.method });
  if (!file.ok) {
    await file.body?.cancel();
    return new Response("not found", { status: 404 });
  }
  const headers = new Headers(file.headers);
  headers.set("content-type", CONTENT_TYPES[extension] ?? "application/octet-stream");
  return new Response(file.body, { status: 200, headers });
};

export class Assets extends WorkerEntrypoint {
  /** @param {Request} request @returns {Promise<Response>} */
  fetch(request) {
    return serveFile(this.env.PUBLIC, request);
  }
}

const WARM_UP_HTML =
  "<!doctype html><title>warm-up</title>" +
  "<style>body{font:16px sans-serif;display:flex}p{color:red;margin:1em}</style>" +
  "<h1>Kitesurf</h1><p>Some <b>bold</b> and <i>italic</i> text</p>" +
  "<form><input value=text><button>Submit</button></form><ul><li>One<li>Two</ul>" +
  "<script>document.body.dataset.ready = [1, 2].map(String).join()</script>";
const WARM_UP_TIMEOUT_MS = 60_000;

/** Opens, screenshots and closes one small page over CDP, so that a client's first navigation
 * does not wait for V8 to compile the engine's wasm. Resolves with its duration. */
const warmUp = async function (engine) {
  const started = Date.now();
  const response = await engine.fetch("http://kitesurf.internal/devtools/browser", {
    headers: { upgrade: "websocket" },
  });
  const ws = response.webSocket;
  if (!ws) throw new Error(`the engine answered the WebSocket upgrade with ${response.status}`);
  ws.accept();
  const pending = new Map();
  let nextId = 0;
  ws.addEventListener("message", (event) => {
    const message = JSON.parse(String(event.data));
    const reply = pending.get(message.id);
    if (!reply) return;
    pending.delete(message.id);
    if (message.error) reply.reject(new Error(`${reply.method}: ${message.error.message}`));
    else reply.resolve(message.result);
  });
  const send = (method, params = {}, sessionId) =>
    new Promise((resolve, reject) => {
      const id = ++nextId;
      pending.set(id, { resolve, reject, method });
      ws.send(JSON.stringify({ id, method, params, ...(sessionId ? { sessionId } : {}) }));
    });
  const run = async () => {
    const { targetId } = await send("Target.createTarget", { url: "about:blank" });
    const { sessionId } = await send("Target.attachToTarget", { targetId, flatten: true });
    const url = "data:text/html," + encodeURIComponent(WARM_UP_HTML);
    await send("Page.navigate", { url }, sessionId);
    await send("Runtime.evaluate", { expression: "document.body.dataset.ready" }, sessionId);
    await send("Page.captureScreenshot", {}, sessionId);
    await send("Target.closeTarget", { targetId });
  };
  let timer;
  const timeout = new Promise((_, reject) => {
    timer = setTimeout(() => reject(new Error("warm-up timed out")), WARM_UP_TIMEOUT_MS);
  });
  try {
    await Promise.race([run(), timeout]);
    return Date.now() - started;
  } finally {
    clearTimeout(timer);
    ws.close();
  }
};

let warmUpStarted = false;
let warm = false;

export default {
  /** @param {Request} request @param {{ENGINE: Fetcher, PUBLIC: Fetcher}} env
   * @param {ExecutionContext} ctx @returns {Promise<Response>} */
  async fetch(request, env, ctx) {
    if (!warmUpStarted) {
      warmUpStarted = true;
      ctx.waitUntil(
        warmUp(env.ENGINE)
          .then(
            (ms) => console.log(`[kitesurf] warmed up in ${ms} ms`),
            (error) => console.warn(`[kitesurf] warm-up failed: ${error.message}`),
          )
          .finally(() => {
            warm = true;
          }),
      );
    }
    const { pathname } = new URL(request.url);
    if (pathname === "/health" && !warm) {
      return new Response("warming up\n", { status: 503, headers: { "retry-after": "1" } });
    }
    if (!workerFirst(pathname) && !request.headers.has("upgrade")) {
      const file = await serveFile(env.PUBLIC, request);
      if (file.status !== 404) return file;
    }
    return env.ENGINE.fetch(request);
  },
};
