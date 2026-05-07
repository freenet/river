import { test, expect, Page } from "@playwright/test";

// Regression test for: when the tab loses focus, the title incorrectly shows
// "(N) River - …" with a non-zero unread count even though the user was just
// active on the page. The fix marks every room as read at the moment of the
// visible -> hidden transition, so only messages arriving *after* that point
// drive the unread badge.
//
// Reported by Ian Clarke, 2026-05-06.

async function waitForApp(page: Page) {
  await page.waitForSelector(".app-root", { timeout: 30_000 });
  await expect(page.locator("aside, .app-root button")).not.toHaveCount(0);
}

// Force the tab into the "hidden" visibility state. We override the
// `document.hidden` and `document.visibilityState` getters and dispatch the
// `visibilitychange` event the same way Chromium / WebKit / Firefox do when
// the tab goes to the background.
async function setTabHidden(page: Page) {
  await page.evaluate(() => {
    Object.defineProperty(document, "hidden", {
      configurable: true,
      get: () => true,
    });
    Object.defineProperty(document, "visibilityState", {
      configurable: true,
      get: () => "hidden",
    });
    document.dispatchEvent(new Event("visibilitychange"));
  });
}

async function setTabVisible(page: Page) {
  await page.evaluate(() => {
    Object.defineProperty(document, "hidden", {
      configurable: true,
      get: () => false,
    });
    Object.defineProperty(document, "visibilityState", {
      configurable: true,
      get: () => "visible",
    });
    document.dispatchEvent(new Event("visibilitychange"));
  });
}

test.describe("Document title unread badge", () => {
  test.use({ viewport: { width: 1280, height: 800 } });

  test("hiding the tab while active does not introduce an unread badge", async ({
    page,
  }) => {
    await page.goto("/");
    await waitForApp(page);

    // Wait for the title to settle. Example data has multiple rooms but no
    // room is auto-selected, so the title starts as "River".
    await expect(page).toHaveTitle("River", { timeout: 5_000 });

    // Simulate the user switching to a different tab.
    await setTabHidden(page);

    // The title must NOT gain a "(N)" badge: while the tab was visible the
    // user had the chance to see all messages already in state.
    await expect(page).not.toHaveTitle(/^\(\d+\) /, { timeout: 2_000 });
    await expect(page).toHaveTitle("River");
  });

  test("hiding the tab after selecting a room keeps the plain title", async ({
    page,
  }) => {
    await page.goto("/");
    await waitForApp(page);

    const roomBtn = page.getByRole("button", { name: "Public Discussion Room" });
    await expect(roomBtn).toBeVisible({ timeout: 5_000 });
    await roomBtn.click();
    await expect(
      page.getByRole("heading", { name: "Public Discussion Room" })
    ).toBeVisible({ timeout: 5_000 });

    await expect(page).toHaveTitle("River - Public Discussion Room", {
      timeout: 5_000,
    });

    await setTabHidden(page);

    await expect(page).not.toHaveTitle(/^\(\d+\) /, { timeout: 2_000 });
    await expect(page).toHaveTitle("River - Public Discussion Room");
  });

  // Defensive: if hide -> show -> hide cycles introduce churn (e.g. by
  // counting messages twice or by toggling state in the wrong direction)
  // the title would gain a badge on the second hide. With the fix it should
  // not — there are no new messages between the two hides.
  test("hide -> show -> hide cycle does not introduce a badge", async ({
    page,
  }) => {
    await page.goto("/");
    await waitForApp(page);
    await expect(page).toHaveTitle("River", { timeout: 5_000 });

    await setTabHidden(page);
    await expect(page).toHaveTitle("River");

    await setTabVisible(page);
    await expect(page).toHaveTitle("River");

    await setTabHidden(page);
    await expect(page).not.toHaveTitle(/^\(\d+\) /, { timeout: 2_000 });
    await expect(page).toHaveTitle("River");
  });
});
