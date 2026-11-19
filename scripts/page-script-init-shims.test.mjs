// Copyright (c) 2026 Cloudflare, Inc.
// Licensed under the Apache 2.0 license found in the LICENSE file or at:
// https://opensource.org/licenses/Apache-2.0

import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import test from "node:test";
import { createContext, runInContext } from "node:vm";
import ts from "typescript";

const source = readFileSync(
  new URL("../packages/page-script/src/init-shims.ts", import.meta.url),
  "utf8",
);
const code = ts.transpileModule(source, {
  compilerOptions: { module: ts.ModuleKind.CommonJS, target: ts.ScriptTarget.ES2022 },
}).outputText;

function load() {
  const calls = { fetch: [], setTimeout: [], clearTimeout: [], setInterval: [], clearInterval: [] };
  const sandbox = {
    exports: {},
    require: () => ({ ksGlobal: {} }),
    console,
    Promise,
    Map,
    Set,
    DOMException,
    fetch: (...args) => {
      calls.fetch.push(args);
      return Promise.resolve({ real: true });
    },
    setTimeout: (...args) => {
      calls.setTimeout.push(args);
      return 101;
    },
    clearTimeout: (...args) => calls.clearTimeout.push(args),
    setInterval: (...args) => {
      calls.setInterval.push(args);
      return 202;
    },
    clearInterval: (...args) => calls.clearInterval.push(args),
  };
  runInContext(code, createContext(sandbox));
  return { sandbox, calls, shims: sandbox.exports };
}

test("a fetch saved during init queues until the drain, then goes straight to the real fetch", async () => {
  const { sandbox, calls, shims } = load();
  shims.installInitShims();
  const saved = sandbox.fetch;
  const early = saved("data:text/plain,started");
  assert.equal(calls.fetch.length, 0, "queued while the shims are installed");
  shims.drainInitShims();
  assert.deepEqual(await early, { real: true });
  assert.equal(calls.fetch.length, 1);

  const late = await saved("data:text/plain,loaded", { method: "POST" });
  assert.deepEqual(late, { real: true });
  assert.deepEqual(calls.fetch[1], ["data:text/plain,loaded", { method: "POST" }]);
});

test("timers saved during init forward to the real ones after the drain", () => {
  const { sandbox, calls, shims } = load();
  shims.installInitShims();
  const savedSetTimeout = sandbox.setTimeout;
  const savedClearTimeout = sandbox.clearTimeout;
  const savedSetInterval = sandbox.setInterval;
  const savedClearInterval = sandbox.clearInterval;
  shims.drainInitShims();
  const fn = () => {};
  assert.equal(savedSetTimeout(fn, 5, "x"), 101);
  assert.deepEqual(calls.setTimeout.at(-1), [fn, 5, "x"]);
  assert.equal(savedSetInterval(fn, 7), 202);
  assert.deepEqual(calls.setInterval.at(-1), [fn, 7]);
  savedClearTimeout(101);
  savedClearInterval(202);
  assert.deepEqual(calls.clearTimeout.at(-1), [101]);
  assert.deepEqual(calls.clearInterval.at(-1), [202]);
});

test("a second install queues again and the first install stays retired", async () => {
  const { sandbox, calls, shims } = load();
  shims.installInitShims();
  const first = sandbox.fetch;
  shims.drainInitShims();
  shims.installInitShims();
  const second = sandbox.fetch;
  assert.notEqual(first, second);
  void second("a");
  assert.equal(calls.fetch.length, 0, "the new install queues");
  await first("b");
  assert.equal(calls.fetch.length, 1, "the retired shim does not queue");
  assert.equal(calls.fetch[0][0], "b");
});
