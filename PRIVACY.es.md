# Política de privacidad de Kuali

**Fecha de vigencia:** 8 de agosto de 2026
**English:** [PRIVACY.md](PRIVACY.md)

Kuali es una aplicación open source y autohospedada para transcribir reuniones.
Esta política cubre la aplicación de escritorio, el bot de Discord y la
extensión de navegador Kuali distribuidos desde este repositorio.

## La versión corta

Kuali no opera cuentas, publicidad, analítica ni un servicio de transcripción
en la nube. La extensión envía el audio y los datos de los participantes
únicamente a la aplicación Kuali que corre en tu propio equipo. Whisper
transcribe el audio localmente. Kuali no vende información personal ni conserva
el audio capturado como un archivo de audio.

Si configuras un proveedor de resúmenes, entregas por Discord o un webhook, la
aplicación envía a ese servicio los datos descritos abajo porque tú se lo
pediste. Cada servicio tiene su propia política de privacidad.

## Información que procesa Kuali

Cuando eliges grabar y transcribir una reunión compatible, Kuali puede procesar:

- audio en vivo de la reunión;
- plataforma, identificador, título de la pestaña y horas de inicio y fin;
- nombre visible, usuario, ID de la plataforma, URL de avatar, estado de
  silencio y actividad de voz de cada participante, además de si su canal de
  audio es separado o mezclado; y
- la transcripción atribuida por hablante, valores de confianza, resumen,
  puntos clave, decisiones, preguntas abiertas y tareas.

En llamadas de Discord, la app puede procesar la información equivalente de
servidor, canal, participante, voz y entrega de mensajes necesaria para entrar,
atribuir hablantes, anunciar la grabación y entregar resultados.

Kuali también guarda los ajustes que introduces. Estos pueden incluir el token
del bot de Discord, usuario o ID de Discord, ubicación de modelos, vocabulario
especial, proveedor de resumen y sus claves de API, y URLs y secretos de firma
de webhooks. La extensión sólo guarda el puerto local usado para encontrar la
aplicación de escritorio.

## Para qué se usa

Kuali utiliza estos datos exclusivamente para:

- capturar una reunión que inicias explícitamente;
- mostrar participantes y transcripción en vivo;
- crear y buscar un registro local de la reunión;
- generar título, resumen, decisiones, preguntas y tareas;
- entregar resultados a Discord o webhooks configurados por ti; y
- diagnosticar la captura y mantener separadas las reuniones simultáneas.

Kuali no usa los datos de las reuniones para publicidad, decisiones de crédito,
perfilado ni entrenamiento de un modelo operado por Kuali.

## A dónde va la información

En reuniones web, los datos viajan desde la página hacia la extensión y luego,
mediante un WebSocket de loopback (`127.0.0.1`), a la aplicación Kuali del mismo
equipo. El servicio local rechaza los orígenes de páginas web normales. La
extensión no envía estos datos a un servidor operado por Kuali.

Whisper transcribe localmente. Según lo que configures:

- un proveedor OpenAI, Anthropic, Gemini, compatible con OpenAI o de línea de
  comandos recibe los metadatos, nombres visibles y transcripción necesarios
  para producir el resumen;
- Discord recibe avisos, resúmenes, tareas y transcripciones que solicites; y
- cada webhook habilitado recibe la reunión terminada, incluyendo metadatos,
  participantes, transcripción, resumen y tareas.

Kuali no habilita estos destinos por ti. Revisa los términos de privacidad de
cada servicio antes de configurarlo.

## Almacenamiento, conservación y eliminación

La app guarda las reuniones en el directorio de datos de Kuali del sistema
operativo y los ajustes en su directorio de configuración. Los pesos de los
modelos se guardan en `~/.kuali` de forma predeterminada o en la carpeta que
elijas. En sistemas Unix, Kuali escribe la configuración con permisos exclusivos
del propietario (`0600`); los secretos no tienen cifrado adicional en reposo.

Las reuniones permanecen hasta que las elimines desde Kuali o borres sus
archivos. Los pesos permanecen hasta que los elimines desde Kuali. Al quitar la
extensión, el navegador elimina su preferencia de puerto conforme a su manejo
normal de datos de extensiones. Los datos ya entregados a Discord, un proveedor
o un webhook quedan bajo el control de ese servicio y deben borrarse allí.

El audio PCM capturado se procesa en memoria y Kuali no lo conserva como
archivo. El audio temporal desaparece al terminar el procesamiento o cerrar la
aplicación.

## Aviso y consentimiento

Kuali exige que quien inicia la captura confirme que informó a los participantes
y tiene permiso para grabar y transcribir. También muestra un indicador
persistente. Las leyes y reglas de las plataformas cambian según el lugar; la
persona que opera Kuali es responsable de obtener todos los consentimientos
necesarios.

## Seguridad

Kuali limita el puente del navegador a loopback, restringe los orígenes del
navegador a extensiones de Chrome, solicita acceso sólo a las plataformas
compatibles y distribuye todo el código de la extensión dentro del paquete.
Como Kuali corre en tu equipo, quien tenga acceso a tu cuenta, archivos, tokens
o destinos configurados podría acceder a la información de las reuniones.

## Uso limitado de Chrome Web Store

El uso y transferencia que Kuali hace de información recibida de servicios de
Google cumple la Política de Datos de Usuario de Chrome Web Store, incluidos sus
requisitos de Uso Limitado. Kuali usa los datos de Google Meet sólo para las
funciones visibles de captura, transcripción, biblioteca, resumen y entrega que
describe esta política.

## Cambios y contacto

Los cambios importantes se documentarán aquí y se actualizará la fecha de
vigencia. Puedes enviar preguntas, solicitudes de privacidad o reportes de
seguridad en <https://github.com/igarrux/kuali/issues>. No incluyas contenido de
reuniones, tokens, claves de API ni otros secretos en una incidencia pública.
