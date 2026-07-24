import { test, expect, Page } from "@playwright/test";

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

async function waitForApp(page: Page) {
  await page.waitForSelector(".app-root", { timeout: 30_000 });
  await expect(page.locator("aside, .app-root button")).not.toHaveCount(0);
}

// A room where the test user is a member, so "Invite Member" can generate
// an invitation (matches the portable-invite-code spec).
const ROOM_NAME = "Public Discussion Room";

async function openInviteModal(page: Page) {
  const vp = page.viewportSize();
  if (vp && vp.width < 1024) {
    await page.setViewportSize({ width: 1280, height: vp.height });
  }
  const roomBtn = page.getByRole("button", { name: ROOM_NAME });
  await expect(roomBtn).toBeVisible({ timeout: 5_000 });
  await roomBtn.click();
  await expect(page.getByRole("heading", { name: ROOM_NAME })).toBeVisible({
    timeout: 5_000,
  });

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

test.describe("Invite-member modal guidance copy", () => {
  test.use({ viewport: { width: 1280, height: 800 } });

  test("recommends sending the invitation via DM, naming Share invite", async ({
    page,
  }) => {
    await page.goto("/");
    await waitForApp(page);
    await openInviteModal(page);

    const rec = page.getByTestId("invite-dm-recommendation");
    await expect(rec).toBeVisible();
    await expect(rec).toContainText(/in a DM/i);
    await expect(rec).toContainText(/Share invite/);
  });

  test("still warns that the link or code is for one person only", async ({
    page,
  }) => {
    await page.goto("/");
    await waitForApp(page);
    await openInviteModal(page);

    const warning = page.getByTestId("invite-share-warning");
    await expect(warning).toBeVisible();
    await expect(warning).toContainText(/one person only/i);
    await expect(warning).toContainText(/New Invitation/);
  });

  test("shows the DM recommendation above the link-sharing warning", async ({
    page,
  }) => {
    await page.goto("/");
    await waitForApp(page);
    await openInviteModal(page);

    const recBox = await page
      .getByTestId("invite-dm-recommendation")
      .boundingBox();
    const warnBox = await page
      .getByTestId("invite-share-warning")
      .boundingBox();
    expect(recBox).not.toBeNull();
    expect(warnBox).not.toBeNull();
    // Recommended path first — the fallback warning sits below it.
    expect(recBox!.y).toBeLessThan(warnBox!.y);
  });
});
