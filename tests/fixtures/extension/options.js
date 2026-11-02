const greeting = document.getElementById("greeting");
const status = document.getElementById("status");

chrome.storage.sync.get({ greeting: "" }).then((saved) => {
  greeting.value = saved.greeting;
});

document.getElementById("options").addEventListener("submit", async (event) => {
  event.preventDefault();
  await chrome.storage.sync.set({ greeting: greeting.value });
  status.textContent = "Saved";
  console.log("hba options saved", greeting.value);
});
