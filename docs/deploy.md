# Deploying Receipts (Cloudflare Pages + R2)

The app is static files. Snapshots are content-addressed static files.
Nothing runs on a server.

| What | Where | Why |
|---|---|---|
| App (`web/dist`: HTML, JS, CSS, both WASM packages) | Cloudflare **Pages** | Sends the COOP/COEP headers from `web/public/_headers`, so the threaded engine works. Every file is well under Pages' 25 MiB limit. |
| Snapshot (`manifest.json` + three `.arrow` files, 239 MB) | Cloudflare **R2** bucket, public through a custom domain | Pages can't hold a 239 MB file. R2 charges nothing for egress. |

## 1. Build

```sh
tools/wasm/build.sh                                  # web/public/pkg/{st,mt}
cd web && npm ci
VITE_SNAPSHOT_BASE=https://data.example.org/nyc311/3b42e46a2d17981f/ npm run build   # web/dist
```

`VITE_SNAPSHOT_BASE` is the URL of the snapshot directory, with a trailing
slash. The default `/snap/` is for local development only.

## 2. Snapshot on R2

```sh
npx wrangler r2 bucket create receipts-snapshots
D=snapshots/nyc311/3b42e46a2d17981f
for f in manifest.json data.arrow cleaning_log.arrow rejects.arrow; do
  npx wrangler r2 object put "receipts-snapshots/nyc311/3b42e46a2d17981f/$f" --file "$D/$f" \
    --cache-control "public, max-age=31536000, immutable" --remote
done
```

The directory name is the snapshot hash, so its files never change and
can be cached forever. A new snapshot gets a new directory, and the app is
rebuilt to point at it.

**CORS is required.** The app fetches from another origin while
cross-origin isolated (COEP `require-corp`). Only CORS-enabled responses
are allowed, so give the bucket a CORS policy (R2 → bucket → Settings →
CORS policy):

```json
[
  {
    "AllowedOrigins": ["https://receipts.example.org"],
    "AllowedMethods": ["GET", "HEAD"],
    "AllowedHeaders": ["*"],
    "MaxAgeSeconds": 86400
  }
]
```

Connect a custom domain (R2 → bucket → Settings → Custom domains, for
example `data.example.org`). The `r2.dev` URL is rate-limited and not for
production.

## 3. App on Pages

```sh
npx wrangler pages project create receipts --production-branch main
npx wrangler pages deploy web/dist --project-name receipts
```

Or connect the repository in the Pages dashboard with these settings:
- **Root directory:** `web`
- **Build command:** `npm ci && npm run build`
- **Output directory:** `dist`
- **Environment variable:** `VITE_SNAPSHOT_BASE`

The dashboard build can't run `tools/wasm/build.sh`, because it needs Rust
and a nightly toolchain. Either build the WASM packages in CI and deploy
with `wrangler pages deploy`, or commit `web/public/pkg`. The first is
recommended.

## 4. Check

- The page header shows **✓ Verified … 4 threads** (or however many cores
  the device has). "1 thread" means the COOP/COEP headers aren't arriving;
  check `_headers` made it into `dist`.
- "The data couldn't be loaded", with a CORS error in the console, means
  the R2 CORS policy doesn't list the app's origin.
- A hash mismatch message means the files on R2 don't match the manifest.
  Re-upload the directory. The engine refuses the snapshot rather than
  showing unverifiable numbers.
