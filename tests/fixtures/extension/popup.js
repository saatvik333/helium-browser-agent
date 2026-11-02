const output = document.getElementById("clicks");

chrome.storage.local.get({ clicks: 0 }).then(({ clicks }) => {
  output.value = clicks;
});

document.getElementById("click").addEventListener("click", async () => {
  const { clicks } = await chrome.runtime.sendMessage({ type: "click" });
  output.value = clicks;
  console.log("hba popup clicks", clicks);
});

document.getElementById("fail").addEventListener("click", () => {
  chrome.runtime.sendMessage({ type: "fail", why: "popup button" }).catch((error) => {
    console.warn("hba popup: worker failed:", error.message);
  });
});

console.log("hba popup opened");
