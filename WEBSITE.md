# Kuali website

The public website is a dependency-free static site in [`website/`](website/).
Production is served from <https://kuali.garrux.dev> by the `kuali-site`
Cloudflare Worker using Workers Static Assets. The Wrangler configuration at
[`wrangler.jsonc`](wrangler.jsonc) is the deployment source of truth.
It pins the public Worker to the `Garrux` Cloudflare account so non-interactive
deployments never select another account available to the maintainer.

## Preview locally

```sh
python3 -m http.server 4173 --directory website
```

Open <http://localhost:4173>.

## Deploy to Cloudflare

Authenticate Wrangler, validate the site, and deploy the current revision:

```sh
npm ci
npm exec wrangler login
npm run deploy:website
```

Use `npm run check:website` to run the same SEO and link validation plus a
Wrangler dry run without publishing.

The custom domain remains attached to the Worker in Cloudflare. Do not create a
second public deployment: duplicate hosts split indexing signals and can expose
stale installation instructions. The `_headers` file prevents `workers.dev`
preview URLs from being indexed.

## Canonical URL

SEO metadata uses `https://kuali.garrux.dev/` as the canonical site. Keep this
origin in every public page, `website/robots.txt`, and `website/sitemap.xml`.
Every English page must link to its Spanish equivalent and vice versa through
`hreflang`; update the sitemap and website tests whenever a public page is
added or renamed.
