import { test, expect, Page } from "@playwright/test";
import { waitForApp, panicsOf } from "./example-room";
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

test("the composer is enabled and Send before admission queues, then posts exactly once", async ({ page }) => {
  const panics = panicsOf(page);
  await openRoom(page, "open");
  const composer = page.getByTestId("viewed-room-composer");
  await expect(composer).toBeEnabled();
  await expect(page.getByTestId("viewed-room-join")).toBeVisible();

  await composer.fill("Hello from a visitor");
  await page.getByTestId("viewed-room-send").click();

  // The room becomes a member room: the viewer is gone, the room is selected.
  await expect(page.getByTestId("viewed-room")).toHaveCount(0, { timeout: 30_000 });
  await expect(page.getByTestId("send-message-button")).toBeVisible();
  // The message is there once.
  expect(await callRiverTest(page, "roomMessageCount", "Hello from a visitor")).toBe(1);
  expect(await callRiverTest(page, "joinSessionKey")).toBe("");
  expect(panics).toEqual([]);
});

test("Join alone proves the work, then the first message makes the visitor a member", async ({ page }) => {
  const panics = panicsOf(page);
  await openRoom(page, "open");
  await page.getByTestId("viewed-room-join").click();
  await expect(page.getByTestId("viewed-room-join-ready")).toBeVisible({ timeout: 30_000 });
  // Nothing is posted and the viewer is still a viewer.
  await expect(page.getByTestId("viewed-room")).toBeVisible();
  await expect(page.getByTestId("viewed-room-pending-message")).toHaveCount(0);

  await page.getByTestId("viewed-room-composer").fill("First post");
  await page.getByTestId("viewed-room-send").click();
  await expect(page.getByTestId("viewed-room")).toHaveCount(0, { timeout: 30_000 });
  expect(await callRiverTest(page, "roomMessageCount", "First post")).toBe(1);
  expect(panics).toEqual([]);
});

test("a queued message is shown as pending and is not lost when the join is cancelled", async ({ page }) => {
  const panics = panicsOf(page);
  await openRoom(page, "hard");
  await callRiverTest(page, "holdSolver", true);
  await page.getByTestId("viewed-room-composer").fill("Keep this draft");
  await page.getByTestId("viewed-room-send").click();

  // Pending, not posted: shown apart from the room's messages, with Cancel.
  await expect(page.getByTestId("viewed-room-pending-message")).toContainText("Keep this draft");
  await expect(page.getByTestId("viewed-room-join-progress")).toBeVisible();
  await expect(page.getByTestId("viewed-room")).toBeVisible();

  await page.getByTestId("viewed-room-join-cancel").click();
  await expect(page.getByTestId("viewed-room-pending-message")).toHaveCount(0);
  await expect(page.getByTestId("viewed-room-composer")).toHaveValue("Keep this draft");
  // No key outlives the cancel, and nothing was posted anywhere.
  expect(await callRiverTest(page, "joinSessionKey")).toBe("");
  expect(await callRiverTest(page, "roomMessageCount", "Keep this draft")).toBe(0);
  // Nothing was posted and the visitor is not a member.
  await expect(page.getByTestId("viewed-room")).toBeVisible();
  await expect(page.getByTestId("viewed-room-join")).toBeVisible();
  expect(panics).toEqual([]);
});

test("a banned key is told so, and the draft comes back", async ({ page }) => {
  const panics = panicsOf(page);
  await openRoom(page, "hard");
  await callRiverTest(page, "holdSolver", true);
  await page.getByTestId("viewed-room-composer").fill("Hello");
  await page.getByTestId("viewed-room-send").click();
  await expect(page.getByTestId("viewed-room-join-progress")).toBeVisible();

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
  await page.getByTestId("viewed-room-send").click();

  await expect(page.getByTestId("viewed-room-join-error")).toContainText("open-member slots", { timeout: 30_000 });
  await expect(page.getByTestId("viewed-room-composer")).toHaveValue("Too late");
  // Still a viewer, nothing posted, and the room was not joined.
  await expect(page.getByTestId("viewed-room")).toBeVisible();
  await expect(page.getByTestId("viewed-room-pending-message")).toHaveCount(0);
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
  await expect(page.getByTestId("viewed-room-composer-note")).toContainText("needs an invitation");
});

for (const forget of [false, true]) {
  test(`a member the room pruned re-admits on the next message and it posts once (${forget ? "proof solved again" : "held proof re-submitted"})`, async ({ page }) => {
    const panics = panicsOf(page);
    await openRoom(page, "open");
    await page.getByTestId("viewed-room-composer").fill("Joined");
    await page.getByTestId("viewed-room-send").click();
    await expect(page.getByTestId("viewed-room")).toHaveCount(0, { timeout: 30_000 });
    expect(await callRiverTest(page, "roomMessageCount", "Joined")).toBe(1);
    const keyBefore = await callRiverTest(page, "currentRoomSelfKey");
    expect(keyBefore.length).toBeGreaterThan(30);

    // The room's cleanup drops the member (and, with `forget`, a reload has
    // lost the proof). The normal composer is replaced by the re-admission one.
    expect(await callRiverTest(page, "pruneJoiner", forget)).toBe(true);
    expect(await callRiverTest(page, "roomMessageCount", "Joined")).toBe(0);
    await expect(page.getByTestId("send-message-button")).toHaveCount(0);
    await expect(page.getByTestId("viewed-room-join-idle")).toContainText("removed you for being quiet");

    await page.getByTestId("viewed-room-composer").fill("Back again");
    await page.getByTestId("viewed-room-send").click();
    // It posts once, under the same identity, and the composer returns.
    await expect.poll(() => callRiverTest(page, "roomMessageCount", "Back again"), { timeout: 30_000 }).toBe(1);
    await expect(page.getByTestId("readmit-pending-message")).toHaveCount(0);
    // Same identity: the room's own key was reused, not a new one made.
    expect(await callRiverTest(page, "currentRoomSelfKey")).toBe(keyBefore);
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
  await page.getByTestId("viewed-room-send").click();
  await expect(page.getByTestId("viewed-room-join-progress")).toBeVisible();
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

test("a message too long for the room is refused before any work, and kept in the box", async ({ page }) => {
  const panics = panicsOf(page);
  await openRoom(page, "open");
  const long = "x".repeat(5000);
  await page.getByTestId("viewed-room-composer").fill(long);
  await page.getByTestId("viewed-room-send").click();
  await expect(page.getByTestId("viewed-room-join-notice")).toContainText("longer than the room allows");
  await expect(page.getByTestId("viewed-room-composer")).toHaveValue(long);
  await expect(page.getByTestId("viewed-room-join-progress")).toHaveCount(0);
  expect(await callRiverTest(page, "joinSessionKey")).toBe("");
  expect(panics).toEqual([]);
});

test("closing the viewer mid-solve keeps the queued text and posts nothing", async ({ page }) => {
  const panics = panicsOf(page);
  await openRoom(page, "open");
  await callRiverTest(page, "holdSolver", true);
  await page.getByTestId("viewed-room-composer").fill("do not lose me");
  await page.getByTestId("viewed-room-send").click();
  await expect(page.getByTestId("viewed-room-pending-message")).toHaveCount(1);
  await page.getByTestId("viewed-room-close").click();
  await expect(page.getByTestId("viewed-room")).toHaveCount(0);
  await callRiverTest(page, "holdSolver", false);
  expect(await callRiverTest(page, "joinSessionKey")).toBe("");
  expect(await callRiverTest(page, "roomMessageCount", "do not lose me")).toBe(0);
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
  // Opening a room the user is in shows that room, not a joinable viewer.
  await expect(page.getByTestId("viewed-room-join")).toHaveCount(0);
  expect(await callRiverTest(page, "joinSessionKey")).toBe("");
  expect(panics).toEqual([]);
});
