import { test, expect } from "@playwright/test";
import { waitForApp, selectRoom } from "./example-room";

// Regression test for the Export Identity "Copy to Clipboard" button. Before
// the fix, clicking the button copied the token but gave no visual feedback,
// leaving the user unsure whether anything had happened (Matrix bug report
// from Ivvor, 2026-04-30).

// Use a room where the test user is a member (not the owner) so the
// "Export ID" affordance is available.
const ROOM_NAME = "Public Discussion Room";

test.describe("Export Identity copy feedback", () => {
  test.use({ viewport: { width: 1280, height: 800 } });

  test("clicking Copy to Clipboard updates the button to 'Copied!'", async ({ page }) => {
    await page.goto("/");
    await waitForApp(page);
    await selectRoom(page, ROOM_NAME);

    const exportButton = page.getByRole("button", { name: "Export ID" });
    await expect(exportButton).toBeVisible({ timeout: 5_000 });
    await exportButton.click();

    const copyButton = page.getByRole("button", { name: "Copy to Clipboard" });
    await expect(copyButton).toBeVisible({ timeout: 5_000 });

    await copyButton.click();

    await expect(page.getByRole("button", { name: "Copied!" })).toBeVisible({ timeout: 2_000 });
  });

  test("dismissing via the backdrop also resets the button text", async ({ page }) => {
    await page.goto("/");
    await waitForApp(page);
    await selectRoom(page, ROOM_NAME);

    const exportButton = page.getByRole("button", { name: "Export ID" });
    await exportButton.click();

    const copyButton = page.getByRole("button", { name: "Copy to Clipboard" });
    await expect(copyButton).toBeVisible({ timeout: 5_000 });
    await copyButton.click();
    await expect(page.getByRole("button", { name: "Copied!" })).toBeVisible({ timeout: 2_000 });

    // Click the backdrop (anywhere outside the inner panel). The modal panel
    // calls stop_propagation, so a click in the corner reaches the backdrop.
    await page.mouse.click(5, 5);
    await expect(page.getByRole("button", { name: "Copied!" })).toHaveCount(0);

    await exportButton.click();
    await expect(page.getByRole("button", { name: "Copy to Clipboard" })).toBeVisible({
      timeout: 5_000,
    });
    await expect(page.getByRole("button", { name: "Copied!" })).toHaveCount(0);
  });

  test("reopening the modal resets the button text", async ({ page }) => {
    await page.goto("/");
    await waitForApp(page);
    await selectRoom(page, ROOM_NAME);

    const exportButton = page.getByRole("button", { name: "Export ID" });
    await exportButton.click();

    const copyButton = page.getByRole("button", { name: "Copy to Clipboard" });
    await expect(copyButton).toBeVisible({ timeout: 5_000 });
    await copyButton.click();
    await expect(page.getByRole("button", { name: "Copied!" })).toBeVisible({ timeout: 2_000 });

    // Close via the explicit Close button.
    //
    // `exact: true` is load-bearing. `getByRole` matches the accessible name by
    // case-insensitive SUBSTRING, and a member row's accessible name includes
    // its badges' `aria-label`s — the ⚠ impersonation warning's begins
    // "…their name CLOSEly resembles…" (freenet/river#489). So a room
    // containing a flagged member gives this locator a second match and the
    // click fails on strict mode, with no obvious connection to the cause.
    await page.getByRole("button", { name: "Close", exact: true }).click();
    await expect(page.getByRole("button", { name: "Copied!" })).toHaveCount(0);

    // Reopen — the button must say "Copy to Clipboard" again, not stay stuck on "Copied!".
    await exportButton.click();
    await expect(page.getByRole("button", { name: "Copy to Clipboard" })).toBeVisible({
      timeout: 5_000,
    });
    await expect(page.getByRole("button", { name: "Copied!" })).toHaveCount(0);
  });
});
