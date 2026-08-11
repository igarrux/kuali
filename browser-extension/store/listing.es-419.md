# Ficha en español latinoamericano

## Nombre

Kuali

## Resumen

Transcribe Google Meet localmente con Kuali. Teams y Zoom tienen soporte experimental y parcial.

## Descripción detallada

Kuali convierte reuniones del navegador en notas buscables y atribuidas por
hablante, sin añadir un bot de grabación a la llamada.

Google Meet tiene soporte estable. Microsoft Teams y Zoom tienen soporte
experimental y parcial mientras sus integraciones reciben más pruebas reales.

Inicia la captura desde la barra, revisa qué datos se procesarán y confirma que
informaste a los participantes. Kuali envía el audio directamente a la app de
escritorio del mismo equipo. Whisper transcribe localmente y un indicador
permanece visible hasta que se detenga la captura.

Funciones:

- transcripción en vivo dentro de la app Kuali;
- nombres y canales separados cuando la plataforma los expone;
- captura simultánea de varias reuniones compatibles;
- cierre automático al salir o terminar la reunión;
- biblioteca local, búsqueda, resúmenes, decisiones, preguntas y tareas; y
- proveedores de IA y webhooks opcionales configurados por el usuario.

Se necesita la app Kuali, gratuita y open source. La extensión sólo se conecta
a Kuali mediante `127.0.0.1`, no tiene una nube operada por Kuali y no conserva
el audio capturado como archivo. La transcripción sólo sale del equipo si
configuras un proveedor de resúmenes, una entrega por Discord o un webhook.

Los servicios de reuniones pueden cambiar sus interfaces privadas. Teams y Zoom
pueden tener captura, identidad o separación de hablantes incompletas en algunos
modos. Kuali etiqueta el audio mezclado cuando no puede separarlo en vez de
atribuirlo a la persona equivocada.

Informa siempre a los participantes y obtén el consentimiento exigido en tu
ubicación. Kuali no está afiliado con Google, Microsoft ni Zoom.

Código, descargas, documentación y privacidad:
https://github.com/igarrux/kuali
