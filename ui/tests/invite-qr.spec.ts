import { test, expect, Page } from "@playwright/test";
import { waitForApp, selectRoom } from "./example-room";

// freenet/river#741. The invite modal shows a QR of the portable code, and
// Enter Invite Code offers a scan only where BarcodeDetector exists. A real
// scan needs a camera, so CI checks which controls and copy are shown, with
// and without a (stubbed) detector; the matrix round-trip is a Rust test in
// ui/src/invite_qr.rs.

const ROOM_NAME = "Public Discussion Room";

async function openInviteModal(page: Page) {
  await page.getByTestId("invite-member-button").click();
  await expect(page.getByTestId("invite-member-modal")).toBeVisible({
    timeout: 5_000,
  });
  await expect(page.getByTestId("invite-code-input")).not.toHaveValue("", {
    timeout: 10_000,
  });
}

test.describe("Invite QR code (issue #741)", { tag: "@chromium-only" }, () => {
  test.use({ viewport: { width: 1280, height: 800 } });

  test("the invite modal shows a QR code of the portable invite", async ({
    page,
  }) => {
    await page.goto("/");
    await waitForApp(page);
    await selectRoom(page, ROOM_NAME);
    await openInviteModal(page);

    const qr = page.getByTestId("invite-qr");
    await expect(qr).toBeVisible();
    // The heading icon is also an svg. The code itself is the image with
    // role="img"; matching every svg fails strict mode on the icon.
    const code = qr.locator('svg[role="img"]');
    await expect(code).toHaveCount(1);
    await expect(code).toBeVisible();
    await expect(page.getByTestId("invite-qr-error")).toHaveCount(0);
  });

  async function openJoinModal(page: Page) {
    await page.goto("/");
    await waitForApp(page);
    await page.getByTestId("join-with-code-button").click();
    const modal = page.getByTestId("join-with-code-modal");
    await expect(modal).toBeVisible({ timeout: 5_000 });
    return modal;
  }

  test("without BarcodeDetector there is no scan button or scan copy", async ({ page }) => {
    // Like iOS Safari and Firefox. Removed explicitly so the test does not
    // depend on the platform's Chromium build lacking it.
    await page.addInitScript(() => {
      delete (window as any).BarcodeDetector;
    });
    const modal = await openJoinModal(page);
    expect(await page.evaluate(() => "BarcodeDetector" in window)).toBe(false);
    await expect(page.getByTestId("join-with-code-scan-button")).toHaveCount(0);
    await expect(modal).toContainText("Paste a portable invite code someone shared with you.");
    await expect(modal).not.toContainText(/scan/i);
  });

  test("with BarcodeDetector the scan button is a secondary action", async ({ page }) => {
    await page.addInitScript(() => {
      (window as any).BarcodeDetector = class {
        static async getSupportedFormats() {
          return ["qr_code"];
        }
        async detect() {
          return [];
        }
      };
    });
    const modal = await openJoinModal(page);
    const scan = page.getByTestId("join-with-code-scan-button");
    await expect(scan).toBeVisible();
    await expect(scan).toHaveText(/Scan QR code/);
    await expect(modal).toContainText("or scan their QR code");
    // One primary action per modal: Scan uses the surface style, not the accent.
    await expect(scan).not.toHaveClass(/bg-accent/);
    await expect(scan).toHaveClass(/(^|\s)bg-surface(\s|$)/);
  });
});
