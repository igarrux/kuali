# Guía de publicación en Chrome Web Store

**Español** · [English](README.md)

Esta carpeta contiene los textos, declaraciones, instrucciones para revisión y
gráficos necesarios para publicar la extensión Kuali. El ZIP se crea con
`npm run package:store`; no comprimas el repositorio completo.

## Antes de enviarla

1. Haz públicos el repositorio y `PRIVACY.md`. La URL de política para el panel
   será `https://github.com/igarrux/kuali/blob/main/PRIVACY.md`.
2. Publica una versión firmada de la app Kuali en GitHub Releases. El revisor
   necesita la app compañera porque la extensión no usa un backend de Kuali.
3. Registra una cuenta de desarrollador de Chrome Web Store y paga la cuota
   única de Google.
4. Ejecuta `npm test` y `npm run package:store` dentro de `browser-extension`.
5. Sube `dist/kuali-chrome-0.1.5.zip` en el panel de desarrolladores.

## Campos del panel

- **Nombre del producto:** Kuali
- **Categoría:** Productividad
- **Idioma predeterminado:** Inglés (Estados Unidos)
- **Visibilidad:** Pública
- **Página principal:** `https://github.com/igarrux/kuali`
- **Soporte:** `https://github.com/igarrux/kuali/issues`
- **Política de privacidad:**
  `https://github.com/igarrux/kuali/blob/main/PRIVACY.md`
- **URL oficial:** déjala vacía mientras Kuali no tenga un sitio verificado
- **Contenido para adultos:** No
- **Compras integradas:** No

Copia las fichas de `listing.en.md` y `listing.es-419.md`, y sube los PNG de
`assets`. Las tres capturas seleccionadas de 1280×800 se pueden regenerar desde
`video/storyboard.html` cuando cambie la interfaz.

Usa las declaraciones exactas de `privacy-answers.md` y las instrucciones de
`reviewer-notes.md`. Antes de enviar, reemplaza el enlace de descarga indicado
por una versión pública y directa de la app.

Cada nueva subida necesita un número de versión distinto. Conserva siempre la
licencia Apache-2.0 y el `NOTICE` de Vexa, y mantén la instalación manual del
README mientras la versión de tienda esté en revisión.
