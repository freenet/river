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
// persisted room blob (public_join::tests), not a spec here.

type Kind = "open" | "hard" | "full";

async function openRoom(page: Page, kind: Kind) {
  await page.goto("/");
  await waitForApp(page);
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
  await expect(page.getByText("Hello from a visitor")).toHaveCount(1);
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
  await expect(page.getByText("First post")).toHaveCount(1);
  expect(panics).toEqual([]);
});

test("a queued message is shown as pending and is not lost when the join is cancelled", async ({ page }) => {
  const panics = panicsOf(page);
  await openRoom(page, "hard");
  await page.getByTestId("viewed-room-composer").fill("Keep this draft");
  await page.getByTestId("viewed-room-send").click();

  // Pending, not posted: shown apart from the room's messages, with Cancel.
  await expect(page.getByTestId("viewed-room-pending-message")).toContainText("Keep this draft");
  await expect(page.getByTestId("viewed-room-join-progress")).toBeVisible();
  await expect(page.getByTestId("viewed-room")).toBeVisible();

  await page.getByTestId("viewed-room-join-cancel").click();
  await expect(page.getByTestId("viewed-room-pending-message")).toHaveCount(0);
  await expect(page.getByTestId("viewed-room-composer")).toHaveValue("Keep this draft");
  // Nothing was posted and the visitor is not a member.
  await expect(page.getByTestId("viewed-room")).toBeVisible();
  await expect(page.getByTestId("viewed-room-join")).toBeVisible();
  expect(panics).toEqual([]);
});

test("a banned key is told so, and the draft comes back", async ({ page }) => {
  const panics = panicsOf(page);
  await openRoom(page, "hard");
  await page.getByTestId("viewed-room-composer").fill("Hello");
  await page.getByTestId("viewed-room-send").click();
  await expect(page.getByTestId("viewed-room-join-progress")).toBeVisible();

  await expect.poll(() => callRiverTest(page, "banJoiner", "hard")).toBe(true);
  await expect(page.getByTestId("viewed-room-join-error")).toContainText("banned", { timeout: 30_000 });
  await expect(page.getByTestId("viewed-room-composer")).toHaveValue("Hello");
  await expect(page.getByTestId("viewed-room-pending-message")).toHaveCount(0);
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
    await expect(page.getByText("Joined")).toHaveCount(1);

    // The room's cleanup drops the member (and, with `forget`, a reload has
    // lost the proof). The normal composer is replaced by the re-admission one.
    expect(await callRiverTest(page, "pruneJoiner", forget)).toBe(true);
    await expect(page.getByText("Joined")).toHaveCount(0);
    await expect(page.getByTestId("send-message-button")).toHaveCount(0);
    await expect(page.getByTestId("viewed-room-join-idle")).toContainText("removed you for being quiet");

    await page.getByTestId("viewed-room-composer").fill("Back again");
    await page.getByTestId("viewed-room-send").click();
    // It posts once, under the same identity, and the composer returns.
    await expect(page.getByText("Back again")).toHaveCount(1, { timeout: 30_000 });
    await expect(page.getByTestId("readmit-pending-message")).toHaveCount(0);
    await expect(page.getByTestId("send-message-button")).toBeVisible();
    expect(panics).toEqual([]);
  });
}
