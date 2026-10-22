# Norn website

The static site for Norn: `index.html` (English), `zh-TW/index.html` (繁體中文),
`style.css`, and `assets/` (copies of the icon, screenshots and benchmark charts).
No build step.

## Preview locally

```bash
python3 -m http.server -d site 8000
```

and open http://localhost:8000.

## Deploy on Cloudflare Pages

1. Cloudflare dashboard → **Workers & Pages** → **Create** → **Pages** →
   **Connect to Git**, and pick `humphrey0925/norn`.
2. Build settings:
   - Production branch: `master`
   - Framework preset: **None**
   - Build command: *(empty)*
   - Build output directory: `site`
3. **Save and Deploy**. Every push to `master` redeploys; pull requests get preview URLs.
4. In the project → **Custom domains** → **Set up a custom domain**, enter the
   domain `norn-c.com` (and `www.norn-c.com` if wanted). Since the domain's DNS is already on Cloudflare,
   the record is added for you and the certificate is issued in a few minutes.

## Updating

The screenshots and charts in `assets/` are copies of `vscode/media/screenshots`
and `docs/benchmarks/*.svg`; copy them again when those change. The benchmark
table is a subset of the one in the top-level README.
