import { test, expect, Page } from "@playwright/test";

// Coverage for freenet/river#402 — mobile / touch UX improvements:
//   1. Touch-accessible message action menu (kebab), since the hover action
//      bar can never appear on a device without a hover pointer.
//   2. Extra spacing between the header hamburger (open room list) and the
//      room-name/details tap target, so switching rooms does not accidentally
//      open the room-details modal.
//   3. A scroll-to-latest button shown whenever the history is not pinned to
//      the bottom, plus a snap-to-bottom on room switch.

// Helper: wait for WASM app to fully render
async function waitForApp(page: Page) {
  await page.waitForSelector(".app-root", { timeout: 30_000 });
  await expect(page.locator("aside, .app-root button")).not.toHaveCount(0);
}

// Helper: select a room at any viewport width (mirrors responsive-layout.spec).
async function selectRoom(page: Page, roomName: string) {
  const roomBtn = page.getByRole("button", { name: roomName });

  if (!(await roomBtn.isVisible({ timeout: 500 }).catch(() => false))) {
    const hamburger = page.locator(
      ".border-b.border-border.bg-panel button >> nth=0"
    );
    if (await hamburger.isVisible({ timeout: 500 }).catch(() => false)) {
      await hamburger.click();
      await expect(roomBtn).toBeVisible({ timeout: 5_000 });
    } else {
      const vp = page.viewportSize();
      if (vp && vp.width < 768) {
        await page.setViewportSize({ width: 1280, height: vp.height });
        await expect(roomBtn).toBeVisible({ timeout: 5_000 });
        await roomBtn.click();
        await expect(
          page.getByRole("heading", { name: roomName })
        ).toBeVisible({ timeout: 5_000 });
        await page.setViewportSize({ width: vp.width, height: vp.height });
        return;
      }
    }
  }

  await roomBtn.click();
  await expect(
    page.getByRole("heading", { name: roomName })
  ).toBeVisible({ timeout: 5_000 });
}

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

// Scroll the history to the top and HOLD it there for a few frames. iOS WebKit
// momentum scrolling plus the app's async entry-scroll can otherwise snap the
// container back down before the IntersectionObserver registers that the user
// left the bottom. Holding at the top gives the observer a stable frame to fire
// (a real finger-scroll produces the same sustained not-at-bottom state).
async function scrollHistoryToTop(page: Page) {
  await page.evaluate(
    () =>
      new Promise<void>((resolve) => {
        const el = document.getElementById("chat-scroll-container");
        let n = 0;
        const id = setInterval(() => {
          if (el) el.scrollTop = 0;
          if (++n > 6) {
            clearInterval(id);
            resolve();
          }
        }, 60);
      })
  );
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

    // Tapping the backdrop dismisses the menu.
    await page.locator(".fixed.inset-0").first().click({ position: { x: 5, y: 5 } });
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
});

test.describe("Mobile header hamburger spacing (#402.2)", () => {
  test.use({ viewport: { width: 390, height: 844 } });

  test("hamburger does not overlap the room-name tap target", async ({
    page,
  }) => {
    await page.goto("/");
    await waitForApp(page);
    await selectRoom(page, "Team Chat Room");

    const header = page.locator(".border-b.border-border.bg-panel").first();
    const hamburger = header.locator("button").first();
    // The room-details button is the one wrapping the room-name heading.
    const roomDetails = header.locator("button:has(h2)").first();

    const hb = await hamburger.boundingBox();
    const rb = await roomDetails.boundingBox();
    expect(hb).not.toBeNull();
    expect(rb).not.toBeNull();
    if (hb && rb) {
      // The room-details target must start strictly to the right of the
      // hamburger, with a real gap rather than an overlapping hit area.
      const gap = rb.x - (hb.x + hb.width);
      expect(gap).toBeGreaterThanOrEqual(4);
    }
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
    await scrollHistoryToTop(page);
    await expect(button).toBeVisible({ timeout: 5_000 });

    await button.click();

    // After clicking, the history returns to the bottom and the button hides.
    await expect(button).toBeHidden({ timeout: 5_000 });
    // The scroll is animated (smooth), so poll until it settles at the bottom.
    await expect.poll(() => distanceFromBottom(page), { timeout: 5_000 }).toBeLessThan(120);
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
    await scrollHistoryToTop(page);
    await expect(
      page.locator('[data-testid="scroll-to-bottom"]')
    ).toBeVisible({ timeout: 5_000 });

    // Switch away and back. The Conversation component is reused across rooms,
    // so without the room-change reset the scroll position would persist near
    // the top. It must snap back to the newest message instead.
    await selectRoom(page, "Team Chat Room");
    await selectRoom(page, "Your Private Room");

    await expect.poll(() => distanceFromBottom(page), { timeout: 5_000 }).toBeLessThan(120);
  });
});
