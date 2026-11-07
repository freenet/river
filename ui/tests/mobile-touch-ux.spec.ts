import { test, expect, Page } from "@playwright/test";
import { coarsePointer, waitForApp, selectRoom } from "./example-room";
import { AT_BOTTOM_EPSILON_PX } from "./history-geometry";

// Coverage for freenet/river#402 — mobile / touch UX improvements:
//   1. Touch-accessible message actions: each message's footer shows React,
//      Reply, Edit and Delete at rest where a finger can be the pointer
//      (message-action-buttons.spec.ts has the rest of the footer).
//   2. Header hamburger spacing: see room-header-layout.spec.ts.
//   3. A scroll-to-latest button shown whenever the newest message's bottom
//      is off screen, plus a jump to the newest message on room switch.


// The app scrolls to the bottom asynchronously on room entry. Wait for that to
// settle before a test scrolls up, otherwise the pending async scroll races the
// test and snaps the history back down under it.
async function waitSettledAtBottom(page: Page) {
  await expect
    .poll(
      () =>
        page.evaluate(() => {
          const el = document.getElementById("chat-scroll-container");
          if (!el) return Number.MAX_SAFE_INTEGER;
          return el.scrollHeight - el.scrollTop - el.clientHeight;
        }),
      { timeout: 5_000 }
    )
    .toBeLessThan(120);
}

async function distanceFromBottom(page: Page): Promise<number> {
  return page.evaluate(() => {
    const el = document.getElementById("chat-scroll-container");
    if (!el) return Number.MAX_SAFE_INTEGER;
    return el.scrollHeight - el.scrollTop - el.clientHeight;
  });
}

// Scroll the history to the top and wait for the scroll-to-latest button to
// appear. iOS WebKit momentum scrolling plus the app's async entry-scroll and
// the deferred (setTimeout-based) IntersectionObserver can otherwise miss a
// single programmatic scroll, so re-assert scrollTop=0 on each poll until the
// observer registers "not at bottom" and the button renders.
async function scrollUpUntilButtonVisible(page: Page) {
  const button = page.locator('[data-testid="scroll-to-bottom"]');
  // Jump to the top and hold there briefly to defeat the app's async
  // entry-scroll, then STOP scrolling. WebKit's IntersectionObserver lags on a
  // programmatic scroll (worsened by -webkit-overflow-scrolling: touch) but DOES
  // fire once the position is stable — continuously re-scrolling instead keeps
  // rescheduling the deferred observer callback so it never settles.
  await page.evaluate(
    () =>
      new Promise<void>((resolve) => {
        const el = document.getElementById("chat-scroll-container");
        let n = 0;
        const id = setInterval(() => {
          if (el) el.scrollTop = 0;
          if (++n > 5) {
            clearInterval(id);
            resolve();
          }
        }, 80);
      })
  );
  // Position is now stable at the top; give the lagging observer time to fire.
  await expect(button).toBeVisible({ timeout: 12_000 });
}

test.describe("Message footer on touch (#402.1)", () => {
  test("footer actions show at rest where a finger can be the pointer, and wait for hover where not", async ({ page }) => {
    await page.goto("/");
    await waitForApp(page);
    await selectRoom(page, "Your Private Room");

    const ownRow = page.locator('[id^="msg-"][data-message-self="true"]').last();
    await ownRow.scrollIntoViewIfNeeded();
    await page.mouse.move(0, 0);
    const opacity = (id: string) =>
      ownRow.getByTestId(id).evaluate(async (el) => {
        await Promise.all(el.getAnimations().map((a) => a.finished));
        return getComputedStyle(el).opacity;
      });
    const expected = (await coarsePointer(page)) ? "1" : "0";
    for (const id of ["message-react-button", "message-reply-button", "message-edit-button", "message-delete-button"]) {
      expect(await opacity(id), id).toBe(expected);
    }
  });

  test("while the picker is open, a tap on another message's control only closes it", async ({ page }) => {
    await page.goto("/");
    await waitForApp(page);
    await selectRoom(page, "Your Private Room");
    test.skip(!(await coarsePointer(page)), "footer actions show at rest only where a finger can be the pointer");

    const rows = page.locator('[id^="msg-"]');
    const first = rows.last();
    await first.scrollIntoViewIfNeeded();
    await first.getByTestId("message-react-button").click();
    await expect(page.getByTitle(/^React with/).first()).toBeVisible({ timeout: 5_000 });

    // The picker's raised backdrop covers every other row's controls, so a
    // tap at another message's Reply lands on it and closes the picker; no
    // second action starts from that tap. Dispatched at the coordinate,
    // since Playwright refuses to click an obscured element.
    const other = rows.nth((await rows.count()) - 2);
    const box = await other.getByTestId("message-reply-button").boundingBox();
    expect(box).not.toBeNull();
    const [x, y] = [box!.x + box!.width / 2, box!.y + box!.height / 2];
    const at = await page.evaluate(([x, y]) => {
      const el = document.elementFromPoint(x, y);
      return { picker: !!el?.closest('[data-testid="emoji-picker"]'), reply: !!el?.closest('[data-testid="message-reply-button"]') };
    }, [x, y]);
    expect(at, "premise: the tap lands outside the picker, over the other Reply").toEqual({ picker: false, reply: false });
    const chips = await page.getByTestId("reaction-chip").count();
    await page.mouse.click(x, y);
    await expect(page.getByTitle(/^React with/)).toHaveCount(0);
    expect(await page.getByTestId("reaction-chip").count(), "nothing was reacted").toBe(chips);
    await expect(page.getByTitle("Cancel reply")).toHaveCount(0);
  });
});

test.describe("Scroll-to-latest button (#402.3)", () => {
  // A short viewport guarantees the example history overflows and is scrollable
  // regardless of the (randomised) example message lengths.
  test.use({ viewport: { width: 500, height: 400 } });

  test("appears when scrolled up and returns to bottom on click", async ({
    page,
  }) => {
    await page.goto("/");
    await waitForApp(page);
    await selectRoom(page, "Team Chat Room");

    const button = page.locator('[data-testid="scroll-to-bottom"]');
    // Pinned to the bottom on entry (once the async entry-scroll settles): no button.
    await waitSettledAtBottom(page);
    await expect(button).toHaveCount(0);

    // Scroll the history to the top; the button must appear.
    await scrollUpUntilButtonVisible(page);
    await expect(button).toBeVisible();

    await button.click();

    // Latest jumps to the end instantly.
    await expect
      .poll(() => distanceFromBottom(page), { timeout: 2_000 })
      .toBeLessThanOrEqual(AT_BOTTOM_EPSILON_PX);
    // Once the observer sees the sentinel again, the button hides.
    await expect(button).toBeHidden({ timeout: 5_000 });
  });
});

test.describe("Room-switch scroll reset (#402.3)", () => {
  // Short viewport so the example history overflows and is scrollable.
  test.use({ viewport: { width: 1280, height: 420 } });

  test("switching rooms lands at the bottom even after scrolling up", async ({
    page,
  }) => {
    await page.goto("/");
    await waitForApp(page);

    // Enter a room and scroll up until its newest message is off screen.
    await selectRoom(page, "Your Private Room");
    await waitSettledAtBottom(page);
    await scrollUpUntilButtonVisible(page);

    // Switch away and back. The Conversation component is reused across rooms,
    // so without the room-change reset the scroll position would persist near
    // the top. It must snap back to the newest message instead.
    await selectRoom(page, "Team Chat Room");
    await selectRoom(page, "Your Private Room");

    await expect.poll(() => distanceFromBottom(page), { timeout: 5_000 }).toBeLessThan(120);
  });
});
