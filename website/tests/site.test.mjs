import assert from "node:assert/strict";
import { existsSync, readFileSync, readdirSync, statSync } from "node:fs";
import { dirname, extname, join, resolve } from "node:path";
import test from "node:test";
import { fileURLToPath } from "node:url";

const websiteRoot = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const pages = ["index.html", "es/index.html", "guides/index.html", "es/guides/index.html"];

function read(relativePath) {
  return readFileSync(join(websiteRoot, relativePath), "utf8");
}

function attributeValues(html, attribute) {
  return [...html.matchAll(new RegExp(`\\b${attribute}="([^"]+)"`, "g"))].map((match) => match[1]);
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

test("all public pages contain complete SEO metadata", () => {
  for (const page of pages) {
    const html = read(page);
    const title = html.match(/<title>([^<]+)<\/title>/)?.[1] ?? "";
    const description = html.match(/<meta name="description" content="([^"]+)">/)?.[1] ?? "";

    assert.ok(title.length >= 30 && title.length <= 70, `${page} title length is ${title.length}`);
    assert.ok(description.length >= 110 && description.length <= 180, `${page} description length is ${description.length}`);
    assert.match(html, /<meta name="robots" content="index, follow, max-image-preview:large">/);
    assert.match(html, /<link rel="canonical" href="https:\/\/kuali\.garrux\.dev\//);
    assert.equal((html.match(/rel="alternate" hreflang=/g) ?? []).length, 3, `${page} hreflang count`);
    assert.match(html, /<meta property="og:title"/);
    assert.match(html, /<meta property="og:description"/);
    assert.match(html, /<meta property="og:image"/);
    assert.match(html, /<meta name="twitter:card" content="summary_large_image">/);
    assert.equal((html.match(/<h1\b/g) ?? []).length, 1, `${page} must contain one h1`);
  }
});

test("structured data is valid JSON", () => {
  for (const page of pages) {
    const blocks = [...read(page).matchAll(/<script type="application\/ld\+json">([\s\S]*?)<\/script>/g)];
    assert.ok(blocks.length >= 1, `${page} has no JSON-LD`);
    for (const [, block] of blocks) {
      const value = JSON.parse(block);
      assert.equal(value["@context"], "https://schema.org");
      assert.ok(value["@type"]);
    }
  }
});

test("local links and assets resolve from every deployment depth", () => {
  for (const page of pages) {
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
  for (const page of pages) {
    const html = read(page);
    const ids = attributeValues(html, "id");
    assert.equal(new Set(ids).size, ids.length, `${page} contains duplicate IDs`);
    assert.doesNotMatch(html, /\son(?:click|load|error)=/i, `${page} contains inline event handlers`);
    assert.doesNotMatch(html, /\sstyle="/i, `${page} contains inline styles`);

    for (const image of html.matchAll(/<img\b([^>]*)>/g)) {
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
  for (const page of pages) {
    const html = read(page);
    assert.ok(html.includes(install), `${page} is missing Homebrew installation`);
    assert.ok(html.includes(quarantine), `${page} is missing explicit quarantine command`);
    assert.doesNotMatch(html, /postflight|--no-quarantine|automatically removes the quarantine/i);
  }
});

test("the landing pages show the real app and state the local data boundary", () => {
  const english = read("index.html");
  const spanish = read("es/index.html");

  assert.match(english, /assets\/kuali-app\.png/);
  assert.match(spanish, /assets\/kuali-app\.es\.png/);
  assert.doesNotMatch(english + spanish, /local-flow/);
  assert.match(english, /Everything Kuali creates lives on your PC/);
  assert.match(spanish, /Todo lo que Kuali genera vive en tu PC/);
  for (const html of [english, spanish]) {
    assert.match(html, />LLM</);
    assert.match(html, />Webhook</);
    assert.match(html, /Jhon Guerrero/);
    assert.doesNotMatch(html, /©|Kuali contributors|Colaboradores de Kuali/);
  }
});

test("download actions lead to installation while the secondary hero action opens GitHub", () => {
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
  }
});

test("Discord setup is a three-step token-driven authorization flow", () => {
  const variants = [
    {
      page: "guides/index.html",
      oldCopy: /Enable Guild Install|Configure the install link and scopes/,
      automaticCopy: /obtains the application ID from the token/,
    },
    {
      page: "es/guides/index.html",
      oldCopy: /Activa Instalación de servidor|Configura el enlace y los ámbitos/,
      automaticCopy: /obtiene el ID de la aplicación desde el token/,
    },
  ];

  for (const { page, oldCopy, automaticCopy } of variants) {
    const html = read(page);
    const section = html.match(/<section class="guide-section" id="discord"[\s\S]*?<\/section>/)?.[0];
    assert.ok(section, `${page} is missing the Discord guide`);
    assert.equal((section.match(/<article class="guide-step">/g) ?? []).length, 3);
    assert.equal((section.match(/data-lightbox-source/g) ?? []).length, 2);
    assert.match(section, automaticCopy);
    assert.match(section, /applications\.commands/);
    assert.doesNotMatch(section, oldCopy);
  }
});

test("platform support is explicit in both languages", () => {
  const englishHome = read("index.html");
  const spanishHome = read("es/index.html");
  const englishGuide = read("guides/index.html");
  const spanishGuide = read("es/guides/index.html");

  assert.match(englishHome, /Microsoft Teams <small>Experimental · partial<\/small>/);
  assert.match(englishHome, /Zoom <small>Experimental · partial<\/small>/);
  assert.match(spanishHome, /Microsoft Teams <small>Experimental · parcial<\/small>/);
  assert.match(spanishHome, /Zoom <small>Experimental · parcial<\/small>/);
  assert.match(englishGuide, /Google Meet is stable[\s\S]*Microsoft Teams and Zoom are experimental/);
  assert.match(spanishGuide, /Google Meet es estable[\s\S]*Microsoft Teams y Zoom son experimentales/);
});

test("only experimental platform dots use the orange status color", () => {
  const styles = read("assets/site.css");

  assert.match(styles, /--experimental: #f59e0b/);
  assert.match(styles, /\.platform-badge\.experimental i \{[\s\S]*?background: var\(--experimental\)/);
  assert.doesNotMatch(styles, /\.platform-badge\.experimental small/);
  for (const page of pages) {
    assert.match(read(page), /site\.css\?v=20260811/);
  }
});

test("deployment metadata and security policy are present", () => {
  assert.ok(existsSync(join(websiteRoot, ".nojekyll")));
  assert.doesNotThrow(() => JSON.parse(read("site.webmanifest")));
  assert.match(read("robots.txt"), /Sitemap: https:\/\/kuali\.garrux\.dev\/sitemap\.xml/);
  assert.equal((read("sitemap.xml").match(/<url>/g) ?? []).length, pages.length);
  assert.match(read("_headers"), /Content-Security-Policy:/);
  assert.match(read("_headers"), /frame-ancestors 'none'/);
});

test("all deployable files fit Cloudflare Pages asset limits", () => {
  const maxAssetSize = 25 * 1024 * 1024;
  for (const file of allFiles(websiteRoot)) {
    assert.ok(statSync(file).size <= maxAssetSize, `${file} exceeds 25 MiB`);
  }
});

test("the site has no remote runtime dependencies", () => {
  assert.doesNotMatch(read("assets/site.css"), /@import|url\(["']?https?:/i);
  for (const page of pages) {
    for (const source of attributeValues(read(page), "src")) {
      assert.ok(!/^https?:/.test(source), `${page} loads remote script or image ${source}`);
    }
  }
  assert.equal(extname(join(websiteRoot, "assets/site.js")), ".js");
});
