// Content script of the hba test extension (isolated world on the fixture site's ext-* pages).
var hbaContent = {
  extension: chrome.runtime.id,
  version: chrome.runtime.getManifest().version,
  title: document.title,
};
console.log("hba content script on", location.pathname);
