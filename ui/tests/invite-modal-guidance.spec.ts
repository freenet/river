import { test, expect, Page } from "@playwright/test";
import { waitForApp, selectRoom } from "./example-room";

// Copy test for the invite-member modal's guidance blocks.
//
// The modal used to warn ONLY about the link being single-use, never
// mentioning that River can send an invitation directly in a DM (#252,
// #457) — which is the recommended flow: ask the person first, then use
// "Share invite" from their member card in a room you already share, so
// no bearer credential travels through an outside channel.
//
// These assertions pin BOTH blocks and their order, so a future refactor
// of this modal can't silently drop the recommendation and leave
// copy-the-link as the only documented path.

// A room where the test user is a member, so "Invite Member" can generate
// an invitation (matches the portable-invite-code spec).
const ROOM_NAME = "Public Discussion Room";

async function openInviteModal(page: Page) {
  await selectRoom(page, ROOM_NAME);
  await page.getByTestId("invite-member-button").click();
  await expect(page.getByTestId("invite-member-modal")).toBeVisible({
    timeout: 5_000,
  });
  // The invitation is generated asynchronously (delegate signing with a
  // local fallback); the guidance blocks render alongside it.
  await expect(page.getByTestId("invite-link-input")).not.toHaveValue("", {
    timeout: 10_000,
  });
}

test.describe("Invite-member modal guidance copy", { tag: "@chromium-only" }, () => {
  test.use({ viewport: { width: 1280, height: 800 } });

  test("recommends a DM invite above the one-person-only link warning", async ({
    page,
  }) => {
    await page.goto("/");
    await waitForApp(page);
    await openInviteModal(page);

    const rec = page.getByTestId("invite-dm-recommendation");
    await expect(rec).toBeVisible();
    await expect(rec).toContainText(/in a DM/i);
    await expect(rec).toContainText(/Share invite/);

    const warning = page.getByTestId("invite-share-warning");
    await expect(warning).toBeVisible();
    await expect(warning).toContainText(/one person only/i);
    await expect(warning).toContainText(/New Invitation/);

    // Recommended path first — the fallback warning sits below it.
    const recBox = await rec.boundingBox();
    const warnBox = await warning.boundingBox();
    expect(recBox).not.toBeNull();
    expect(warnBox).not.toBeNull();
    expect(recBox!.y).toBeLessThan(warnBox!.y);
  });
});

// An invitation made while the modal was closed used to leave a stale error on the next open.
test.describe("Invite-member modal while the invitation is created", () => {
  test.use({ viewport: { width: 1280, height: 800 } });
  const STALE = ["Modal closed", "Try Again"]; // no "Generating invitation": PR 15 replaces that text

  async function watchForText(page: Page, needles: string[]) {
    await page.evaluate((needles) => {
      const w = window as any;
      w.__seenInviteText = [];
      new MutationObserver(() => {
        const text = document.body.innerText;
        for (const n of needles) if (text.includes(n) && !w.__seenInviteText.includes(n)) w.__seenInviteText.push(n);
      }).observe(document.body, { childList: true, subtree: true, characterData: true });
    }, needles);
  }

  test("opening it never shows a stale error", { tag: "@chromium-only" }, async ({ page }) => {
    await page.goto("/");
    await waitForApp(page);
    await watchForText(page, STALE);
    await openInviteModal(page);
    await page.getByTestId("invite-member-close-button").click();
    await expect(page.getByTestId("invite-member-modal")).toHaveCount(0);
    await page.getByTestId("invite-member-button").click();
    await expect(page.getByTestId("invite-link-input")).not.toHaveValue("", { timeout: 10_000 });
    expect(await page.evaluate(() => (window as any).__seenInviteText)).toEqual([]);
  });

  test("New Invitation replaces the link without a stale error", { tag: "@chromium-only" }, async ({ page }) => {
    await page.goto("/");
    await waitForApp(page);
    await openInviteModal(page);
    const link = page.getByTestId("invite-link-input");
    const first = await link.inputValue();
    await watchForText(page, STALE);
    await page.getByTestId("invite-new-invitation-button").click();
    await expect(link).not.toHaveValue(first, { timeout: 10_000 });
    await expect(link).not.toHaveValue("");
    expect(await page.evaluate(() => (window as any).__seenInviteText)).toEqual([]);
  });
});
