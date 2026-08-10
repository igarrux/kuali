import { spawnSync } from "node:child_process";
import { mkdirSync, rmSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

const root = dirname(fileURLToPath(import.meta.url));
const storyboard = join(root, "storyboard.html");
const rendered = join(root, "rendered");
const frames = join(rendered, "scenes");
const clips = join(rendered, "clips");
const output = join(root, "output", "kuali-extension-promo-en.mp4");
const chrome = "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome";
const fps = 30;
const transitionDuration = 0.85;

const scenes = [
  { id: "intro", duration: 5.5, transition: "fade" },
  { id: "suggest", duration: 7, transition: "smoothleft" },
  { id: "consent", duration: 7, transition: "fade" },
  { id: "live-one", duration: 6.5, transition: "smoothleft" },
  { id: "live-two", duration: 6.5, transition: "fade" },
  { id: "summary", duration: 7, transition: "smoothleft" },
  { id: "tasks", duration: 6, transition: "fade" },
  { id: "local", duration: 7, transition: "smoothup" },
  { id: "privacy", duration: 6.5, transition: "fade" },
  { id: "outro", duration: 6, transition: "fade" },
];

function run(command, args) {
  const result = spawnSync(command, args, { cwd: root, stdio: "inherit" });
  if (result.error) throw result.error;
  if (result.status !== 0) throw new Error(`${command} exited with status ${result.status}`);
}

rmSync(rendered, { recursive: true, force: true });
mkdirSync(frames, { recursive: true });
mkdirSync(clips, { recursive: true });
mkdirSync(dirname(output), { recursive: true });

for (const scene of scenes) {
  const screenshot = join(frames, `${scene.id}.png`);
  const url = new URL(pathToFileURL(storyboard));
  url.searchParams.set("scene", scene.id);
  run(chrome, [
    "--headless=new",
    "--disable-gpu",
    "--disable-extensions",
    "--hide-scrollbars",
    "--no-first-run",
    "--force-device-scale-factor=1",
    "--window-size=1920,1080",
    `--screenshot=${screenshot}`,
    url.href,
  ]);

  const clip = join(clips, `${scene.id}.mp4`);
  const frameCount = Math.round(scene.duration * fps);
  const horizontalDrift = scenes.indexOf(scene) % 2 === 0
    ? "iw/2-(iw/zoom/2)"
    : "iw/2-(iw/zoom/2)+8*sin(on/45)";
  const zoom = scenes.indexOf(scene) % 2 === 0
    ? "min(zoom+0.00022,1.028)"
    : "if(eq(on,1),1.028,max(1.0,zoom-0.00022))";
  run("ffmpeg", [
    "-y",
    "-loop", "1",
    "-framerate", String(fps),
    "-i", screenshot,
    "-vf", `zoompan=z='${zoom}':x='${horizontalDrift}':y='ih/2-(ih/zoom/2)':d=${frameCount}:s=1920x1080:fps=${fps},format=yuv420p`,
    "-frames:v", String(frameCount),
    "-an",
    "-c:v", "libx264",
    "-preset", "medium",
    "-crf", "16",
    clip,
  ]);
}

const ffmpegInputs = scenes.flatMap((scene) => ["-i", join(clips, `${scene.id}.mp4`)]);
const filters = [];
let cumulative = scenes[0].duration;
let previous = "0:v";
for (let index = 1; index < scenes.length; index += 1) {
  const outputLabel = `v${index}`;
  const offset = cumulative - transitionDuration * index;
  filters.push(
    `[${previous}][${index}:v]xfade=transition=${scenes[index].transition}:duration=${transitionDuration}:offset=${offset.toFixed(2)}[${outputLabel}]`,
  );
  previous = outputLabel;
  cumulative += scenes[index].duration;
}
const totalDuration = scenes.reduce((sum, scene) => sum + scene.duration, 0)
  - transitionDuration * (scenes.length - 1);
const audioInputIndex = scenes.length;

run("ffmpeg", [
  "-y",
  ...ffmpegInputs,
  "-f", "lavfi",
  "-t", totalDuration.toFixed(2),
  "-i", "anullsrc=channel_layout=stereo:sample_rate=48000",
  "-filter_complex", filters.join(";"),
  "-map", `[${previous}]`,
  "-map", `${audioInputIndex}:a`,
  "-t", totalDuration.toFixed(2),
  "-r", String(fps),
  "-c:v", "libx264",
  "-preset", "slow",
  "-crf", "17",
  "-pix_fmt", "yuv420p",
  "-movflags", "+faststart",
  "-c:a", "aac",
  "-b:a", "128k",
  "-shortest",
  output,
]);

console.log(`Created ${output}`);
