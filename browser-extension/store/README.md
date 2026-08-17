# Chrome Web Store publishing guide

**English** · [Español](README.es.md)

This directory contains the copy, disclosure answers, reviewer instructions,
and graphics needed to submit the Kuali browser extension. The upload ZIP is
created by `npm run package:store`; do not ZIP the whole repository.

## Before submitting

1. Make the repository and `PRIVACY.md` publicly reachable. The dashboard
   privacy-policy URL is:
   `https://github.com/igarrux/kuali/blob/main/PRIVACY.md`.
2. Publish a signed Kuali desktop build under GitHub Releases. Reviewers need
   the companion app because the extension intentionally has no cloud backend.
3. Register a Chrome Web Store developer account and pay Google's one-time
   registration fee.
4. Run `npm test` and `npm run package:store` in `browser-extension`.
5. Upload `dist/kuali-chrome-0.1.5.zip` in the Developer Dashboard.

## Dashboard fields

- **Product name:** Kuali
- **Category:** Productivity
- **Language:** English (United States)
- **Visibility:** Public
- **Homepage:** `https://github.com/igarrux/kuali`
- **Support URL:** `https://github.com/igarrux/kuali/issues`
- **Privacy policy:** `https://github.com/igarrux/kuali/blob/main/PRIVACY.md`
- **Official URL:** leave empty unless Kuali later has a verified website
- **Mature content:** No
- **In-app purchases:** No

Paste the English and Spanish store copy from `listing.en.md` and
`listing.es-419.md`. Upload the PNGs from `assets`; the three selected 1280×800
screenshots can be regenerated from `video/storyboard.html` whenever the UI
changes.

## Privacy tab

Use the exact declarations and permission justifications in
`privacy-answers.md`. Read them again before every release: the answers must
describe the uploaded code, not an older version.

## Reviewer instructions

Paste `reviewer-notes.md` into the testing-instructions field. Replace the
release placeholder with a direct, public download before submitting. No test
account is required, but the reviewer needs a supported meeting and permission
from its participants.

## Release discipline

- A Web Store version cannot be reused. Increment `manifest.json` and
  `package.json` together for every upload after `0.1.0`.
- Keep the Apache-2.0 `LICENSE` and Vexa attribution `NOTICE` in every package.
- Do not add remote scripts, `eval`, broad host permissions, analytics, or a
  new data destination without updating the policy and dashboard declarations.
- Keep the unpacked-install instructions in the extension README for
  development and while a store release is under review.

Google controls the final approval. New extensions and permissions involving
meeting capture can receive additional review, so submit well before a planned
release date.
