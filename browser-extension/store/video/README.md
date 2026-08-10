# Kuali extension promo video

This directory contains the reproducible English promotional video used for
the Chrome Web Store listing. Every meeting, participant, transcript, and task
shown in the storyboard is fictional.

## Render

Requirements:

- Google Chrome at `/Applications/Google Chrome.app`
- Node.js 22 or newer
- FFmpeg with `libx264` and AAC support

Run from the repository root:

```sh
node browser-extension/store/video/render.mjs
```

The final 1080p file is written to:

```text
browser-extension/store/video/output/kuali-extension-promo-en.mp4
```

Intermediate screenshots and clips are generated under `rendered/`. Both the
intermediate files and final MP4 are ignored by Git because they can be rebuilt
from `storyboard.html` and `render.mjs`.

## Chrome Web Store screenshots

Generate the selected 1280×800, 24-bit RGB PNG screenshots with:

```sh
node browser-extension/store/video/render-store-images.mjs
```

The three files are written to `browser-extension/store/assets/` without an
alpha channel.
