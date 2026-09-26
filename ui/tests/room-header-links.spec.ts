import { test, expect } from "@playwright/test";
import { waitForApp, selectRoom } from "./example-room";

// The room header description renders user-supplied markdown that may include
// `<a>` links. Those links must live outside the clickable "room details"
// button — `<a>` inside `<button>` is invalid HTML and bubbles link clicks to
// the modal-opening onclick handler.

const ROOM_WITH_LINKS = "Public Discussion Room";

test.describe("Room header description links", () => {
  test.use({ viewport: { width: 1280, height: 800 } });

  test("links in description are NOT nested inside <button>", async ({
    page,
  }) => {
    await page.goto("/");
    await waitForApp(page);
    await selectRoom(page, ROOM_WITH_LINKS);

    const link = page.getByTestId("room-header-description").locator('a[href="https://freenet.org/"]');
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

    const link = page.getByTestId("room-header-description").locator('a[href="https://freenet.org/"]');
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

    // The title still opens it, which also proves the heading locator above is live.
    await page.getByTestId("room-title-button").click();
    await expect(
      page.getByRole("heading", { name: /Room Details/i })
    ).toBeVisible({ timeout: 5_000 });
  });
});
