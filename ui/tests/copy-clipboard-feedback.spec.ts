import { test, expect, Page } from "@playwright/test";

// Regression test for the Export Identity "Copy to Clipboard" button. Before
// the fix, clicking the button copied the token but gave no visual feedback,
// leaving the user unsure whether anything had happened (Matrix bug report
// from Ivvor, 2026-04-30).

async function waitForApp(page: Page) {
  await page.waitForSelector(".app-root", { timeout: 30_000 });
  await expect(page.locator("aside, .app-root button")).not.toHaveCount(0);
}

// Use a room where the test user is a member (not the owner) so the
// "Export ID" affordance is available.
const ROOM_NAME = "Public Discussion Room";

async function selectRoom(page: Page) {
  const vp = page.viewportSize();
  if (vp && vp.width < 1024) {
    await page.setViewportSize({ width: 1280, height: vp.height });
  }
  const roomBtn = page.getByRole("button", { name: ROOM_NAME });
  await expect(roomBtn).toBeVisible({ timeout: 5_000 });
  await roomBtn.click();
  await expect(
    page.getByRole("heading", { name: ROOM_NAME })
  ).toBeVisible({ timeout: 5_000 });
}

test.describe("Export Identity copy feedback", () => {
  test.use({ viewport: { width: 1280, height: 800 } });

  test("clicking Copy to Clipboard updates the button to 'Copied!'", async ({
    page,
  }) => {
    await page.goto("/");
    await waitForApp(page);
    await selectRoom(page);

    const exportButton = page.getByRole("button", { name: "Export ID" });
    await expect(exportButton).toBeVisible({ timeout: 5_000 });
    await exportButton.click();

    const copyButton = page.getByRole("button", { name: "Copy to Clipboard" });
    await expect(copyButton).toBeVisible({ timeout: 5_000 });

    await copyButton.click();

    await expect(
      page.getByRole("button", { name: "Copied!" })
    ).toBeVisible({ timeout: 2_000 });
  });

  test("reopening the modal resets the button text", async ({ page }) => {
    await page.goto("/");
    await waitForApp(page);
    await selectRoom(page);

    const exportButton = page.getByRole("button", { name: "Export ID" });
    await exportButton.click();

    const copyButton = page.getByRole("button", { name: "Copy to Clipboard" });
    await expect(copyButton).toBeVisible({ timeout: 5_000 });
    await copyButton.click();
    await expect(
      page.getByRole("button", { name: "Copied!" })
    ).toBeVisible({ timeout: 2_000 });

    // Close via the explicit Close button.
    await page.getByRole("button", { name: "Close" }).click();
    await expect(
      page.getByRole("button", { name: "Copied!" })
    ).toHaveCount(0);

    // Reopen — the button must say "Copy to Clipboard" again, not stay stuck on "Copied!".
    await exportButton.click();
    await expect(
      page.getByRole("button", { name: "Copy to Clipboard" })
    ).toBeVisible({ timeout: 5_000 });
    await expect(
      page.getByRole("button", { name: "Copied!" })
    ).toHaveCount(0);
  });
});
