import { test, expect, Page } from "@playwright/test";
import { CLOSE_VIEWER_LABEL, closeViewer, waitForApp, panicsOf } from "./example-room";
import { callRiverTest } from "./river-test";

// freenet/river#975 step 2: join a public room from inside the read-only view
// with proof of work, and post. The example build has no network, so the three
// fixture rooms (open: 4 bits, hard: 26 bits, full: one slot already held by a
// newer message) are resolved locally. Joining makes the room a member room in
// ROOMS; with no node there is nothing to publish to, so what these tests check
// is the visitor's own side: the queue, the single post, the move into the
// room list, and the clear failure states. The example build keeps no state
// across a reload, so "reload keeps membership" is a native test of the
// persisted room blob (public_join::tests), not a spec here. "Exactly once" is
// counted through the roomMessageCount hook (the message bodies in ROOMS), not
// DOM text, and the hard room's solver is held by a hook so cancel and ban
// tests cannot race a lucky proof.

type Kind = "open" | "hard" | "full";

// The app is up and its test hooks are installed (they appear after first render).
async function hooksReady(page: Page) {
  await page.waitForFunction(() => typeof (window as any).__riverTest?.viewableRoomKey === "function");
}

async function openRoom(page: Page, kind: Kind) {
  await page.goto("/");
  await waitForApp(page);
  await hooksReady(page);
  const key = await callRiverTest(page, "viewableRoomKey", kind);
  expect(key.length).toBeGreaterThan(30);
  await page.goto(`/?room=${key}`);
  await waitForApp(page);
  await expect(page.getByTestId("viewed-room-name")).toBeVisible();
}

// The first Send of a visitor asks for a display name (one prompt, prefilled).
// Continue without typing sends under the generated default, as a visitor who
// skips it does. Pass `name` to choose one.
async function sendFirst(page: Page, name?: string) {
  await page.getByTestId("viewed-room-send").click();
  const input = page.getByTestId("viewed-room-nickname-input");
  await expect(input).toBeVisible();
  if (name !== undefined) await input.fill(name);
  await page.getByTestId("viewed-room-nickname-continue").click();
  await expect(page.getByTestId("viewed-room-nickname-modal")).toHaveCount(0);
}

test("the composer is enabled and Send before admission queues, then posts exactly once", async ({ page }) => {
  const panics = panicsOf(page);
  await openRoom(page, "open");
  const composer = page.getByTestId("viewed-room-composer");
  await expect(composer).toBeEnabled();
  // No Join button, no read-only label: a visitor just writes.
  await expect(page.getByTestId("viewed-room-join")).toHaveCount(0);
  await expect(page.getByTestId("viewed-room")).not.toContainText("read-only");

  await composer.fill("Hello from a visitor");
  await sendFirst(page);

  // The room becomes a member room: the viewer is gone, the room is selected.
  await expect(page.getByTestId("viewed-room")).toHaveCount(0, { timeout: 30_000 });
  await expect(page.getByTestId("send-message-button")).toBeVisible();
  // The message is there once.
  expect(await callRiverTest(page, "roomMessageCount", "Hello from a visitor")).toBe(1);
  expect(await callRiverTest(page, "joinSessionKey")).toBe("");
  expect(panics).toEqual([]);
});

test("typing starts the proof quietly, and the first message makes the visitor a member", async ({ page }) => {
  const panics = panicsOf(page);
  await openRoom(page, "open");
  // Opening the link starts nothing: no key exists until the visitor interacts.
  expect(await callRiverTest(page, "joinSessionKey")).toBe("");
  await page.getByTestId("viewed-room-composer").fill("First post");
  await expect.poll(() => callRiverTest(page, "joinSessionKey")).not.toBe("");
  // Nothing is posted, nothing is announced, and the viewer is still a viewer.
  await expect(page.getByTestId("viewed-room")).toBeVisible();
  await expect(page.getByTestId("viewed-room-pending-message")).toHaveCount(0);
  await expect(page.getByTestId("viewed-room-sending-notice")).toHaveCount(0);

  await sendFirst(page);
  await expect(page.getByTestId("viewed-room")).toHaveCount(0, { timeout: 30_000 });
  expect(await callRiverTest(page, "roomMessageCount", "First post")).toBe(1);
  expect(panics).toEqual([]);
});

test("a proof that finishes while the name prompt is open does not post under the default name", async ({ page }) => {
  const panics = panicsOf(page);
  await openRoom(page, "open");
  await callRiverTest(page, "holdSolver", true);
  const composer = page.getByTestId("viewed-room-composer");
  await composer.fill("Slow proof");
  await composer.press("Enter");
  // A second Enter before focus reaches the prompt must not skip it.
  await composer.press("Enter");
  const input = page.getByTestId("viewed-room-nickname-input");
  await expect(input).toBeVisible();
  // The proof finishes while the visitor is still choosing a name.
  await callRiverTest(page, "holdSolver", false);
  // Wait for the event, not the clock: the proof is done, nothing was queued.
  await expect.poll(() => callRiverTest(page, "joinPhase")).toBe("ready");
  await expect(page.getByTestId("viewed-room-nickname-modal")).toBeVisible();
  await expect(page.getByTestId("viewed-room")).toBeVisible();
  expect(await callRiverTest(page, "roomMessageCount", "Slow proof")).toBe(0);
  await input.fill("Quinn");
  await page.getByTestId("viewed-room-nickname-continue").click();
  await expect(page.getByTestId("viewed-room")).toHaveCount(0, { timeout: 30_000 });
  expect(await callRiverTest(page, "roomMessageCount", "Slow proof")).toBe(1);
  expect(await callRiverTest(page, "currentRoomSelfNickname")).toBe("Quinn");
  expect(panics).toEqual([]);
});

test("Continue without typing posts under the suggested name", async ({ page }) => {
  const panics = panicsOf(page);
  await openRoom(page, "open");
  await page.getByTestId("viewed-room-composer").fill("Skip the name");
  await page.getByTestId("viewed-room-send").click();
  const input = page.getByTestId("viewed-room-nickname-input");
  await expect(input).toBeVisible();
  const suggested = await input.inputValue();
  expect(suggested.length).toBeGreaterThan(0);
  await page.getByTestId("viewed-room-nickname-continue").click();
  await expect(page.getByTestId("viewed-room")).toHaveCount(0, { timeout: 30_000 });
  expect(await callRiverTest(page, "currentRoomSelfNickname")).toBe(suggested);
  expect(panics).toEqual([]);
});

test("Escape at the name prompt cancels: nothing is sent and the text stays in the box", async ({ page }) => {
  const panics = panicsOf(page);
  await openRoom(page, "open");
  const composer = page.getByTestId("viewed-room-composer");
  await composer.fill("Not yet");
  await page.getByTestId("viewed-room-send").click();
  // Focus moves into the dialog a moment after it opens; Escape acts from there.
  await expect(page.getByTestId("viewed-room-nickname-input")).toBeFocused();
  await page.keyboard.press("Escape");
  await expect(page.getByTestId("viewed-room-nickname-modal")).toHaveCount(0);
  await expect(composer).toHaveValue("Not yet");
  await expect(page.getByTestId("viewed-room-pending-message")).toHaveCount(0);
  // Give a wrongly queued message time to post before checking it did not.
  await expect.poll(() => callRiverTest(page, "joinPhase")).toBe("ready");
  await expect(page.getByTestId("viewed-room")).toBeVisible();
  expect(await callRiverTest(page, "roomMessageCount", "Not yet")).toBe(0);
  // Sending again asks again, and then sends.
  await page.getByTestId("viewed-room-send").click();
  await expect(page.getByTestId("viewed-room-nickname-input")).toBeVisible();
  await page.getByTestId("viewed-room-nickname-continue").click();
  await expect(page.getByTestId("viewed-room")).toHaveCount(0, { timeout: 30_000 });
  expect(await callRiverTest(page, "roomMessageCount", "Not yet")).toBe(1);
  expect(panics).toEqual([]);
});

test("Tab follows the focused control even after a click inside the dialog", async ({ page }) => {
  await openRoom(page, "hard");
  await callRiverTest(page, "holdSolver", true);
  await page.getByTestId("viewed-room-composer").fill("Hello");
  await page.getByTestId("viewed-room-send").click();
  const input = page.getByLabel("Your display name");
  const cont = page.getByTestId("viewed-room-nickname-continue");
  await cont.focus();
  // Click the input, then Tab: focus must go to Continue, not stay or skip.
  await input.click();
  await page.keyboard.press("Tab");
  await expect(cont).toBeFocused();
  await page.keyboard.press("Tab");
  await expect(input).toBeFocused();
});

test("the name prompt is a labelled dialog that keeps focus, and its buttons are touch sized", async ({ page }) => {
  await openRoom(page, "hard");
  await callRiverTest(page, "holdSolver", true);
  await page.getByTestId("viewed-room-composer").fill("Hello");
  await page.getByTestId("viewed-room-send").click();
  const modal = page.getByTestId("viewed-room-nickname-modal");
  await expect(modal).toHaveAttribute("role", "dialog");
  await expect(modal).toHaveAttribute("aria-modal", "true");
  const labelled = await modal.getAttribute("aria-labelledby");
  await expect(page.locator(`#${labelled}`)).toContainText("display name");
  // The label is tied to the input, and the input has focus.
  await expect(page.getByLabel("Your display name")).toBeFocused();
  // Tab stays inside the dialog.
  await page.keyboard.press("Tab");
  await expect(page.getByTestId("viewed-room-nickname-continue")).toBeFocused();
  await page.keyboard.press("Tab");
  await expect(page.getByLabel("Your display name")).toBeFocused();
  const box = await page.getByTestId("viewed-room-nickname-continue").boundingBox();
  expect(box!.height).toBeGreaterThanOrEqual(44);
  // Closing it (Escape cancels) puts focus back in the composer.
  await expect(modal).toContainText("Escape cancels");
  await page.keyboard.press("Escape");
  await expect(page.getByTestId("viewed-room-composer")).toBeFocused();
});

test("a ban that lands while the name prompt is open is shown, and Continue sends again", async ({ page }) => {
  const panics = panicsOf(page);
  await openRoom(page, "hard");
  await callRiverTest(page, "holdSolver", true);
  await page.getByTestId("viewed-room-composer").fill("Still here");
  await page.getByTestId("viewed-room-send").click();
  await expect(page.getByTestId("viewed-room-nickname-modal")).toBeVisible();
  await expect.poll(() => callRiverTest(page, "banJoiner", "hard")).toBe(true);
  await expect(page.getByTestId("viewed-room-join-error")).toContainText("banned", { timeout: 30_000 });
  // The text was never queued, so nothing is lost; answering sends it afresh.
  await page.getByTestId("viewed-room-nickname-continue").click();
  await expect(page.getByTestId("viewed-room-pending-message")).toContainText("Still here");
  expect(await callRiverTest(page, "roomMessageCount", "Still here")).toBe(0);
  expect(panics).toEqual([]);
});

test("the way out is labelled and visible: the rail x on a wide screen, the header x on a narrow one", async ({ page }) => {
  await openRoom(page, "open");
  const wide = (page.viewportSize()?.width ?? 0) >= 768;
  const rail = page.getByTestId("viewed-room-close");
  const narrow = page.getByTestId("viewed-room-close-narrow");
  const shown = wide ? rail : narrow;
  const hidden = wide ? narrow : rail;
  await expect(shown).toBeVisible();
  await expect(hidden).toBeHidden();
  await expect(shown).toHaveAttribute("aria-label", CLOSE_VIEWER_LABEL);
  await expect(shown).toHaveAttribute("title", CLOSE_VIEWER_LABEL);
  if (!wide) {
    const box = await narrow.boundingBox();
    expect(box!.width).toBeGreaterThanOrEqual(44);
    expect(box!.height).toBeGreaterThanOrEqual(44);
  }
  // The tab is named for the room that is open, not the one selected before.
  const roomName = (await page.getByTestId("viewed-room-name").textContent())!;
  await expect(page).toHaveTitle(new RegExp(roomName));
  // Only the viewed room's row is highlighted in the rail.
  await expect(page.getByTestId("viewed-room-rail-row")).toHaveCount(1);
  await closeViewer(page);
  await expect(page.getByTestId("viewed-room")).toHaveCount(0);
  // ... and the title goes back once the viewer is closed.
  await expect(page).not.toHaveTitle(new RegExp(roomName));
});

test("when the user's rooms failed to load, a visitor is told to reload and nothing starts", async ({ page }) => {
  const panics = panicsOf(page);
  await openRoom(page, "open");
  await callRiverTest(page, "failRoomsLoad");
  const composer = page.getByTestId("viewed-room-composer");
  await composer.fill("Might double up");
  await page.getByTestId("viewed-room-send").click();
  await expect(page.getByTestId("viewed-room-join-error")).toContainText("Reload this page");
  // Nothing was queued or posted, no key was made, and the text is kept.
  await expect(page.getByTestId("viewed-room-pending-message")).toHaveCount(0);
  await expect(composer).toHaveValue("Might double up");
  expect(await callRiverTest(page, "joinSessionKey")).toBe("");
  expect(await callRiverTest(page, "roomMessageCount", "Might double up")).toBe(0);
  await expect(page.getByTestId("viewed-room")).toBeVisible();
  expect(panics).toEqual([]);
});

test("the display name chosen at the first message is the one posted", async ({ page }) => {
  const panics = panicsOf(page);
  await openRoom(page, "open");
  await page.getByTestId("viewed-room-composer").fill("Named post");
  await sendFirst(page, "Quinn");
  await expect(page.getByTestId("viewed-room")).toHaveCount(0, { timeout: 30_000 });
  expect(await callRiverTest(page, "currentRoomSelfNickname")).toBe("Quinn");
  expect(panics).toEqual([]);
});

test("a queued message is shown as pending and is not lost when the join is cancelled", async ({ page }) => {
  const panics = panicsOf(page);
  await openRoom(page, "hard");
  await callRiverTest(page, "holdSolver", true);
  await page.getByTestId("viewed-room-composer").fill("Keep this draft");
  await sendFirst(page);

  // Pending, not posted: shown apart from the room's messages, with a quiet
  // delay notice and Cancel. No talk of bots or proofs.
  await expect(page.getByTestId("viewed-room-pending-message")).toContainText("Keep this draft");
  await expect(page.getByTestId("viewed-room-sending-notice")).toContainText("Sending in a moment");
  await expect(page.getByTestId("viewed-room-sending-notice")).toContainText("spam");
  await expect(page.getByTestId("viewed-room-sending-notice")).not.toContainText(/proof|bot|bits/i);
  await expect(page.getByTestId("viewed-room")).toBeVisible();

  await page.getByTestId("viewed-room-join-cancel").click();
  await expect(page.getByTestId("viewed-room-pending-message")).toHaveCount(0);
  await expect(page.getByTestId("viewed-room-composer")).toHaveValue("Keep this draft");
  // No key outlives the cancel, and nothing was posted anywhere.
  expect(await callRiverTest(page, "joinSessionKey")).toBe("");
  expect(await callRiverTest(page, "roomMessageCount", "Keep this draft")).toBe(0);
  // Nothing was posted and the visitor is not a member.
  await expect(page.getByTestId("viewed-room")).toBeVisible();
  await expect(page.getByTestId("viewed-room-join")).toHaveCount(0);
  expect(panics).toEqual([]);
});

test("a banned key is told so, and the draft comes back", async ({ page }) => {
  const panics = panicsOf(page);
  await openRoom(page, "hard");
  await callRiverTest(page, "holdSolver", true);
  await page.getByTestId("viewed-room-composer").fill("Hello");
  await sendFirst(page);
  await expect(page.getByTestId("viewed-room-sending-notice")).toBeVisible();

  await expect.poll(() => callRiverTest(page, "banJoiner", "hard")).toBe(true);
  await expect(page.getByTestId("viewed-room-join-error")).toContainText("banned", { timeout: 30_000 });
  await expect(page.getByTestId("viewed-room-composer")).toHaveValue("Hello");
  await expect(page.getByTestId("viewed-room-pending-message")).toHaveCount(0);
  expect(await callRiverTest(page, "joinSessionKey")).toBe("");
  expect(panics).toEqual([]);
});

test("a full room says so, offers a retry later, and keeps the draft", async ({ page }) => {
  const panics = panicsOf(page);
  await openRoom(page, "full");
  await page.getByTestId("viewed-room-composer").fill("Too late");
  await sendFirst(page);

  await expect(page.getByTestId("viewed-room-join-error")).toContainText("no free spot", { timeout: 30_000 });
  await expect(page.getByTestId("viewed-room-composer")).toHaveValue("Too late");
  // Still a viewer, nothing posted, and the room was not joined.
  await expect(page.getByTestId("viewed-room")).toBeVisible();
  await expect(page.getByTestId("viewed-room-pending-message")).toHaveCount(0);
  expect(await callRiverTest(page, "joinSessionKey")).toBe("");
  // Try again is offered and starts over (the error clears); sending the kept
  // text into the still-full room says so again, with the text kept.
  await page.getByTestId("viewed-room-join").click();
  await expect(page.getByTestId("viewed-room-join-error")).toHaveCount(0);
  await page.getByTestId("viewed-room-send").click();
  await expect(page.getByTestId("viewed-room-join-error")).toContainText("no free spot", { timeout: 30_000 });
  await expect(page.getByTestId("viewed-room-composer")).toHaveValue("Too late");
  expect(panics).toEqual([]);
});

test("a private room and a public room without open admission offer no Join", async ({ page }) => {
  await page.goto("/");
  await waitForApp(page);
  await hooksReady(page);
  const key = await callRiverTest(page, "viewableRoomKey", "public");
  await page.goto(`/?room=${key}`);
  await waitForApp(page);
  await expect(page.getByTestId("viewed-room-join")).toHaveCount(0);
  await expect(page.getByTestId("viewed-room-composer")).toBeDisabled();
  await expect(page.getByTestId("viewed-room-composer-note")).toContainText("invitation");
});

for (const forget of [false, true]) {
  test(`a member the room pruned re-admits on the next message and it posts once (${forget ? "proof solved again" : "held proof re-submitted"})`, async ({ page }) => {
    const panics = panicsOf(page);
    await openRoom(page, "open");
    await page.getByTestId("viewed-room-composer").fill("Joined");
    await sendFirst(page);
    await expect(page.getByTestId("viewed-room")).toHaveCount(0, { timeout: 30_000 });
    expect(await callRiverTest(page, "roomMessageCount", "Joined")).toBe(1);
    const keyBefore = await callRiverTest(page, "currentRoomSelfKey");
    expect(keyBefore.length).toBeGreaterThan(30);
    const nameBefore = await callRiverTest(page, "currentRoomSelfNickname");
    expect(nameBefore.length).toBeGreaterThan(0);
    // The first message was authored by that key.
    expect(await callRiverTest(page, "messageAuthorKey", "Joined")).toBe(keyBefore);

    // The room's cleanup drops the member (and, with `forget`, a reload has
    // lost the proof). The normal composer is replaced by the re-admission one.
    expect(await callRiverTest(page, "pruneJoiner", forget)).toBe(true);
    expect(await callRiverTest(page, "roomMessageCount", "Joined")).toBe(0);
    await expect(page.getByTestId("send-message-button")).toHaveCount(0);
    await expect(page.getByTestId("viewed-room-join-idle")).toContainText("newer activity pushed yours out");

    await page.getByTestId("viewed-room-composer").fill("Back again");
    await page.getByTestId("viewed-room-send").click();
    // It posts once, under the same identity, and the composer returns.
    await expect.poll(() => callRiverTest(page, "roomMessageCount", "Back again"), { timeout: 30_000 }).toBe(1);
    await expect(page.getByTestId("readmit-pending-message")).toHaveCount(0);
    // Same identity: the room's own key was reused, not a new one made.
    expect(await callRiverTest(page, "currentRoomSelfKey")).toBe(keyBefore);
    // ... and the new message was AUTHORED by that same key, with the same nickname.
    expect(await callRiverTest(page, "messageAuthorKey", "Back again")).toBe(keyBefore);
    expect(await callRiverTest(page, "currentRoomSelfNickname")).toBe(nameBefore);
    await expect(page.getByTestId("send-message-button")).toBeVisible();
    expect(panics).toEqual([]);
  });
}

test("messages sent while solving are all queued, shown pending, and post exactly once each", async ({ page }) => {
  const panics = panicsOf(page);
  await openRoom(page, "open");
  await callRiverTest(page, "holdSolver", true);
  const composer = page.getByTestId("viewed-room-composer");
  await composer.fill("first queued");
  await sendFirst(page);
  await expect(page.getByTestId("viewed-room-sending-notice")).toBeVisible();
  await composer.fill("second queued");
  await composer.press("Enter");
  await expect(page.getByTestId("viewed-room-pending-message")).toHaveCount(2);
  await expect(composer).toHaveValue("");

  await callRiverTest(page, "holdSolver", false);
  await expect(page.getByTestId("viewed-room")).toHaveCount(0, { timeout: 30_000 });
  await expect.poll(() => callRiverTest(page, "roomMessageCount", "first queued")).toBe(1);
  await expect.poll(() => callRiverTest(page, "roomMessageCount", "second queued")).toBe(1);
  expect(panics).toEqual([]);
});

test("a message too long for the room is refused before it is queued, and kept in the box", async ({ page }) => {
  const panics = panicsOf(page);
  await openRoom(page, "open");
  const long = "x".repeat(5000);
  await page.getByTestId("viewed-room-composer").fill(long);
  await sendFirst(page);
  await expect(page.getByTestId("viewed-room-join-notice")).toContainText("longer than the room allows");
  await expect(page.getByTestId("viewed-room-composer")).toHaveValue(long);
  await expect(page.getByTestId("viewed-room-sending-notice")).toHaveCount(0);
  // Typing started the proof quietly, but nothing was queued or posted.
  await expect(page.getByTestId("viewed-room-pending-message")).toHaveCount(0);
  expect(await callRiverTest(page, "roomMessageCount", long)).toBe(0);
  expect(panics).toEqual([]);
});

test("closing the viewer mid-solve keeps the queued text and posts nothing", async ({ page }) => {
  const panics = panicsOf(page);
  await openRoom(page, "open");
  await callRiverTest(page, "holdSolver", true);
  await page.getByTestId("viewed-room-composer").fill("do not lose me");
  await sendFirst(page);
  await expect(page.getByTestId("viewed-room-pending-message")).toHaveCount(1);
  await closeViewer(page);
  await expect(page.getByTestId("viewed-room")).toHaveCount(0);
  await callRiverTest(page, "holdSolver", false);
  expect(await callRiverTest(page, "joinSessionKey")).toBe("");
  expect(await callRiverTest(page, "roomMessageCount", "do not lose me")).toBe(0);
  // Reopening the room gives the text back in the composer.
  await callRiverTest(page, "viewRoom", "open");
  await expect(page.getByTestId("viewed-room-name")).toBeVisible();
  await expect(page.getByTestId("viewed-room-composer")).toHaveValue("do not lose me");
  await expect(page.getByTestId("viewed-room-pending-message")).toHaveCount(0);
  expect(panics).toEqual([]);
});

test("a room the user already holds cannot be joined over", async ({ page }) => {
  const panics = panicsOf(page);
  await page.goto("/");
  await waitForApp(page);
  await hooksReady(page);
  const key = await callRiverTest(page, "viewableRoomKey", "member");
  await page.goto(`/?room=${key}`);
  await waitForApp(page);
  // Opening a room the user is in shows that room, not a visitor's view of it.
  await expect(page.getByTestId("viewed-room")).toHaveCount(0);
  await expect(page.getByTestId("viewed-room-composer")).toHaveCount(0);
  await expect(page.getByTestId("message-composer").first()).toBeVisible();
  expect(await callRiverTest(page, "currentRoomOwnerKey")).toBe(key);
  await expect(page.getByTestId("viewed-room-rail-row")).toHaveCount(0);
  expect(await callRiverTest(page, "joinSessionKey")).toBe("");
  expect(panics).toEqual([]);
});

test("a draft typed while the join solves is in the member room's composer afterwards", async ({ page }) => {
  const panics = panicsOf(page);
  await openRoom(page, "open");
  await callRiverTest(page, "holdSolver", true);
  const composer = page.getByTestId("viewed-room-composer");
  await composer.fill("sent one");
  await sendFirst(page);
  await expect(page.getByTestId("viewed-room-pending-message")).toHaveCount(1);
  await composer.fill("typed but not sent");
  await callRiverTest(page, "holdSolver", false);
  await expect(page.getByTestId("viewed-room")).toHaveCount(0, { timeout: 30_000 });
  expect(await callRiverTest(page, "roomMessageCount", "sent one")).toBe(1);
  expect(await callRiverTest(page, "roomMessageCount", "typed but not sent")).toBe(0);
  await expect(page.locator("#message-input")).toHaveValue("typed but not sent");
  expect(panics).toEqual([]);
});

test("a draft typed while a re-admission solves is in the member composer afterwards", async ({ page }) => {
  const panics = panicsOf(page);
  await openRoom(page, "open");
  await page.getByTestId("viewed-room-composer").fill("Joined");
  await sendFirst(page);
  await expect(page.getByTestId("viewed-room")).toHaveCount(0, { timeout: 30_000 });
  expect(await callRiverTest(page, "pruneJoiner", true)).toBe(true);
  await expect(page.getByTestId("viewed-room-join-idle")).toContainText("newer activity pushed yours out");

  await callRiverTest(page, "holdSolver", true);
  const composer = page.getByTestId("viewed-room-composer");
  await composer.fill("Back");
  await page.getByTestId("viewed-room-send").click();
  await expect(page.getByTestId("viewed-room-join-progress")).toBeVisible();
  await composer.fill("meanwhile");
  await callRiverTest(page, "holdSolver", false);
  await expect.poll(() => callRiverTest(page, "roomMessageCount", "Back"), { timeout: 30_000 }).toBe(1);
  await expect(page.locator("#message-input")).toHaveValue("meanwhile");
  expect(panics).toEqual([]);
});

test("switching to another room mid-solve leaves that room idle and returns the first room's text", async ({ page }) => {
  const panics = panicsOf(page);
  await openRoom(page, "open");
  await callRiverTest(page, "holdSolver", true);
  await page.getByTestId("viewed-room-composer").fill("for the open room");
  await sendFirst(page);
  await expect(page.getByTestId("viewed-room-pending-message")).toHaveCount(1);

  await callRiverTest(page, "viewRoom", "hard");
  await expect(page.getByTestId("viewed-room-name")).toHaveText("Hard Join Room");
  // The other room shows nothing pending and no delay notice.
  await expect(page.getByTestId("viewed-room-sending-notice")).toHaveCount(0);
  await expect(page.getByTestId("viewed-room-pending-message")).toHaveCount(0);
  await expect(page.getByTestId("viewed-room-composer")).toHaveValue("");

  // Sending there starts that room's own join; the first room's text goes back to it.
  await page.getByTestId("viewed-room-composer").fill("for the hard room");
  await sendFirst(page);
  await expect(page.getByTestId("viewed-room-pending-message")).toHaveCount(1);
  await expect(page.getByTestId("viewed-room-pending-message")).toContainText("for the hard room");
  await callRiverTest(page, "viewRoom", "open");
  await expect(page.getByTestId("viewed-room-name")).toHaveText("Open Join Room");
  await expect(page.getByTestId("viewed-room-composer")).toHaveValue("for the open room");
  await expect(page.getByTestId("viewed-room-pending-message")).toHaveCount(0);
  await callRiverTest(page, "holdSolver", false);
  expect(await callRiverTest(page, "roomMessageCount", "for the open room")).toBe(0);
  expect(panics).toEqual([]);
});
