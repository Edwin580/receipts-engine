// Static server for the engine test pages, with the cross-origin isolation
// headers the threaded build needs (COOP/COEP). Node built-ins only.
//
//   node web/engine-test/serve.mjs <snapshot dir> [port]
//
// Serves web/ at /, the WASM packages (web/public/pkg) at /pkg/ and the
// snapshot directory at /snap/.

import { createServer } from "node:http";
import { createReadStream, statSync } from "node:fs";
import { join, dirname, extname, normalize } from "node:path";
import { fileURLToPath } from "node:url";

const web = join(dirname(fileURLToPath(import.meta.url)), "..");
const types = {
  ".html": "text/html", ".js": "text/javascript", ".mjs": "text/javascript",
  ".wasm": "application/wasm", ".json": "application/json", ".arrow": "application/octet-stream",
};

export function serve(snapshotDir, port = 0) {
  const server = createServer((req, res) => {
    const url = new URL(req.url, "http://x");
    const [base, rel] = url.pathname.startsWith("/snap/")
      ? [snapshotDir, url.pathname.slice(6)]
      : url.pathname.startsWith("/pkg/")
        ? [join(web, "public"), url.pathname.slice(1)]
        : [web, url.pathname.slice(1) || "index.html"];
    const path = normalize(join(base, decodeURIComponent(rel)));
    if (!path.startsWith(normalize(base))) {
      res.writeHead(403).end();
      return;
    }
    const file = path;
    let size;
    try {
      const st = statSync(file);
      if (!st.isFile()) throw new Error("not a file");
      size = st.size;
    } catch {
      res.writeHead(404).end("not found");
      return;
    }
    res.writeHead(200, {
      "Content-Type": types[extname(file)] ?? "application/octet-stream",
      "Content-Length": size,
      "Cross-Origin-Opener-Policy": "same-origin",
      "Cross-Origin-Embedder-Policy": "require-corp",
      "Cache-Control": "no-store",
    });
    createReadStream(file).pipe(res);
  });
  return new Promise((resolve) => server.listen(port, "127.0.0.1", () => resolve(server)));
}

if (import.meta.url === `file://${process.argv[1]}`) {
  const [dir, port = "8080"] = process.argv.slice(2);
  const s = await serve(dir, Number(port));
  console.log(`http://127.0.0.1:${s.address().port}/engine-test/bench.html`);
}
