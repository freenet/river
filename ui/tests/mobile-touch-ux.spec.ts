import { test, expect, Page } from "@playwright/test";
import { selectListedRoom, waitForApp } from "./example-room";

// Coverage for freenet/river#402 (mobile / touch UX): a scroll-to-latest
// button shown whenever the history is not pinned to the bottom, plus a
// snap-to-bottom on room switch. The #402 gap between the header hamburger and
// the room-name tap target is pinned in room-header-layout.spec.ts.

// Select a room at any viewport width. The list click itself is shared;
// reaching the list on a narrow screen stays here.
async function selectRoom(page: Page, roomName: string) {
  const listed = page.getByTestId("room-list").getByRole("button", { name: roomName });

  if (!(await listed.isVisible({ timeout: 500 }).catch(() => false))) {
    // Two elements carry this testid (the room header's, and the one shown
    // before any room is selected); only one renders at a time.
    const hamburger = page.locator('[data-testid="hamburger-rooms-button"]:visible');
    if (await hamburger.isVisible({ timeout: 500 }).catch(() => false)) {
      await hamburger.click();
    } else {
      const vp = page.viewportSize();
      if (vp && vp.width < 768) {
        await page.setViewportSize({ width: 1280, height: vp.height });
        await selectListedRoom(page, roomName);
        await page.setViewportSize({ width: vp.width, height: vp.height });
        // `--chat-col` (the bubble width cap) is published from a
        // ResizeObserver, so it lags the resize by a frame.
        await page.evaluate(
          () =>
            new Promise((r) => requestAnimationFrame(() => requestAnimationFrame(r)))
        );
        return;
      }
    }
  }

  await selectListedRoom(page, roomName);
}

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

    // Ground truth: the animated scroll returns the history to the bottom.
    await expect
      .poll(() => distanceFromBottom(page), { timeout: 8_000 })
      .toBeLessThan(120);
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

    // Enter a room and scroll up so it is no longer pinned to the bottom.
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
