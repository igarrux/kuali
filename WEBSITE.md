# Kuali website

The public website is a dependency-free static site in [`website/`](website/).
Every path is relative, so the same directory works at a domain root or under a
GitHub Pages project path.

## Preview locally

```sh
python3 -m http.server 4173 --directory website
```

Open <http://localhost:4173>.

## GitHub Pages

1. Open **Settings → Pages** in the repository.
2. Set **Source** to **GitHub Actions**.
3. Run **Deploy website to GitHub Pages** from the Actions tab.

The workflow uploads `website/` without a build step.

## Cloudflare

The production custom domain is served by the `kuali-site` Worker with static
assets. Deploy the current website with:

```sh
npx wrangler deploy website --name=kuali-site --compatibility-date 2026-08-10
```

Cloudflare Pages remains an alternative deployment target. To use it, connect
the repository from the dashboard:

Connect `igarrux/kuali` from **Workers & Pages → Create → Pages → Connect to
Git** and use:

| Setting | Value |
|---|---|
| Production branch | `main` |
| Framework preset | None |
| Build command | Leave blank |
| Build output directory | `website` |
| Root directory | Repository root |

For a direct Pages upload after creating a Pages project, run:

```sh
npx wrangler pages deploy website --project-name=kuali-site
```

## Canonical URL

SEO metadata currently uses `https://igarrux.github.io/kuali/` as the canonical
site. If a custom domain becomes the primary deployment, replace that origin in
the four HTML files, `website/robots.txt`, and `website/sitemap.xml` before
indexing the new domain.
