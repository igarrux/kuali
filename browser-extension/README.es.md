# Extensión de Kuali para el navegador

[English](README.md) · **Español**

Conecta reuniones de Google Meet, Microsoft Teams y Zoom con la aplicación
local de Kuali. Tú sigues siendo un participante normal; no entra una cuenta de
bot externa.

## Instalar para probar

1. Abre `chrome://extensions` (o `edge://extensions`).
2. Activa **Modo desarrollador**.
3. Pulsa **Cargar descomprimida** y elige esta carpeta `browser-extension`.
4. Abre Kuali, entra a una reunión, pulsa el icono de Kuali y elige
   **Grabar y transcribir** después de leer el aviso y confirmar que informaste
   a los participantes.

Por defecto sólo conecta con `ws://127.0.0.1:9099`. El popup permite cambiar el
puerto. El audio viaja en PCM mono a 16 kHz; el ID, nombre, foto y tipo de pista
del participante viajan por el mismo WebSocket vinculados a su canal.
Puedes capturar varias pestañas compatibles al mismo tiempo. Cada una recibe su
propia sesión y transcripción; la app comparte una sola copia de Whisper entre
ellas y cualquier llamada activa de Discord.

## E2E real con Google Meet

Desde esta carpeta ejecuta:

```sh
npm run test:e2e:meet
```

La primera ejecución descarga Chrome for Testing oficial dentro de
`target/e2e`, levanta el motor real de Kuali detrás de una sonda transparente y
abre Meet con la extensión en un perfil descartable. No modifica tu perfil
normal, no llama al proveedor de resúmenes, no dispara webhooks y elimina de la
biblioteca la reunión creada por la prueba.

El modo completo necesita un segundo participante —otro perfil o tu teléfono—
para comprobar la separación remota, ID, nombre, foto, micrófono local,
transcripción en vivo, atribución final, cierre limpio y descarga de Whisper de
la RAM. Para una prueba rápida de una sola persona usa
`npm run test:e2e:meet -- --solo`. El reporte JSON queda en
`target/e2e/reports`.

## Separación real y mezcla

- Meet observa los frames Opus codificados antes de que la web los consuma,
  decodifica una copia con WebCodecs y enruta cada frame por su CSRC. Las tres
  pistas virtuales de transporte pueden cambiar de dueño, pero el canal de cada
  participante en Kuali permanece estable.
- La identidad viene de la colección interna de participantes cuando está
  disponible. Como respaldo, Kuali relaciona una sola vez el CSRC con una ficha
  tras varios cientos de milisegundos de voz decodificada inequívoca; el audio
  posterior nunca persigue el brillo visual.
- Teams usa pistas WebRTC individuales cuando la web las expone.
- Zoom intenta lo mismo. Si ese modo no expone pistas WebRTC, un `tabCapture`
  aislado conserva la reunión como un canal `mixed`. El fallback se descarta tan
  pronto como aparece una pista individual, para no transcribir ambas rutas.

Meet envía actualmente tres pistas virtuales con los hablantes más relevantes.
Kuali conserva identidades estables en reuniones con más participantes conforme
van tomando turnos y separa hasta tres voces remotas simultáneas, pero no puede
extraer una cuarta voz concurrente que Meet no haya enviado al navegador. Como
las plataformas pueden cambiar su DOM y protocolos privados, esta integración
se valida con el E2E real.

## Publicación en Chrome Web Store

Ejecuta `npm run package:store` para crear en `dist` el ZIP mínimo y su
SHA-256. Los textos, declaraciones de privacidad, instrucciones para revisión,
gráficos y checklist del panel están en [`store`](store/README.es.md). La
instalación manual seguirá disponible para desarrollo y mientras Google revisa
la versión de tienda.

## Privacidad

La captura exige confirmar que informaste a los participantes. Mientras está
activa se muestran un indicador en la página y la insignia `REC`. El audio sólo
va a la app Kuali en `127.0.0.1` y la extensión no conserva ningún archivo de
audio. Consulta la [política completa](../PRIVACY.es.md).

## Licencia

Esta carpeta usa Apache-2.0 y está separada del escritorio MIT. Consulta
[LICENSE](LICENSE) y [NOTICE](NOTICE). El contrato de captura y parte del enfoque
provienen de Vexa; `NOTICE` fija la revisión exacta y conserva la atribución.
