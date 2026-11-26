import { test, expect, Page } from "@playwright/test";
import { waitForApp, selectRoom, panicsOf } from "./example-room";
import { callRiverTest } from "./river-test";
import { captureClipboardWrites, clipboardWrites } from "./clipboard";

// freenet/river#973: `?room=<handle>` resolves through the resolver contract to
// the owner key, verifies it against the handle, then opens as a full link. The
// example build has no node: a fixture resolver answers after ~1.2 s.

async function hooksReady(page: Page) {
  await page.waitForFunction(() => typeof (window as any).__riverTest?.shortHandle === "function");
}

async function handleOf(page: Page, kind: "public" | "private" | "missing" | "mismatch") {
  await page.goto("/");
  await waitForApp(page);
  await hooksReady(page);
  const handle = await callRiverTest(page, "shortHandle", kind);
  expect(handle.length).toBeGreaterThanOrEqual(21);
  expect(handle.length).toBeLessThanOrEqual(22);
  return handle;
}

test("a short link shows loading, then opens the room it names", async ({ page }) => {
  const panics = panicsOf(page);
  const handle = await handleOf(page, "public");
  await page.goto(`/?room=${handle}`);
  await waitForApp(page);
  await expect(page.getByTestId("viewed-room-resolving")).toBeVisible();
  await expect(page.getByTestId("viewed-room-name")).toHaveText("Open Public Room");
  await expect(page.getByTestId("viewed-room-composer")).toBeDisabled();
  expect(panics).toEqual([]);
});

test("a short link nobody registered says so", async ({ page }) => {
  const panics = panicsOf(page);
  const handle = await handleOf(page, "missing");
  await page.goto(`/?room=${handle}`);
  await waitForApp(page);
  await expect(page.getByTestId("viewed-room-resolving")).toBeVisible();
  await expect(page.getByTestId("viewed-room-handle-not-found")).toContainText(
    "This short link isn't registered yet",
  );
  await expect(page.getByTestId("viewed-room-retry")).toBeVisible();
  await expect(page.getByTestId("viewed-room-message")).toHaveCount(0);
  expect(panics).toEqual([]);
});

test("an entry that points at a key which does not hash to the handle is refused", async ({ page }) => {
  const panics = panicsOf(page);
  const handle = await handleOf(page, "mismatch");
  await page.goto(`/?room=${handle}`);
  await waitForApp(page);
  await expect(page.getByTestId("viewed-room-invalid")).toContainText("does not match");
  // Nothing of the room it pointed at was opened.
  await expect(page.getByTestId("viewed-room-name")).toHaveCount(0);
  await expect(page.getByTestId("viewed-room-message")).toHaveCount(0);
  await expect(page.getByTestId("viewed-room-retry")).toHaveCount(0);
  expect(panics).toEqual([]);
});

test("a full link opens as before and sends no resolver lookup", async ({ page }) => {
  const panics = panicsOf(page);
  await page.goto("/");
  await waitForApp(page);
  await page.waitForFunction(() => typeof (window as any).__riverTest?.viewableRoomKey === "function");
  const key = await callRiverTest(page, "viewableRoomKey", "public");
  await page.goto(`/?room=${key}`);
  await waitForApp(page);
  await expect(page.getByTestId("viewed-room-name")).toHaveText("Open Public Room");
  await expect(page.getByTestId("viewed-room-resolving")).toHaveCount(0);
  // Wait past the fixture resolver's delay: nothing was asked of it.
  await page.waitForTimeout(1800);
  expect(await callRiverTest(page, "resolverLookups")).toBe(0);
  expect(panics).toEqual([]);
});

test("a short link does send exactly one resolver lookup", async ({ page }) => {
  const handle = await handleOf(page, "public");
  await page.goto(`/?room=${handle}`);
  await waitForApp(page);
  await expect(page.getByTestId("viewed-room-name")).toHaveText("Open Public Room");
  expect(await callRiverTest(page, "resolverLookups")).toBe(1);
});

test("Copy short link appears only after the entry is found; the full link stays the default", async ({ page }) => {
  await page.goto("/");
  await waitForApp(page);
  await hooksReady(page);
  await selectRoom(page, "Public Discussion Room");
  const owner = await callRiverTest(page, "currentRoomOwnerKey");
  expect(owner.length).toBeGreaterThan(30);

  const open = async () => {
    await page.getByTestId("room-info-button").click();
    await expect(page.getByTestId("edit-room-modal")).toBeVisible();
  };
  await open();
  // The full link is there at once and is what the default button copies.
  await expect(page.getByTestId("room-public-link-text")).toContainText(`?room=${owner}`);
  // No entry is published: the check finds nothing and offers nothing.
  await page.waitForTimeout(2200);
  await expect(page.getByTestId("room-short-link")).toHaveCount(0);
  await page.getByTestId("edit-room-close-button").click();
  await expect(page.getByTestId("edit-room-modal")).toBeHidden();

  // The entry is published: the same dialog now offers the short link too.
  expect(await callRiverTest(page, "publishShortEntry", owner)).toBe(true);
  await open();
  await expect(page.getByTestId("room-short-link")).toBeVisible({ timeout: 10_000 });
  const handle = await page.getByTestId("room-short-link-text").innerText();
  expect(handle).toMatch(/^https:\/\/freenet\.org\/open#[1-9A-HJ-NP-Za-km-z]{43,44}\/\?room=[1-9A-HJ-NP-Za-km-z]{21,22}$/);

  await captureClipboardWrites(page);
  await page.getByTestId("room-short-link-copy-button").click();
  await page.getByTestId("room-public-link-copy-button").click();
  const [shortCopied, fullCopied] = await clipboardWrites(page);
  expect(shortCopied).toBe(handle);
  expect(fullCopied).toContain(`?room=${owner}`);
});
