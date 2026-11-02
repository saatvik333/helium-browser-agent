// Content script of the hba test extension (isolated world on 127.0.0.1:8765/ext-* pages).
var hbaContent = {
  extension: chrome.runtime.id,
  version: chrome.runtime.getManifest().version,
  title: document.title,
};
console.log("hba content script on", location.pathname);
