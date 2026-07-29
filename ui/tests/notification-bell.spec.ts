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
    // This room has no stored preference, so the default is All. (Since
    // freenet/river#500 the fixture seeds "Your Private Room" as Muted, so
    // "no stored preference" is no longer true of every example room.)
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

  // freenet/river#510: the modes above only decide WHEN River wants to notify.
  // If the browser is blocking notifications none of them can fire, and River
  // used to discard every status the shell reported — so that failure was
  // invisible and there was no way to ask again.
  test("the modal explains browser permission and offers a way to enable it", async ({
    page,
  }) => {
    await page.goto("/");
    await waitForApp(page);
    await selectRoom(page, ROOM);

    await page.getByTestId("notification-bell-button").click();
    const modal = page.getByTestId("notification-modal");
    await expect(modal).toBeVisible({ timeout: 5_000 });

    // Whatever the browser's state, the user is told something about it.
    const message = modal.getByTestId("notification-permission-message");
    await expect(message).toBeVisible();
    await expect(message).not.toHaveText(/^\s*$/);

    // The projects cover all three real states, so this branches on the
    // browser's actual answer rather than assuming one: Playwright's Chromium
    // reports "denied", Firefox and desktop WebKit "default", and mobile Safari
    // has no Notifications API at all.
    const state = await page.evaluate(() =>
      typeof Notification === "undefined" ? "unsupported" : Notification.permission
    );
    const enable = modal.getByTestId("notification-enable-button");

    if (state === "default") {
      // Nothing is blocked, so the retry #510 asks for must be there.
      await expect(enable).toBeVisible();
      await expect(enable).toBeEnabled();
    } else {
      // Blocked at the browser level, already granted, or no API at all —
      // asking again cannot change the answer, so River must not offer a
      // button that silently does nothing.
      await expect(enable).toHaveCount(0);
      if (state === "denied") {
        await expect(message).toContainText(/blocking/i);
      } else if (state === "unsupported") {
        await expect(message).toContainText(/support/i);
      }
    }
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
