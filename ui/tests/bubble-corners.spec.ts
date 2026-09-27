import { test, expect, Page } from "@playwright/test";
import { openRoomWithComposer, selectRoom, waitForApp } from "./example-room";

async function unevenBubbles(page: Page) {
  return page.evaluate(() =>
    Array.from(document.querySelectorAll('[data-testid="message-bubble"]')).flatMap((el) => {
      const s = getComputedStyle(el);
      const radii = [s.borderTopLeftRadius, s.borderTopRightRadius, s.borderBottomRightRadius, s.borderBottomLeftRadius];
      return new Set(radii).size === 1 ? [] : [{ radii, text: (el.textContent ?? "").replace(/\s+/g, " ").slice(0, 40) }];
    }),
  );
}

// The bug only reshaped a group's last bubble, so target one.
const LAST_IN_GROUP = '[data-testid="conversation-history"] [id^="msg-"]:last-child';
const NO_REACTIONS = ':not(:has([data-testid="reaction-chip"]))';

test.describe("Bubble corners", { tag: "@chromium-only" }, () => {
  test.use({ viewport: { width: 1280, height: 800 } });

  test("every corner of a bubble is the same radius", async ({ page }) => {
    await page.goto("/");
    await waitForApp(page);
    for (const room of ["Your Private Room", "Public Discussion Room", "Team Chat Room"]) {
      await selectRoom(page, room);
      expect(await unevenBubbles(page), room).toEqual([]);
    }
  });

  for (const kind of ["own", "received"] as const) {
    test(`a reaction leaves the bubble's corners unchanged (${kind})`, async ({ page }) => {
      await page.goto("/");
      await waitForApp(page);
      await openRoomWithComposer(page);

      const side = kind === "own" ? ":has(.bg-accent)" : ":not(:has(.bg-accent))";
      // Pin by id: NO_REACTIONS stops matching once the reaction lands.
      const id = await page.locator(`${LAST_IN_GROUP}${side}${NO_REACTIONS}`).last().getAttribute("id");
      const row = page.locator(`[id="${id}"]`);
      const corners = () => row.getByTestId("message-bubble").evaluate((el) => {
        const s = getComputedStyle(el);
        return [s.borderTopLeftRadius, s.borderTopRightRadius, s.borderBottomRightRadius, s.borderBottomLeftRadius];
      });

      await row.scrollIntoViewIfNeeded();
      const before = await corners();
      expect(new Set(before).size, "every corner is the same radius").toBe(1);
      await row.hover();
      await row.getByTestId("add-reaction-button").click();
      await page.getByTestId("emoji-picker").getByRole("button").first().click();
      await expect(row.getByTestId("reaction-chip")).toHaveCount(1);

      expect(await corners()).toEqual(before);
    });
  }
});
