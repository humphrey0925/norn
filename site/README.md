# Norn website

The static site for Norn: `index.html` (English), `zh-TW/index.html` (繁體中文)
and `style.css`. `build.mjs` builds it into `out/site`, copying in the icon
(`vscode/media`), the screenshots (`vscode/media/screenshots`) and the benchmark
charts (`docs/benchmarks`), and filling the benchmark table from the README's.
Nothing is copied by hand: a new screenshot or benchmark is on the site at the
next deploy.

## Preview locally

```bash
npx wrangler dev
```

builds it and serves it at http://localhost:8787, rebuilding when a page,
screenshot or chart changes. Or `node site/build.mjs` and any static server on
`out/site`.

## Deploy on Cloudflare

`wrangler.jsonc` at the top of the repository runs `site/build.mjs` and serves
`out/site` as a Worker's static assets (`README.md` is left out by
`.assetsignore`).

1. Cloudflare dashboard → **Workers & Pages** → **Create** → import the
   `humphrey0925/norn` repository.
2. Project name `norn` (as in `wrangler.jsonc`), build command empty, deploy
   command `npx wrangler deploy`. Every push to `master` redeploys.
3. In the Worker → **Settings** → **Domains & Routes** → **Add** → **Custom
   domain**: `norn-c.com` (and `www.norn-c.com` if wanted). The DNS is on
   Cloudflare, so the record and certificate are set up for you.

## Checked in CI

The `website` job in `.github/workflows/ci.yml` builds the site on every push
and pull request, so a change to the README's benchmark table that the build
cannot read fails there, not at deploy.
