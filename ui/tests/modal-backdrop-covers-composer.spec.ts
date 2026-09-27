import { test, expect, Page } from "@playwright/test";
import { openRoomWithComposer, waitForApp } from "./example-room";

// elementFromPoint only sees the modal's transparent z-50 wrapper; elementsFromPoint gives paint order.
async function paintOrderAtComposer(page: Page, backdropTestId: string) {
  return page.evaluate((id) => {
    const composer = document.querySelector('[data-testid="message-composer"]') as HTMLElement;
    const backdrop = document.querySelector(`[data-testid="${id}"]`);
    const r = composer.getBoundingClientRect();
    // Left edge of the composer: clear of the centred modal card.
    const stack = document.elementsFromPoint(r.left + 8, r.top + r.height / 2);
    return { backdropAt: stack.indexOf(backdrop!), composerAt: stack.findIndex((el) => composer.contains(el)) };
  }, backdropTestId);
}

test.describe("Modal backdrop covers the message composer", () => {
  test.use({ viewport: { width: 1280, height: 800 } });

  test.beforeEach(async ({ page }) => {
    await page.goto("/");
    await waitForApp(page);
    await openRoomWithComposer(page);
  });

  for (const { name, open, modal, backdrop } of [
    { name: "invite-member", open: "invite-member-button", modal: "invite-member-modal", backdrop: "invite-member-backdrop" },
    { name: "create-room", open: "create-room-button", modal: "create-room-modal", backdrop: "create-room-backdrop" },
  ]) {
    test(`${name} modal`, async ({ page }) => {
      await page.getByTestId(open).click();
      await expect(page.getByTestId(modal)).toBeVisible();
      const { backdropAt, composerAt } = await paintOrderAtComposer(page, backdrop);
      expect(backdropAt, "backdrop is under the sample point").toBeGreaterThanOrEqual(0);
      expect(composerAt, "composer is under the sample point").toBeGreaterThanOrEqual(0);
      expect(backdropAt, "backdrop paints above the composer").toBeLessThan(composerAt);
    });
  }
});
