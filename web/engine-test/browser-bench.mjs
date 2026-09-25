// Runs bench.html in headless Chrome through the DevTools protocol and
// prints the result JSON. Node built-ins only (Node 22+ for WebSocket).
//
//   node web/engine-test/browser-bench.mjs <snapshot dir> <chrome binary> [st|mt] [threads]

import { spawn } from "node:child_process";
import { mkdtempSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { serve } from "./serve.mjs";

const [dir, chrome, build = "st", threads] = process.argv.slice(2);
if (!dir || !chrome) {
  console.error("usage: node browser-bench.mjs <snapshot dir> <chrome> [st|mt] [threads]");
  process.exit(2);
}

const server = await serve(dir);
const port = server.address().port;
const browser = spawn(chrome, [
  "--headless=new",
  "--no-sandbox",
  "--disable-gpu",
  "--remote-debugging-port=0",
  `--user-data-dir=${mkdtempSync(join(tmpdir(), "receipts-chrome-"))}`,
  "about:blank",
]);
const wsUrl = await new Promise((resolve, reject) => {
  let err = "";
  browser.stderr.on("data", (d) => {
    err += d;
    const m = /DevTools listening on (ws:\/\/\S+)/.exec(err);
    if (m) resolve(m[1]);
  });
  browser.on("exit", () => reject(new Error(`chrome exited: ${err}`)));
});

const ws = new WebSocket(wsUrl);
await new Promise((r) => ws.addEventListener("open", r));
let nextId = 1;
const pending = new Map();
ws.addEventListener("message", (e) => {
  const msg = JSON.parse(e.data);
  if (msg.id && pending.has(msg.id)) {
    pending.get(msg.id)(msg);
    pending.delete(msg.id);
  }
});
const send = (method, params = {}, sessionId) =>
  new Promise((resolve) => {
    const id = nextId++;
    pending.set(id, resolve);
    ws.send(JSON.stringify({ id, method, params, sessionId }));
  });

const { result: target } = await send("Target.createTarget", { url: "about:blank" });
const { result: attached } = await send("Target.attachToTarget", { targetId: target.targetId, flatten: true });
const session = attached.sessionId;
const q = new URLSearchParams({ build, ...(threads ? { threads } : {}) });
await send("Page.navigate", { url: `http://127.0.0.1:${port}/engine-test/bench.html?${q}` }, session);

let result;
for (let i = 0; i < 1200 && !result; i++) {
  await new Promise((r) => setTimeout(r, 500));
  const r = await send(
    "Runtime.evaluate",
    { expression: "window.__result ? JSON.stringify(window.__result) : ''", returnByValue: true },
    session,
  );
  if (r.result?.result?.value) result = JSON.parse(r.result.result.value);
}
console.log(JSON.stringify(result ?? { error: "timed out" }, null, 2));
ws.close();
browser.kill();
server.close();
process.exit(result && !result.error ? 0 : 1);
