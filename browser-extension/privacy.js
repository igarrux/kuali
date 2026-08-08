const spanish = (chrome.i18n?.getUILanguage?.() || "en").toLowerCase().startsWith("es");
document.documentElement.lang = spanish ? "es-419" : "en";
document.title = spanish ? "Política de privacidad de Kuali" : "Kuali Privacy Policy";
document.getElementById("policy-en").hidden = spanish;
document.getElementById("policy-es").hidden = !spanish;
