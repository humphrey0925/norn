# Norn website

The static site for Norn: `index.html` (English), `zh-TW/index.html` (繁體中文),
`style.css`, and `assets/` (copies of the icon, screenshots and benchmark charts).
No build step.

## Preview locally

```bash
python3 -m http.server -d site 8000
```

and open http://localhost:8000.

## Deploy on Cloudflare

`wrangler.jsonc` at the top of the repository serves `site/` as a Worker's static
assets (`site/README.md` is left out by `site/.assetsignore`).

1. Cloudflare dashboard → **Workers & Pages** → **Create** → import the
   `humphrey0925/norn` repository.
2. Project name `norn` (as in `wrangler.jsonc`), build command empty, deploy
   command `npx wrangler deploy`. Every push to `master` redeploys.
3. In the Worker → **Settings** → **Domains & Routes** → **Add** → **Custom
   domain**: `norn-c.com` (and `www.norn-c.com` if wanted). The DNS is on
   Cloudflare, so the record and certificate are set up for you.

## Updating

The screenshots and charts in `assets/` are copies of `vscode/media/screenshots`
and `docs/benchmarks/*.svg`; copy them again when those change. The benchmark
table is a subset of the one in the top-level README.
