import { test, expect, Page } from "@playwright/test";
import { openRoomWithComposer, waitForApp } from "./example-room";

// elementFromPoint only sees the modal's transparent z-50 wrapper; elementsFromPoint gives paint order.
async function paintOrderAtComposer(page: Page) {
  return page.evaluate(() => {
    const composer = document.querySelector('[data-testid="message-composer"]')!;
    const backdrop = document.querySelector('[data-testid="invite-member-backdrop"]')!;
    const r = composer.getBoundingClientRect();
    // Left edge of the composer: clear of the centred modal card.
    const stack = document.elementsFromPoint(r.left + 8, r.top + r.height / 2);
    return { backdropAt: stack.indexOf(backdrop), composerAt: stack.findIndex((el) => composer.contains(el)) };
  });
}

test.use({ viewport: { width: 1280, height: 800 } });

test("the Invite Member backdrop paints over the composer", async ({ page }) => {
  await page.goto("/");
  await waitForApp(page);
  await openRoomWithComposer(page);
  await page.getByTestId("invite-member-button").click();
  await expect(page.getByTestId("invite-member-backdrop")).toBeVisible();
  const { backdropAt, composerAt } = await paintOrderAtComposer(page);
  expect(backdropAt, "backdrop is under the sample point").toBeGreaterThanOrEqual(0);
  expect(composerAt, "composer is under the sample point").toBeGreaterThanOrEqual(0);
  expect(backdropAt, "backdrop paints above the composer").toBeLessThan(composerAt);
});
