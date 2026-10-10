import { test, expect } from "@playwright/test";
import { waitForApp, selectRoom } from "./example-room";

// freenet/river#718. The header description is one truncated line. A phone
// has no hover, and the ellipsis is not a control, so the rest of a long
// description was unreachable. The chevron (and a tap that is not on a link)
// reveals it. Links stay links, including while it is collapsed.

const ROOM = "Public Discussion Room";

test.describe("Room description expand", { tag: "@chromium-only" }, () => {
  test.use({ viewport: { width: 390, height: 844 } });

  test("a truncated description expands and collapses without opening room details", async ({
    page,
  }) => {
    await page.goto("/");
    await waitForApp(page);
    await selectRoom(page, ROOM);

    const description = page.getByTestId("room-header-description");
    const toggle = page.getByTestId("room-description-toggle");
    await expect(description).toBeVisible();
    await expect(description).toHaveAttribute("data-expanded", "false");
    await expect(toggle).toHaveAttribute("aria-expanded", "false");

    const overflows = await description.evaluate(
      (el) => el.scrollWidth > el.clientWidth + 1
    );
    expect(overflows).toBe(true);

    await toggle.click();
    await expect(description).toHaveAttribute("data-expanded", "true");
    await expect(toggle).toHaveAttribute("aria-expanded", "true");
    await expect(toggle).toHaveAttribute("aria-label", "Hide room description");

    const atlas = description.locator('a[href="https://freenet.org/atlas"]');
    await expect(atlas).toBeVisible();

    // A link click must not open room details, and must not collapse the text.
    await atlas.evaluate((el) => {
      el.removeAttribute("target");
      el.addEventListener("click", (e) => e.preventDefault(), { once: true });
    });
    await atlas.click();
    await page.waitForTimeout(50);
    await expect(page.getByRole("heading", { name: /Room Details/i })).toHaveCount(0);
    await expect(description).toHaveAttribute("data-expanded", "true");

    await toggle.click();
    await expect(description).toHaveAttribute("data-expanded", "false");
    await expect(toggle).toHaveAttribute("aria-expanded", "false");
  });

  test("tapping the description text, not a link, expands it", async ({ page }) => {
    await page.goto("/");
    await waitForApp(page);
    await selectRoom(page, ROOM);

    const description = page.getByTestId("room-header-description");
    await expect(description).toHaveAttribute("data-expanded", "false");
    // The chevron appears only after the line is measured as overflowing.
    await expect(page.getByTestId("room-description-toggle")).toBeVisible();
    // "Welcome" is the leading plain text, before the first link.
    await description.click({ position: { x: 8, y: 6 } });
    await expect(description).toHaveAttribute("data-expanded", "true");
  });
});
