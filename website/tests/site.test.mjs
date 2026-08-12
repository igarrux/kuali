import assert from "node:assert/strict";
import { existsSync, readFileSync, readdirSync, statSync } from "node:fs";
import { dirname, extname, join, resolve } from "node:path";
import test from "node:test";
import { fileURLToPath } from "node:url";

const websiteRoot = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const repositoryRoot = resolve(websiteRoot, "..");
const publicPages = [
  "index.html",
  "es/index.html",
  "discord-meeting-transcription/index.html",
  "es/transcripcion-reuniones-discord/index.html",
  "google-meet-transcription/index.html",
  "es/transcripcion-google-meet/index.html",
  "guides/index.html",
  "es/guides/index.html",
];
const installationPages = ["index.html", "es/index.html", "guides/index.html", "es/guides/index.html"];
const allHtmlPages = [...publicPages, "404.html"];

function read(relativePath) {
  return readFileSync(join(websiteRoot, relativePath), "utf8");
}

function readRepository(relativePath) {
  return readFileSync(join(repositoryRoot, relativePath), "utf8");
}

function attributeValues(html, attribute) {
  return [...html.matchAll(new RegExp(`\\b${attribute}="([^"]+)"`, "g"))].map((match) => match[1]);
}

function metadataContent(html, selector) {
  return html.match(selector)?.[1] ?? "";
}

function canonicalFor(page) {
  return metadataContent(read(page), /<link rel="canonical" href="([^"]+)">/);
}

function resolveLocalReference(page, reference) {
  if (/^(?:https?:|mailto:|tel:|data:)/.test(reference) || reference.startsWith("#")) return null;
  const cleanPath = reference.split(/[?#]/, 1)[0];
  if (!cleanPath) return join(websiteRoot, dirname(page), "index.html");
  const target = resolve(websiteRoot, dirname(page), cleanPath);
  return cleanPath.endsWith("/") ? join(target, "index.html") : target;
}

function allFiles(directory) {
  return readdirSync(directory, { withFileTypes: true }).flatMap((entry) => {
    const path = join(directory, entry.name);
    return entry.isDirectory() ? allFiles(path) : [path];
  });
}

test("all public pages contain unique and complete SEO metadata", () => {
  const titles = new Set();
  const descriptions = new Set();
  const canonicals = new Set();

  for (const page of publicPages) {
    const html = read(page);
    const title = metadataContent(html, /<title>([^<]+)<\/title>/);
    const description = metadataContent(html, /<meta name="description" content="([^"]+)">/);
    const canonical = canonicalFor(page);
    const openGraphUrl = metadataContent(html, /<meta property="og:url" content="([^"]+)">/);

    assert.ok(title.length >= 30 && title.length <= 70, `${page} title length is ${title.length}`);
    assert.ok(description.length >= 110 && description.length <= 180, `${page} description length is ${description.length}`);
    assert.ok(!titles.has(title), `${page} duplicates title ${title}`);
    assert.ok(!descriptions.has(description), `${page} duplicates its description`);
    assert.ok(!canonicals.has(canonical), `${page} duplicates canonical ${canonical}`);
    titles.add(title);
    descriptions.add(description);
    canonicals.add(canonical);

    assert.match(html, /<meta name="author" content="Jhon Guerrero">/);
    assert.match(html, /<meta name="robots" content="index, follow, max-image-preview:large">/);
    assert.match(canonical, /^https:\/\/kuali\.garrux\.dev\//);
    assert.equal(openGraphUrl, canonical, `${page} Open Graph URL must match canonical`);
    assert.equal((html.match(/rel="alternate" hreflang=/g) ?? []).length, 3, `${page} hreflang count`);
    assert.match(html, /<link rel="sitemap" type="application\/xml"/);
    assert.match(html, /<meta property="og:title"/);
    assert.match(html, /<meta property="og:description"/);
    assert.match(html, /<meta property="og:image"/);
    assert.match(html, /<meta property="og:image:alt"/);
    assert.match(html, /<meta name="twitter:card" content="summary_large_image">/);
    assert.match(html, /<meta name="twitter:title"/);
    assert.match(html, /<meta name="twitter:description"/);
    assert.match(html, /<meta name="twitter:image:alt"/);
    assert.equal((html.match(/<h1\b/g) ?? []).length, 1, `${page} must contain one h1`);
    assert.doesNotMatch(html, /<meta name="keywords"/i, `${page} uses obsolete meta keywords`);
  }
});

test("structured data is valid JSON", () => {
  for (const page of publicPages) {
    const blocks = [...read(page).matchAll(/<script type="application\/ld\+json">([\s\S]*?)<\/script>/g)];
    assert.ok(blocks.length >= 1, `${page} has no JSON-LD`);
    for (const [, block] of blocks) {
      const value = JSON.parse(block);
      assert.equal(value["@context"], "https://schema.org");
      assert.ok(value["@type"]);
    }
  }
});

test("local links and assets resolve from every page depth", () => {
  for (const page of allHtmlPages) {
    const html = read(page);
    for (const attribute of ["href", "src"]) {
      for (const reference of attributeValues(html, attribute)) {
        assert.ok(!reference.startsWith("/"), `${page} uses root-relative ${reference}`);
        const target = resolveLocalReference(page, reference);
        if (target) assert.ok(existsSync(target), `${page} points to missing ${reference}`);
      }
    }
  }
});

test("markup keeps identifiers and image descriptions coherent", () => {
  for (const page of allHtmlPages) {
    const html = read(page);
    const ids = attributeValues(html, "id");
    assert.equal(new Set(ids).size, ids.length, `${page} contains duplicate IDs`);
    assert.doesNotMatch(html, /\son(?:click|load|error)=/i, `${page} contains inline event handlers`);
    assert.doesNotMatch(html, /\sstyle="/i, `${page} contains inline styles`);
    assert.doesNotMatch(html, /<div(?=[^>]*\baria-label=)(?![^>]*\brole=)[^>]*>/i, `${page} labels a generic div without a role`);

    for (const image of html.matchAll(/<img\b([^>]*)>/g)) {
      assert.match(image[1], /\bsrc="[^"]+"/, `${page} image is missing a source`);
      assert.match(image[1], /\balt="[^"]*"/, `${page} image is missing alt text`);
      if (!image[1].includes("data-lightbox-image")) {
        assert.match(image[1], /\bwidth="\d+"/, `${page} image is missing width`);
        assert.match(image[1], /\bheight="\d+"/, `${page} image is missing height`);
      }
    }
  }
});

test("installation remains an explicit two-command flow", () => {
  const install = "brew install --cask igarrux/kuali/kuali";
  const quarantine = "xattr -dr com.apple.quarantine /Applications/Kuali.app";
  for (const page of installationPages) {
    const html = read(page);
    assert.ok(html.includes(install), `${page} is missing Homebrew installation`);
    assert.ok(html.includes(quarantine), `${page} is missing explicit quarantine command`);
    assert.doesNotMatch(html, /postflight|--no-quarantine|automatically removes the quarantine/i);
  }
});

test("landing pages show the real app and state the local data boundary", () => {
  const english = read("index.html");
  const spanish = read("es/index.html");

  assert.match(english, /assets\/kuali-app\.png/);
  assert.match(spanish, /assets\/kuali-app\.es\.png/);
  assert.match(english, /<img src="\.\/assets\/kuali-app\.webp"/);
  assert.match(spanish, /<img src="\.\.\/assets\/kuali-app\.es\.webp"/);
  assert.match(english, /Everything Kuali creates lives on your PC/);
  assert.match(spanish, /Todo lo que Kuali genera vive en tu PC/);
  for (const html of [english, spanish]) {
    assert.match(html, />LLM</);
    assert.match(html, />Webhook</);
    assert.match(html, /Jhon Guerrero/);
    assert.doesNotMatch(html, /©|Kuali contributors|Colaboradores de Kuali/);
  }
});

test("landing pages substantiate the native runtime and measured resource use", () => {
  const english = read("index.html");
  const spanish = read("es/index.html");

  for (const html of [english, spanish]) {
    assert.match(html, /Rust/);
    assert.match(html, /~20 MB/);
    assert.match(html, /~600 MB/);
    assert.match(html, /127\.0\.0\.1/);
    assert.match(html, /Apple Silicon/);
    assert.match(html, /Q5/);
  }

  assert.match(english, /first active meeting[\s\S]*released after the last one ends/);
  assert.match(english, /actual memory use varies/);
  assert.match(english, /Free and open source/);
  assert.match(spanish, /primera reunión activa[\s\S]*se libera al terminar la última/);
  assert.match(spanish, /consumo real varía/);
  assert.match(spanish, /Gratis y open source/);
});

test("download actions lead to installation while the secondary action opens GitHub", () => {
  const english = read("index.html");
  const spanish = read("es/index.html");

  assert.match(english, /href="https:\/\/kuali\.garrux\.dev\/guides\/#install">Download for macOS/);
  assert.match(english, /href="https:\/\/github\.com\/igarrux\/kuali">View on GitHub/);
  assert.match(spanish, /href="https:\/\/kuali\.garrux\.dev\/es\/guides\/#instalar">Descargar para macOS/);
  assert.match(spanish, /href="https:\/\/github\.com\/igarrux\/kuali">Ver en GitHub/);
});

test("both guides document model weights and Standard Webhooks", () => {
  for (const page of ["guides/index.html", "es/guides/index.html"]) {
    const html = read(page);
    assert.match(html, /id="(?:model|modelo)"/);
    assert.match(html, /Large v3 Turbo Q5/);
    assert.match(html, /~\/\.kuali/);
    assert.match(html, /id="webhooks"/);
    assert.match(html, /Standard Webhooks/);
    assert.match(html, /meeting\.completed/);
    assert.match(html, /webhook\.test/);
    assert.match(html, /whsec_/);
    assert.match(html, /webhook-id/);
    assert.match(html, /webhook-timestamp/);
    assert.match(html, /webhook-signature/);
    assert.match(html, /<code>type<\/code>[\s\S]*<code>timestamp<\/code>[\s\S]*<code>data<\/code>/);
    assert.match(html, /20 MB/);
    assert.match(html, /600 MB/);
    assert.match(html, /Apple Silicon/);
  }
});

test("Discord setup is a three-step token-driven authorization flow", () => {
  const variants = [
    { page: "guides/index.html", oldCopy: /Enable Guild Install|Configure the install link and scopes/, automaticCopy: /obtains the application ID from the token/ },
    { page: "es/guides/index.html", oldCopy: /Activa Instalación de servidor|Configura el enlace y los ámbitos/, automaticCopy: /obtiene el ID de la aplicación desde el token/ },
  ];

  for (const { page, oldCopy, automaticCopy } of variants) {
    const section = read(page).match(/<section class="guide-section" id="discord"[\s\S]*?<\/section>/)?.[0];
    assert.ok(section, `${page} is missing the Discord guide`);
    assert.equal((section.match(/<article class="guide-step">/g) ?? []).length, 3);
    assert.equal((section.match(/data-lightbox-source/g) ?? []).length, 3);
    assert.match(section, /copy-username\.webp/);
    assert.match(section, /Attach Files|Adjuntar archivos/);
    assert.match(section, /Embed Links|Insertar enlaces/);
    assert.match(section, automaticCopy);
    assert.match(section, /applications\.commands/);
    assert.doesNotMatch(section, oldCopy);
  }
});

test("platform support is explicit in both languages", () => {
  const englishHome = read("index.html");
  const spanishHome = read("es/index.html");
  assert.match(englishHome, /Microsoft Teams <small>Experimental · partial<\/small>/);
  assert.match(englishHome, /Zoom <small>Experimental · partial<\/small>/);
  assert.match(spanishHome, /Microsoft Teams <small>Experimental · parcial<\/small>/);
  assert.match(spanishHome, /Zoom <small>Experimental · parcial<\/small>/);
  assert.match(read("guides/index.html"), /Google Meet is stable[\s\S]*Microsoft Teams and Zoom are experimental/);
  assert.match(read("es/guides/index.html"), /Google Meet es estable[\s\S]*Microsoft Teams y Zoom son experimentales/);
});

test("only experimental platform dots use the orange status color", () => {
  const styles = read("assets/site.css");
  assert.match(styles, /--experimental: #f59e0b/);
  assert.match(styles, /\.platform-badge\.experimental i \{[\s\S]*?background: var\(--experimental\)/);
  assert.doesNotMatch(styles, /\.platform-badge\.experimental small/);
  for (const page of publicPages) assert.match(read(page), /site\.css\?v=20260811/);
});

test("platform pages target real search intents with substantive product details", () => {
  const pages = {
    "discord-meeting-transcription/index.html": [/Discord meeting transcription/i, /participant identity/i, /Standard Webhooks/i, /Whisper/i],
    "es/transcripcion-reuniones-discord/index.html": [/Transcripción de Discord/i, /identidad/i, /Standard Webhooks/i, /Whisper/i],
    "google-meet-transcription/index.html": [/Google Meet transcription/i, /loopback/i, /participant/i, /Whisper/i],
    "es/transcripcion-google-meet/index.html": [/Transcripción de Google Meet/i, /loopback/i, /participante/i, /Whisper/i],
  };

  for (const [page, patterns] of Object.entries(pages)) {
    const html = read(page);
    assert.ok(html.length > 9000, `${page} is too thin to serve its search intent`);
    for (const pattern of patterns) assert.match(html, pattern);
  }
});

test("sitemap contains every canonical page once with language alternates", () => {
  const sitemap = read("sitemap.xml");
  const locations = [...sitemap.matchAll(/<loc>([^<]+)<\/loc>/g)].map((match) => match[1]);
  const canonicals = publicPages.map(canonicalFor);
  assert.deepEqual(new Set(locations), new Set(canonicals));
  assert.equal(locations.length, publicPages.length);
  assert.equal((sitemap.match(/hreflang="x-default"/g) ?? []).length, publicPages.length);
});

test("Cloudflare Workers is the only website deployment target", () => {
  const wrangler = JSON.parse(readRepository("wrangler.jsonc").replace(/^\s*\/\/.*$/gm, ""));
  assert.equal(wrangler.name, "kuali-site");
  assert.equal(wrangler.account_id, "d86563bfb9f7e428720f1b7b01a0b348");
  assert.equal(wrangler.assets.directory, "./website");
  assert.equal(wrangler.assets.not_found_handling, "404-page");
  assert.ok(existsSync(join(websiteRoot, ".assetsignore")));
  assert.match(read(".assetsignore"), /tests\//);
  assert.ok(!existsSync(join(websiteRoot, ".nojekyll")));
  assert.ok(!existsSync(join(repositoryRoot, ".github/workflows/pages.yml")));
  assert.doesNotMatch(readRepository("WEBSITE.md"), /GitHub Pages|gh-pages/i);
  assert.match(readRepository("WEBSITE.md"), /Cloudflare Worker/);
});

test("deployment metadata, security policy, and duplicate-host indexing rules are present", () => {
  assert.doesNotThrow(() => JSON.parse(read("site.webmanifest")));
  assert.match(read("robots.txt"), /Sitemap: https:\/\/kuali\.garrux\.dev\/sitemap\.xml/);
  assert.match(read("_headers"), /Content-Security-Policy:/);
  assert.match(read("_headers"), /frame-ancestors 'none'/);
  assert.match(read("_headers"), /workers\.dev\/\*/);
  assert.match(read("_headers"), /X-Robots-Tag: noindex, nofollow/);
  assert.match(read("404.html"), /<meta name="robots" content="noindex, follow">/);
});

test("the public interface includes baseline keyboard and motion accessibility", () => {
  for (const page of publicPages) {
    const html = read(page);
    const skipTarget = html.match(/<a class="skip-link" href="#([^"]+)"/)?.[1];
    assert.ok(skipTarget, `${page} has no skip link`);
    assert.match(html, new RegExp(`id="${skipTarget}"`), `${page} skip target is missing`);
  }

  const styles = read("assets/site.css");
  assert.match(styles, /:focus-visible/);
  assert.match(styles, /@media \(prefers-reduced-motion: reduce\)/);
  assert.match(styles, /touch-action: manipulation/);
  assert.match(styles, /overscroll-behavior: contain/);
  assert.doesNotMatch(styles, /transition:\s*all/);
  assert.doesNotMatch(styles, /outline:\s*none/);
  assert.match(read("assets/site.js"), /aria-live", "polite"/);
});

test("all deployable files fit Cloudflare static asset limits", () => {
  const maxAssetSize = 25 * 1024 * 1024;
  for (const file of allFiles(websiteRoot)) {
    assert.ok(statSync(file).size <= maxAssetSize, `${file} exceeds 25 MiB`);
  }
});

test("the site has no remote runtime dependencies", () => {
  assert.doesNotMatch(read("assets/site.css"), /@import|url\(["']?https?:/i);
  for (const page of allHtmlPages) {
    for (const source of attributeValues(read(page), "src")) {
      assert.ok(!/^https?:/.test(source), `${page} loads remote script or image ${source}`);
    }
  }
  assert.equal(extname(join(websiteRoot, "assets/site.js")), ".js");
});

test("machine-readable project summary points to canonical product and contributor docs", () => {
  const summary = read("llms.txt");
  assert.match(summary, /^# Kuali/m);
  assert.match(summary, /https:\/\/kuali\.garrux\.dev\/discord-meeting-transcription\//);
  assert.match(summary, /https:\/\/kuali\.garrux\.dev\/google-meet-transcription\//);
  assert.match(summary, /https:\/\/github\.com\/igarrux\/kuali\/blob\/main\/CONTRIBUTING\.md/);
  assert.match(summary, /native desktop core is written in Rust/);
  assert.match(summary, /about 20 MB while waiting/);
  assert.match(summary, /up to about 600 MB/);
  assert.match(summary, /free and open source/);
});

test("repository readmes document the on-demand model lifecycle", () => {
  const english = readRepository("README.md");
  const spanish = readRepository("README.es.md");

  assert.match(english, /## Resource use/);
  assert.match(english, /Waiting for a meeting \| No \| About 20 MB/);
  assert.match(english, /recommended Q5 model \| Yes \| Up to about 600 MB/);
  assert.match(english, /Downloaded\s+weights remain on disk/);
  assert.match(spanish, /## Uso de recursos/);
  assert.match(spanish, /Esperando una reunión \| No \| Cerca de 20 MB/);
  assert.match(spanish, /modelo Q5 recomendado \| Sí \| Hasta unos 600 MB/);
  assert.match(spanish, /pesos descargados permanecen en disco/i);
});
