import { test, expect, Page } from "@playwright/test";

// The per-room notification preference (All / Mentions & replies / Muted) is
// reached from a bell icon in the conversation header, which opens a compact
// dedicated modal. The setting is persisted in the delegate (rooms_meta); these
// tests exercise the UI surface against example data.

const ROOM = "Public Discussion Room";

async function waitForApp(page: Page) {
  await page.waitForSelector(".app-root", { timeout: 30_000 });
  await expect(page.locator("aside, .app-root button")).not.toHaveCount(0);
}

async function selectRoom(page: Page, roomName: string) {
  const vp = page.viewportSize();
  if (vp && vp.width < 1024) {
    await page.setViewportSize({ width: 1280, height: vp.height });
  }
  const roomBtn = page.getByRole("button", { name: roomName });
  await expect(roomBtn).toBeVisible({ timeout: 5_000 });
  await roomBtn.click();
  await expect(
    page.getByRole("heading", { name: roomName })
  ).toBeVisible({ timeout: 5_000 });
}

test.describe("Per-room notification bell", () => {
  test.use({ viewport: { width: 1280, height: 800 } });

  test("bell is in the header and defaults to 'All messages'", async ({
    page,
  }) => {
    await page.goto("/");
    await waitForApp(page);
    await selectRoom(page, ROOM);

    const bell = page.getByTestId("notification-bell-button");
    await expect(bell).toBeVisible();
    // Example rooms have no stored preference, so the default is All.
    await expect(bell).toHaveAttribute("title", "Notifications: All messages");
  });

  test("clicking the bell opens the notification modal with three modes", async ({
    page,
  }) => {
    await page.goto("/");
    await waitForApp(page);
    await selectRoom(page, ROOM);

    await page.getByTestId("notification-bell-button").click();

    const modal = page.getByTestId("notification-modal");
    await expect(modal).toBeVisible({ timeout: 5_000 });
    await expect(modal.getByTestId("notification-mode-option")).toHaveCount(3);
    await expect(modal.getByText("All messages")).toBeVisible();
    await expect(modal.getByText("Mentions & replies only")).toBeVisible();
    await expect(modal.getByText("Muted")).toBeVisible();
  });

  test("selecting a mode applies it, closes the modal, and updates the bell", async ({
    page,
  }) => {
    await page.goto("/");
    await waitForApp(page);
    await selectRoom(page, ROOM);

    await page.getByTestId("notification-bell-button").click();
    const modal = page.getByTestId("notification-modal");
    await expect(modal).toBeVisible({ timeout: 5_000 });

    // Pick "Muted".
    await modal
      .getByTestId("notification-mode-option")
      .filter({ hasText: "Muted" })
      .click();

    // Modal closes on selection (one-click pick).
    await expect(modal).toHaveCount(0, { timeout: 5_000 });

    // Bell now reflects the muted state via its tooltip.
    await expect(
      page.getByTestId("notification-bell-button")
    ).toHaveAttribute("title", "Notifications: Muted");

    // Reopening shows the muted row as the selected one (check indicator).
    await page.getByTestId("notification-bell-button").click();
    const reopened = page.getByTestId("notification-modal");
    await expect(reopened).toBeVisible({ timeout: 5_000 });
    const mutedRow = reopened
      .getByTestId("notification-mode-option")
      .filter({ hasText: "Muted" });
    // Selected row carries the accent border class.
    await expect(mutedRow).toHaveClass(/border-accent/);
  });

  test("closing via the ✕ leaves the preference unchanged", async ({
    page,
  }) => {
    await page.goto("/");
    await waitForApp(page);
    await selectRoom(page, ROOM);

    await page.getByTestId("notification-bell-button").click();
    const modal = page.getByTestId("notification-modal");
    await expect(modal).toBeVisible({ timeout: 5_000 });

    await page.getByTestId("notification-modal-close").click();
    await expect(modal).toHaveCount(0, { timeout: 5_000 });
    await expect(
      page.getByTestId("notification-bell-button")
    ).toHaveAttribute("title", "Notifications: All messages");
  });

  test("room-details modal no longer carries the notification setting", async ({
    page,
  }) => {
    await page.goto("/");
    await waitForApp(page);
    await selectRoom(page, ROOM);

    // Open room details via the title button.
    const header = page.locator(".border-b.border-border.bg-panel").first();
    await header.locator('button[title="Room details"]').click();
    await expect(
      page.getByRole("heading", { name: /Room Details/i })
    ).toBeVisible({ timeout: 5_000 });

    // The notification picker moved to the bell modal; it must not appear here.
    await expect(
      page.getByText("Mentions & replies only")
    ).toHaveCount(0);
  });
});
