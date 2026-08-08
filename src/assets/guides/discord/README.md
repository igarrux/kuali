# Discord guide screenshots

Place the Spanish and English screenshots for Kuali's step-by-step Discord
guide in this directory. The app chooses the matching set from the interface
language and hides an example automatically until its file exists.

Spanish filenames:

1. `discord-02-new-application.png`
2. `discord-03-reset-token.png`
3. `discord-04-guild-install.png`
4. `discord-05-install-link-scopes.png`
5. `discord-06-bot-permissions.png`

English filenames use the same name with the `en-` prefix:

1. `en-discord-02-new-application.png`
2. `en-discord-03-reset-token.png`
3. `en-discord-04-guild-install.png`
4. `en-discord-05-install-link-scopes.png`
5. `en-discord-06-bot-permissions.png`

Capture recommendations:

- Capture one set with Discord's Spanish interface and one with its English
  interface. Keep both sets framed the same way whenever possible.
- Crop around the relevant panel or control, while leaving enough surrounding
  interface to make its location clear.
- Capture at the display's native resolution and keep the PNG lossless. Prefer
  2560 × 1440 px when available; do not resize or recompress it before saving.
  Kuali scales the thumbnail without cropping and exposes the full-size image
  in its viewer.
- Never include a bot token. Hide or redact tokens, usernames, server names,
  avatars, email addresses, and any other private data before saving.
- Keep the cursor away from the control being demonstrated.

What each image should show:

- `02`: the **Nueva aplicación** button in the Developer Portal.
- `03`: **Bot → Restablecer token**. Do not show the resulting token.
- `04`: **Instalaciones → Contextos de instalación → Seleccionar métodos**,
  with **Instalación de servidor** selected.
- `05`: **Enlace proporcionado por Discord** and the `bot` plus
  `applications.commands` scopes under **Ámbitos**.
- `06`: the six minimum bot permissions selected, with **Administrador** off.
