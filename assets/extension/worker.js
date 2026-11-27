// hba evaluates `group(...)` here over DevTools. The DevTools protocol cannot group tabs; only
// the extension API can. `debugger` is needed only for `chrome.debugger.getTargets`, which maps
// DevTools page targets to tab ids without attaching to anything.

/** Moves the tabs of `pageTargets` into the group titled `title` in their window, creating it
 * where missing. Tabs already in a group stay there: the user put them there, or the browser did,
 * because their opener was in it. Returns how many tabs moved. */
globalThis.group = async (pageTargets, title) => {
  const wanted = new Set(pageTargets);
  const tabIds = (await chrome.debugger.getTargets())
    .filter((target) => wanted.has(target.id) && target.tabId !== undefined)
    .map((target) => target.tabId);
  let moved = 0;
  for (const tabId of tabIds) {
    const tab = await chrome.tabs.get(tabId);
    if (tab.groupId !== chrome.tabGroups.TAB_GROUP_ID_NONE || tab.pinned) continue;
    // Popup and app windows have no tab groups.
    if ((await chrome.windows.get(tab.windowId)).type !== "normal") continue;
    const groups = await chrome.tabGroups.query({ windowId: tab.windowId });
    const existing = groups.find((candidate) => candidate.title === title);
    if (existing) {
      await chrome.tabs.group({ groupId: existing.id, tabIds: [tabId] });
    } else {
      const groupId = await chrome.tabs.group({ createProperties: { windowId: tab.windowId }, tabIds: [tabId] });
      await chrome.tabGroups.update(groupId, { title });
    }
    moved += 1;
  }
  return moved;
};
