import { spawnSync } from "node:child_process";
import { mkdirSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

const root = dirname(fileURLToPath(import.meta.url));
const storyboard = join(root, "storyboard.html");
const assets = join(root, "..", "assets");
const chrome = "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome";
const images = [
  ["suggest", "screenshot-suggest.png"],
  ["consent", "screenshot-consent-demo.png"],
  ["local", "screenshot-local-demo.png"],
];

mkdirSync(assets, { recursive: true });

for (const [scene, filename] of images) {
  const url = new URL(pathToFileURL(storyboard));
  url.searchParams.set("scene", scene);
  url.searchParams.set("format", "store");
  const result = spawnSync(chrome, [
    "--headless=new",
    "--disable-gpu",
    "--disable-extensions",
    "--hide-scrollbars",
    "--no-first-run",
    "--force-device-scale-factor=1",
    "--window-size=1280,800",
    `--screenshot=${join(assets, filename)}`,
    url.href,
  ], { cwd: root, stdio: "inherit" });
  if (result.error) throw result.error;
  if (result.status !== 0) throw new Error(`Chrome exited with status ${result.status}`);
}

console.log(`Created ${images.length} Chrome Web Store screenshots in ${assets}`);
