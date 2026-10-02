import { test, expect, Page } from "@playwright/test";
import { waitForApp, selectRoom } from "./example-room";
import { distanceFromBottom } from "./history-scroll-helpers";

// Coverage for freenet/river#402 — mobile / touch UX improvements:
//   1. Touch-accessible message action menu (kebab), since the hover action
//      bar can never appear on a device without a hover pointer.
//   2. Header hamburger spacing: see room-header-layout.spec.ts.
//   3. Scroll-to-latest button and room-switch position: see
//      conversation-scroll-to-latest-button.spec.ts and
//      conversation-room-position.spec.ts.

// Whether this browser context has no hover pointer (i.e. a touch device).
// The kebab is shown only in that case; the hover action bar only otherwise.
async function isTouchOnly(page: Page): Promise<boolean> {
  return page.evaluate(() => window.matchMedia("(hover: none)").matches);
}

// The app places a room opened for the first time at its newest message once
// its rows are laid out. Wait for that before a test scrolls up, so the test
// does not race the placement.
async function waitSettledAtBottom(page: Page) {
  await expect.poll(() => distanceFromBottom(page), { timeout: 5_000 }).toBeLessThan(120);
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

    // Tapping anywhere else (a real viewport coordinate far from the menu, NOT
    // the backdrop's own local origin) dismisses the menu — this verifies the
    // fixed backdrop actually covers the viewport, not just the kebab box.
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

  test("React from the kebab opens the emoji picker and closes the menu", async ({
    page,
  }) => {
    await page.goto("/");
    await waitForApp(page);
    await selectRoom(page, "Your Private Room");
    test.skip(
      !(await isTouchOnly(page)),
      "kebab menu is touch-only; desktop uses the hover action bar"
    );

    const menu = page.locator('[data-testid="message-action-menu"]');
    await page.locator('[data-testid="message-kebab"]').first().click();
    await menu.getByRole("button", { name: "React" }).click();

    // The action menu closes and the emoji picker (emoji buttons titled
    // "React with …") opens.
    await expect(menu).toBeHidden();
    await expect(page.getByTitle(/^React with/).first()).toBeVisible({
      timeout: 5_000,
    });

    // While the picker is open its raised backdrop covers the kebabs, so a tap
    // at a kebab lands on that backdrop and dismisses the picker (the two
    // popovers can't stack). No action menu opens from that same tap.
    const kbox = await page
      .locator('[data-testid="message-kebab"]')
      .first()
      .boundingBox();
    expect(kbox).not.toBeNull();
    if (kbox) {
      await page.mouse.click(kbox.x + kbox.width / 2, kbox.y + kbox.height / 2);
    }
    await expect(page.getByTitle(/^React with/)).toHaveCount(0);
    await expect(menu).toBeHidden();
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
    // Both a self (accent bubble) and a received (surface bubble) message: the
    // menu opens on opposite sides, so both must stay within the viewport.
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
      // The menu content (first action) must be fully on-screen, not clipped by
      // the scroll container's overflow-x-hidden backstop.
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

  test("menu is capped to the scrollport height on a short viewport", async ({
    page,
  }) => {
    await page.setViewportSize({ width: 500, height: 340 });
    await page.goto("/");
    await waitForApp(page);
    await selectRoom(page, "Your Private Room");
    test.skip(
      !(await isTouchOnly(page)),
      "kebab menu is touch-only; desktop uses the hover action bar"
    );

    // Own message (4-row menu) opened on a short scrollport: the menu must be
    // capped to the available space (and scroll internally) rather than extend
    // past the scroll container with actions unreachable.
    await page
      .locator('[id^="msg-"]:has(.bg-accent)')
      .first()
      .locator('[data-testid="message-kebab"]')
      .click();
    const menu = page.locator('[data-testid="message-action-menu"]');
    await expect(menu).toBeVisible();

    const box = await menu.boundingBox();
    const scrollportH = await page.evaluate(() => {
      const el = document.getElementById("chat-scroll-container");
      return el ? el.clientHeight : 0;
    });
    expect(box).not.toBeNull();
    if (box) {
      // Fits within the scrollport (a few px slack), so nothing is clipped away.
      expect(box.height).toBeLessThanOrEqual(scrollportH + 4);
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

    const menus = page.getByTestId("message-action-menu");
    const kebabs = page.getByTestId("message-kebab");
    // A kebab click scrolls its row into view; let the opening snap land first
    // so it can't move the history between that scroll and the taps below.
    await waitSettledAtBottom(page);

    // Example messages have random lengths, so pick two kebabs from what is on
    // screen rather than by index: a fixed index can sit below the viewport,
    // where a tap reaches no element at all. The target comes later in the DOM
    // than the opener, so only the open wrapper's z-[60] raise keeps it covered.
    const pair = await page.evaluate(() => {
      const sp = document.getElementById("chat-scroll-container")!.getBoundingClientRect();
      const visible = Array.from(document.querySelectorAll('[data-testid="message-kebab"]'))
        .map((el, i) => ({ i, r: el.getBoundingClientRect() }))
        .filter(({ r }) => r.top >= sp.top && r.bottom <= Math.min(sp.bottom, innerHeight));
      let best: [number, number] | null = null;
      let bestScore = 0;
      for (const a of visible) {
        for (const b of visible) {
          if (b.i <= a.i) continue;
          // Prefer the opposite gutter, then the largest gap: either keeps the
          // target clear of the opener's menu, which is checked once it opens.
          const score = (a.r.left !== b.r.left ? 1000 : 0) + (b.r.top - a.r.top);
          if (score > bestScore) [best, bestScore] = [[a.i, b.i], score];
        }
      }
      return best;
    });
    expect(pair, "premise: two kebabs fully inside the scrollport").not.toBeNull();
    const [opener, target] = pair!;

    await kebabs.nth(opener).click();
    await expect(menus).toHaveCount(1);

    // Measure and hit-test the target in one evaluation. The tap is a separate
    // step and can still race a layout change; if it fails, compare this point
    // with the trace before blaming the backdrop.
    const tap = await page.evaluate((target) => {
      const sp = document.getElementById("chat-scroll-container")!.getBoundingClientRect();
      const menu = document.querySelector('[data-testid="message-action-menu"]')!.getBoundingClientRect();
      const r = document.querySelectorAll('[data-testid="message-kebab"]')[target].getBoundingClientRect();
      const x = r.left + r.width / 2;
      const y = r.top + r.height / 2;
      const inside = (b: DOMRect) => x >= b.left && x <= b.right && y >= b.top && y <= b.bottom;
      const onScreen = inside(sp) && x <= innerWidth && y <= innerHeight;
      const hit = document.elementFromPoint(x, y)?.closest("[data-testid]")?.getAttribute("data-testid");
      return { x, y, onScreen, inMenu: inside(menu), hit };
    }, target);
    test.info().annotations.push({ type: "tap", description: JSON.stringify(tap) });
    expect(tap.onScreen, `premise: (${tap.x}, ${tap.y}) is inside the scrollport`).toBe(true);
    expect(tap.inMenu, `premise: (${tap.x}, ${tap.y}) is outside the open menu`).toBe(false);
    // Soft, so a stacking regression still reaches the tap and shows what it does.
    expect.soft(tap.hit, "the open menu's backdrop covers the other kebab").toBe("message-action-menu-backdrop");

    // A real tap: forcing a click on the kebab or dispatching one to the
    // backdrop would skip the z-order this test is about.
    await page.mouse.click(tap.x, tap.y);
    await expect(menus).toHaveCount(0);
    await expect(kebabs.and(page.locator('[aria-expanded="true"]'))).toHaveCount(0);
  });
});
