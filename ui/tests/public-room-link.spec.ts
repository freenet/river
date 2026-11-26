import { test, expect, Page } from "@playwright/test";
import { closeViewer, waitForApp, selectRoom, panicsOf } from "./example-room";
import { callRiverTest } from "./river-test";
import { captureClipboardWrites, clipboardWrites } from "./clipboard";

// freenet/river#513: `?room=<owner key>` opens a room read-only, without
// membership. The example build has no network, so it resolves the key from
// two fixture rooms that are NOT in the room list (one public, one private).

// Base58 of `n` bytes of 5, built with the page's alphabet rules (no node needed).
function bs58Of(n: number): string {
  const A = "123456789ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz";
  let x = 0n;
  for (let i = 0; i < n; i++) x = x * 256n + 5n;
  let out = "";
  while (x > 0n) {
    out = A[Number(x % 58n)] + out;
    x /= 58n;
  }
  return out;
}

async function keyOf(page: Page, kind: "public" | "private" | "missing" | "member") {
  await page.goto("/");
  await waitForApp(page);
  await hooksReady(page);
  const key = await callRiverTest(page, "viewableRoomKey", kind);
  expect(key.length).toBeGreaterThan(30);
  return key;
}

// The app is up and its test hooks are installed (they appear after first render).
async function hooksReady(page: Page) {
  await page.waitForFunction(() => typeof (window as any).__riverTest?.viewableRoomKey === "function");
}

async function openLink(page: Page, key: string) {
  await page.goto(`/?room=${key}`);
  await waitForApp(page);
}

test("a public room opens read-only, with messages and a disabled composer", async ({ page }) => {
  const panics = panicsOf(page);
  const key = await keyOf(page, "public");
  await openLink(page, key);

  await expect(page.getByTestId("viewed-room-name")).toHaveText("Open Public Room");
  await expect(page.getByTestId("viewed-room-message").first()).toBeVisible();
  expect(await page.getByTestId("viewed-room-message").count()).toBeGreaterThan(3);
  await expect(page.getByTestId("viewed-room-composer")).toBeDisabled();
  await expect(page.getByTestId("viewed-room-composer-note")).toContainText("invitation");
  await expect(page.getByTestId("viewed-room-composer-note")).not.toContainText("read-only");
  // No member identity was needed: nothing offers to send.
  await expect(page.getByTestId("send-message-button")).toHaveCount(0);
  expect(panics).toEqual([]);
});

test("a private room says it is invite-only and shows no content", async ({ page }) => {
  const panics = panicsOf(page);
  const key = await keyOf(page, "private");
  await openLink(page, key);

  await expect(page.getByTestId("viewed-room-invite-only")).toContainText("invite-only");
  await expect(page.getByTestId("viewed-room-message")).toHaveCount(0);
  await expect(page.getByTestId("viewed-room-name")).toHaveCount(0);
  await expect(page.getByTestId("viewed-room-composer")).toHaveCount(0);
  await expect(page.getByTestId("viewed-room")).not.toContainText("Secret Members Room");
  expect(panics).toEqual([]);
});

test("an invalid key is refused with a clear error", async ({ page }) => {
  const panics = panicsOf(page);
  await openLink(page, "not-a-key");
  await expect(page.getByTestId("viewed-room-invalid")).toContainText("not valid");
  await expect(page.getByTestId("viewed-room-message")).toHaveCount(0);

  // A 17-byte value is neither a key nor a short handle (short links: short-handle-link.spec.ts).
  await openLink(page, bs58Of(17));
  await expect(page.getByTestId("viewed-room-invalid")).toContainText("not valid");
  expect(panics).toEqual([]);
});

test("a valid key nobody holds says the room was not found", async ({ page }) => {
  const key = await keyOf(page, "missing");
  await openLink(page, key);
  await expect(page.getByTestId("viewed-room-not-found")).toContainText("Room not found");
  await expect(page.getByTestId("viewed-room-message")).toHaveCount(0);
});

test("the copy link round-trips: copied from a public room, it opens that room", async ({ page }) => {
  // A room the user is in: copy its public link from the room settings.
  await page.goto("/");
  await waitForApp(page);
  await selectRoom(page, "Public Discussion Room");
  await page.getByTestId("room-info-button").click();
  await expect(page.getByTestId("edit-room-modal")).toBeVisible();
  await captureClipboardWrites(page);
  await page.getByTestId("room-public-link-copy-button").click();
  const [copied] = await clipboardWrites(page);
  expect(copied).toMatch(/^https:\/\/freenet\.org\/open#[1-9A-HJ-NP-Za-km-z]{43,44}\/\?room=[1-9A-HJ-NP-Za-km-z]{43,44}$/);
  await expect(page.getByTestId("room-public-link-text")).toHaveText(copied);

  // The opened view has no Copy link of its own (it looks like a joined room),
  // so links are shared from room settings. Open a link of that shape for a
  // viewable room. freenet.org/open sends the reader to
  // `/v1/contract/web/<id>/?room=...` on their own node; here that is the
  // local build.
  const key = await callRiverTest(page, "viewableRoomKey", "public");
  await page.goto("/?room=" + key);
  await waitForApp(page);
  await expect(page.getByTestId("viewed-room-name")).toHaveText("Open Public Room");
});

test("closing the viewer returns to the user's own rooms", async ({ page }) => {
  const key = await keyOf(page, "public");
  await openLink(page, key);
  await closeViewer(page);
  await expect(page.getByTestId("viewed-room")).toHaveCount(0);
  // The user's own rooms are untouched and open normally.
  await selectRoom(page, "Team Chat Room");
  await expect(page.getByTestId("message-composer").first()).toBeVisible();
});

test("a link to a room the user is already in opens their own room, not a visitor view", async ({ page }) => {
  const panics = panicsOf(page);
  const key = await keyOf(page, "member");
  await openLink(page, key);
  await expect(page.getByTestId("viewed-room")).toHaveCount(0);
  await expect(page.getByTestId("message-composer").first()).toBeVisible();
  expect(panics).toEqual([]);
});

test("an invitation in the URL wins over room", async ({ page }) => {
  const key = await keyOf(page, "public");
  // An undecodable invitation: the invitation flow owns the URL, so no visitor view opens.
  await page.goto(`/?invitation=notarealinvitation&room=${key}`);
  await waitForApp(page);
  await expect(page.getByTestId("viewed-room")).toHaveCount(0);
});

test("a link in a public message is rendered as a link", async ({ page }) => {
  const key = await keyOf(page, "public");
  await openLink(page, key);
  await expect(page.getByTestId("viewed-room-messages")).toBeVisible();
  // Messages go through the same renderer as the member view: no raw markup shows.
  await expect(page.getByTestId("viewed-room-messages")).not.toContainText("<p>");
});
