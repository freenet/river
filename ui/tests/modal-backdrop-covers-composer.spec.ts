import { test, expect, Page } from "@playwright/test";
import { openRoomWithComposer } from "./example-room";

// elementFromPoint only sees the modal's transparent z-50 wrapper; elementsFromPoint gives paint order.
async function paintOrderAtComposer(page: Page, backdropId = "invite-member-backdrop") {
  return page.evaluate((backdropId) => {
    const composer = document.querySelector('[data-testid="message-composer"]')!;
    const backdrop = document.querySelector(`[data-testid="${backdropId}"]`)!;
    const r = composer.getBoundingClientRect();
    // Left edge of the composer: clear of the centred modal card.
    const stack = document.elementsFromPoint(r.left + 8, r.top + r.height / 2);
    return { backdropAt: stack.indexOf(backdrop), composerAt: stack.findIndex((el) => composer.contains(el)) };
  }, backdropId);
}

test.use({ viewport: { width: 1280, height: 800 } });

test("the Invite Member backdrop paints over the composer", async ({ page }) => {
  await openRoomWithComposer(page);
  await page.getByTestId("invite-member-button").click();
  await expect(page.getByTestId("invite-member-backdrop")).toBeVisible();
  const { backdropAt, composerAt } = await paintOrderAtComposer(page);
  expect(backdropAt, "backdrop is under the sample point").toBeGreaterThanOrEqual(0);
  expect(composerAt, "composer is under the sample point").toBeGreaterThanOrEqual(0);
  expect(backdropAt, "backdrop paints above the composer").toBeLessThan(composerAt);
});

// These two mount inside the rooms rail, which precedes the conversation in
// the DOM, so an equal z-index let the composer paint over their backdrops.
for (const [what, open, backdrop] of [
  ["Enter invite code", "join-with-code-button", "join-with-code-backdrop"],
  ["Import ID", "import-id-button", "import-identity-backdrop"],
] as const) {
  test(`the ${what} backdrop paints over the composer`, async ({ page }) => {
    await openRoomWithComposer(page);
    await page.getByTestId(open).click();
    await expect(page.getByTestId(backdrop)).toBeVisible();
    const { backdropAt, composerAt } = await paintOrderAtComposer(page, backdrop);
    expect(backdropAt, "backdrop is under the sample point").toBeGreaterThanOrEqual(0);
    expect(composerAt, "composer is under the sample point").toBeGreaterThanOrEqual(0);
    expect(backdropAt, "backdrop paints above the composer").toBeLessThan(composerAt);
  });
}
