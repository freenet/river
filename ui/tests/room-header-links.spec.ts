import { test, expect, Page } from "@playwright/test";

// The room header description renders user-supplied markdown that may include
// `<a>` links. Those links must live outside the clickable "room details"
// button — `<a>` inside `<button>` is invalid HTML and bubbles link clicks to
// the modal-opening onclick handler.

const ROOM_WITH_LINKS = "Public Discussion Room";

async function waitForApp(page: Page) {
  await page.waitForSelector(".app-root", { timeout: 30_000 });
  await expect(page.locator("aside, .app-root button")).not.toHaveCount(0);
}

async function selectRoom(page: Page, roomName: string) {
  const vp = page.viewportSize();
  // Use desktop layout for this regression test; mobile navigation is exercised
  // elsewhere and is orthogonal to the bug under test.
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

test.describe("Room header description links", () => {
  test.use({ viewport: { width: 1280, height: 800 } });

  test("links in description are NOT nested inside <button>", async ({
    page,
  }) => {
    await page.goto("/");
    await waitForApp(page);
    await selectRoom(page, ROOM_WITH_LINKS);

    const header = page.locator(".border-b.border-border.bg-panel").first();
    const link = header.locator('a[href="https://freenet.org/"]');
    await expect(link).toBeVisible();

    const hasButtonAncestor = await link.evaluate((el) =>
      el.closest("button") !== null
    );
    expect(hasButtonAncestor).toBe(false);
  });

  test("clicking a description link does NOT open the room details modal", async ({
    page,
  }) => {
    await page.goto("/");
    await waitForApp(page);
    await selectRoom(page, ROOM_WITH_LINKS);

    const header = page.locator(".border-b.border-border.bg-panel").first();
    const link = header.locator('a[href="https://freenet.org/"]');
    await expect(link).toBeVisible();

    // Neutralise navigation so the click stays on the page. We deliberately
    // do NOT stop propagation: the test exists to catch a click bubbling up
    // to the room-details button, so the click must still reach any ancestor
    // handler that would (incorrectly) be wired up.
    await link.evaluate((el) => {
      el.removeAttribute("target");
      el.addEventListener("click", (e) => e.preventDefault(), {
        once: true,
      });
    });

    await link.click();

    // The room details modal opens via crate::util::defer (setTimeout(0)),
    // so a synchronous toHaveCount(0) immediately after the click could pass
    // before the deferred handler runs. Wait one tick so any incorrectly
    // bubbled click has had time to render the modal.
    await page.waitForTimeout(50);

    await expect(
      page.getByRole("heading", { name: /Room Details/i })
    ).toHaveCount(0);
  });

  test("clicking the room title still opens the room details modal", async ({
    page,
  }) => {
    await page.goto("/");
    await waitForApp(page);
    await selectRoom(page, ROOM_WITH_LINKS);

    const header = page.locator(".border-b.border-border.bg-panel").first();
    // The title button has title="Room details".
    const titleButton = header.locator('button[title="Room details"]');
    await expect(titleButton).toBeVisible();
    await titleButton.click();

    await expect(
      page.getByRole("heading", { name: /Room Details/i })
    ).toBeVisible({ timeout: 5_000 });
  });
});
