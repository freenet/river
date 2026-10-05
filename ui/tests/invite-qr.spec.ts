import { test, expect, Page } from "@playwright/test";
import { waitForApp, selectRoom } from "./example-room";

// freenet/river#741. The invite modal shows a QR of the portable code, and
// Enter Invite Code offers a camera scan. The scan itself needs a camera and
// BarcodeDetector, so CI only checks that the control is there; the matrix
// round-trip is a Rust test in ui/src/invite_qr.rs.

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
    await expect(qr.locator("svg")).toBeVisible();
    await expect(qr.locator("svg")).toHaveAttribute("role", "img");
    await expect(page.getByTestId("invite-qr-error")).toHaveCount(0);
  });

  test("Enter Invite Code offers a QR scan", async ({ page }) => {
    await page.goto("/");
    await waitForApp(page);

    await page.getByTestId("join-with-code-button").click();
    await expect(page.getByTestId("join-with-code-modal")).toBeVisible({
      timeout: 5_000,
    });
    await expect(page.getByTestId("join-with-code-scan-button")).toBeVisible();
    await expect(page.getByTestId("join-with-code-scan-button")).toHaveText(
      /Scan QR code/
    );
  });
});
