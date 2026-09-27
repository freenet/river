import { test, expect } from "@playwright/test";
import { openRoomWithComposer, waitForApp } from "./example-room";

// The bug only reshaped a group's last bubble, so target one.
const LAST_IN_GROUP = '[data-testid="conversation-history"] [id^="msg-"]:last-child';
const NO_REACTIONS = ':not(:has([data-testid="reaction-chip"]))';

test.describe("Bubble corners", { tag: "@chromium-only" }, () => {
  test.use({ viewport: { width: 1280, height: 800 } });

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
      await row.hover();
      await row.getByTestId("add-reaction-button").click();
      await page.getByTestId("emoji-picker").getByRole("button").first().click();
      await expect(row.getByTestId("reaction-chip")).toHaveCount(1);

      expect(await corners()).toEqual(before);
    });
  }
});
