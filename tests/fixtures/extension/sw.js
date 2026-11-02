// Service worker of the hba test extension.
const started = new Date().toISOString();
console.log("hba sw started", started);

chrome.runtime.onInstalled.addListener(({ reason }) => {
  console.info("hba installed:", reason);
  chrome.storage.local.set({ installed: reason, clicks: 0 });
});

chrome.runtime.onMessage.addListener((message, sender, reply) => {
  console.log("hba message", JSON.stringify(message), "from", sender.url);
  if (message.type === "fail") {
    // Thrown from an event listener: an uncaught runtime error Helium records for the extension.
    throw new Error("hba fixture failure: " + message.why);
  }
  if (message.type === "click") {
    chrome.storage.local.get({ clicks: 0 }).then(({ clicks }) => {
      chrome.storage.local.set({ clicks: clicks + 1 });
      reply({ clicks: clicks + 1 });
    });
    return true;
  }
  reply({ ok: true, started });
  return false;
});

/** Throws on the next task, outside any caller: for testing error capture from `hba ext eval`. */
self.hbaFail = (why = "requested") => {
  setTimeout(() => {
    throw new Error("hba fixture failure: " + why);
  });
  return "scheduled";
};
