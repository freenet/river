import { test, expect, Page } from "@playwright/test";
import { waitForApp, selectRoom } from "./example-room";

// Coverage for freenet/river#402 — mobile / touch UX improvements:
//   1. Touch-accessible message action menu (kebab), since the hover action
//      bar can never appear on a device without a hover pointer.
//   2. Header hamburger spacing: see room-header-layout.spec.ts.
//   3. A scroll-to-latest button shown whenever the history is not pinned to
//      the bottom, plus a snap-to-bottom on room switch.

// Whether this browser context has no hover pointer (i.e. a touch device).
// The kebab is shown only in that case; the hover action bar only otherwise.
async function isTouchOnly(page: Page): Promise<boolean> {
  return page.evaluate(() => window.matchMedia("(hover: none)").matches);
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

test.describe("Message action kebab menu (#402.1)", () => {
  test("kebab visibility follows hover capability", async ({ page }) => {
    await page.goto("/");
    await waitForApp(page);
    await selectRoom(page, "Your Private Room");

    const kebab = page.locator('[data-testid="message-kebab"]').first();
    // Every message renders a kebab element; whether it is *displayed* is a
    // pure-CSS decision keyed on `@media (hover: none)`.
    await expect(kebab).toHaveCount(1);

    if (await isTouchOnly(page)) {
      await expect(kebab).toBeVisible();
    } else {
      // On a device with a hover pointer the kebab stays display:none — the
      // desktop hover action bar is used instead.
      await expect(kebab).toBeHidden();
    }
  });

  test("kebab opens a menu with Reply / Edit / Delete on own messages", async ({
    page,
  }) => {
    await page.goto("/");
    await waitForApp(page);
    await selectRoom(page, "Your Private Room");

    // This flow only applies where the kebab is actually usable (touch).
    test.skip(
      !(await isTouchOnly(page)),
      "kebab menu is touch-only; desktop uses the hover action bar"
    );

    // A self (right-aligned, accent-coloured) message bubble. Its row carries
    // the kebab that must expose Edit + Delete as well as Reply.
    const ownRow = page.locator('[id^="msg-"]:has(.bg-accent)').first();
    await expect(ownRow).toBeVisible();
    const ownKebab = ownRow.locator('[data-testid="message-kebab"]');
    await ownKebab.click();

    const menu = page.locator('[data-testid="message-action-menu"]');
    await expect(menu).toBeVisible();
    await expect(menu.getByRole("button", { name: "Reply" })).toBeVisible();
    await expect(menu.getByRole("button", { name: "Edit" })).toBeVisible();
    await expect(menu.getByRole("button", { name: "Delete" })).toBeVisible();

    // A tap far from the menu light-dismisses it.
    const vp = page.viewportSize();
    const box = await menu.boundingBox();
    const farX = box && vp && box.x > vp.width / 2 ? 5 : (vp?.width ?? 100) - 5;
    await page.mouse.click(farX, 5);
    await expect(menu).toBeHidden();
  });

  test("Reply from the kebab opens the composer reply preview", async ({
    page,
  }) => {
    await page.goto("/");
    await waitForApp(page);
    await selectRoom(page, "Your Private Room");
    test.skip(
      !(await isTouchOnly(page)),
      "kebab menu is touch-only; desktop uses the hover action bar"
    );

    const kebab = page.locator('[data-testid="message-kebab"]').first();
    await kebab.click();
    await page
      .locator('[data-testid="message-action-menu"]')
      .getByRole("button", { name: "Reply" })
      .click();

    // The composer shows a reply-preview strip (with a "Cancel reply" button)
    // once a reply target is set.
    await expect(page.getByTitle("Cancel reply")).toBeVisible({ timeout: 5_000 });
  });

  test("menu stays on-screen and dismisses via a far tap (narrow phone)", async ({
    page,
  }) => {
    await page.goto("/");
    await waitForApp(page);
    await selectRoom(page, "Your Private Room");
    test.skip(
      !(await isTouchOnly(page)),
      "kebab menu is touch-only; desktop uses the hover action bar"
    );

    const vp = page.viewportSize();
    // A self and a received message: their kebabs sit on opposite sides, and both menus must stay on screen.
    for (const sel of [
      '[id^="msg-"]:has(.bg-accent)',
      '[id^="msg-"]:has(.bg-surface)',
    ]) {
      const row = page.locator(sel).first();
      if ((await row.count()) === 0) continue;
      await row.locator('[data-testid="message-kebab"]').click();
      const menu = page.locator('[data-testid="message-action-menu"]');
      await expect(menu).toBeVisible();

      const box = await menu.boundingBox();
      expect(box).not.toBeNull();
      if (box && vp) {
        expect(box.x).toBeGreaterThanOrEqual(-1);
        expect(box.x + box.width).toBeLessThanOrEqual(vp.width + 1);
      }
      // The first action must be fully on-screen too.
      const replyBox = await menu
        .getByRole("button", { name: "Reply" })
        .boundingBox();
      if (replyBox && vp) {
        expect(replyBox.x).toBeGreaterThanOrEqual(-1);
        expect(replyBox.x + replyBox.width).toBeLessThanOrEqual(vp.width + 1);
      }
      // Opening the menu must not introduce a horizontal page scrollbar.
      const hScroll = await page.evaluate(
        () =>
          document.documentElement.scrollWidth >
          document.documentElement.clientWidth
      );
      expect(hScroll).toBe(false);

      // Dismiss via a far viewport tap before the next iteration.
      const farX = box && vp && box.x > vp.width / 2 ? 5 : (vp?.width ?? 100) - 5;
      await page.mouse.click(farX, 5);
      await expect(menu).toBeHidden();
    }
  });

  test("never more than one menu open at a time", async ({ page }) => {
    await page.goto("/");
    await waitForApp(page);
    await selectRoom(page, "Your Private Room");
    test.skip(
      !(await isTouchOnly(page)),
      "kebab menu is touch-only; desktop uses the hover action bar"
    );

    const menu = page.locator('[data-testid="message-action-menu"]');
    const kebabs = page.locator('[data-testid="message-kebab"]');

    await kebabs.nth(0).click();
    await expect(menu).toBeVisible();
    // One shared menu: another kebab's tap closes it, and the next tap opens it for that message.
    await kebabs.nth(2).click();
    await expect(menu).toBeHidden();
    await kebabs.nth(2).click();
    await expect(menu).toBeVisible();
    await expect(kebabs.nth(2)).toHaveAttribute("aria-expanded", "true");
  });

  test("nothing covers the open menu, even over later messages", async ({ page }) => {
    await page.goto("/");
    await waitForApp(page);
    await selectRoom(page, "Your Private Room");
    test.skip(
      !(await isTouchOnly(page)),
      "kebab menu is touch-only; desktop uses the hover action bar"
    );

    // The first message's menu opens over every later group.
    await page.locator('[data-testid="message-kebab"]').nth(0).click();
    const menu = page.locator('[data-testid="message-action-menu"]');
    await expect(menu).toBeVisible();
    const covered = await menu.evaluate((m) => {
      const r = m.getBoundingClientRect();
      // Edge midpoints 3px in, corners 8px in, as reaction-picker.spec.ts probes.
      const cx = (r.left + r.right) / 2;
      const cy = (r.top + r.bottom) / 2;
      return [
        [cx, r.top + 3], [cx, r.bottom - 3], [r.left + 3, cy], [r.right - 3, cy],
        [r.left + 8, r.top + 8], [r.right - 8, r.top + 8], [r.left + 8, r.bottom - 8], [r.right - 8, r.bottom - 8],
      ]
        .map(([x, y]) => document.elementFromPoint(x, y))
        .filter((hit) => !hit || !m.contains(hit))
        .map((hit) => (hit ? `${hit.tagName.toLowerCase()}[${(hit as HTMLElement).dataset.testid ?? ""}]` : "nothing"));
    });
    expect(covered, "something paints over the menu").toEqual([]);
    expect(await menu.evaluate((el) => el.matches(":popover-open"))).toBe(true);
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
