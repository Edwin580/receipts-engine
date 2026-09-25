import type { Plugin } from "vite";
import { defineConfig } from "vitest/config";
import react from "@vitejs/plugin-react";
import { createReadStream, statSync } from "node:fs";
import { join, normalize } from "node:path";

// Cross-origin isolation, so the threaded engine can use SharedArrayBuffer.
// Production sends the same headers from public/_headers (Cloudflare Pages).
const isolation = {
  "Cross-Origin-Opener-Policy": "same-origin",
  "Cross-Origin-Embedder-Policy": "require-corp",
};

/** Serves a local snapshot directory at /snap/ (dev and preview only). */
function localSnapshot(): Plugin {
  const dir = process.env.RECEIPTS_SNAPSHOT_DIR;
  const middleware = (req: { url?: string }, res: any, next: () => void) => {
    if (!dir || !req.url?.startsWith("/snap/")) return next();
    const file = normalize(join(dir, decodeURIComponent(req.url.slice(6).split("?")[0])));
    if (!file.startsWith(normalize(dir))) return res.writeHead(403).end();
    let size: number;
    try {
      const st = statSync(file);
      if (!st.isFile()) throw new Error("not a file");
      size = st.size;
    } catch {
      return res.writeHead(404).end();
    }
    res.writeHead(200, { ...isolation, "Content-Length": size, "Content-Type": "application/octet-stream" });
    createReadStream(file).pipe(res);
  };
  return {
    name: "receipts-local-snapshot",
    configureServer: (s) => void s.middlewares.use(middleware),
    configurePreviewServer: (s) => void s.middlewares.use(middleware),
  };
}

export default defineConfig({
  plugins: [react(), localSnapshot()],
  server: { headers: isolation },
  preview: { headers: isolation },
  worker: { format: "es" },
  build: { target: "es2022" },
  test: { include: ["src/**/*.test.ts"] },
});
