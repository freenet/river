import { test, expect } from "@playwright/test";
import { memberRows, openRoomWithComposer, resolveColor, selectRoom, settle, waitForApp } from "./example-room";
import {
  READING_ROW_BUDGET_PX,
  expectParkedAwayFromEnd,
  expectSettledAtBottom,
  fillHistory,
  gateResizeObservers,
  holdResizeObservations,
  openRoomAtBottom,
  readerScrollsWithoutGesture,
  releaseResizeObservations,
  scrollMetrics,
} from "./history-geometry";
import { callRiverTest } from "./river-test";

/// The reading row as the app defines it (history-scrolling.md): the first
/// message or event row whose top lies inside the view, with that top.
function anchorRow(page: import("@playwright/test").Page, key?: string): Promise<{ key: string; top: number } | null> {
  return page.evaluate((wanted) => {
    const c = document.getElementById("chat-scroll-container")!;
    const view = c.getBoundingClientRect();
    for (const el of Array.from(c.querySelectorAll<HTMLElement>("[data-anchor-key]"))) {
      const r = el.getBoundingClientRect();
      const k = el.getAttribute("data-anchor-key")!;
      if (wanted ? k === wanted : r.top >= view.top - 0.5 && r.top < view.bottom) return { key: k, top: r.top - view.top };
    }
    return null;
  }, key ?? null);
}

/// Every localStorage key and value (the gateway iframe may throw on access).
function localStorageSnapshot(page: import("@playwright/test").Page): Promise<Record<string, string> | null> {
  return page.evaluate(() => {
    try {
      const out: Record<string, string> = {};
      for (let i = 0; i < window.localStorage.length; i++) {
        const k = window.localStorage.key(i)!;
        out[k] = window.localStorage.getItem(k)!;
      }
      return out;
    } catch {
      return null;
    }
  });
}

// The member panel: collapsible on desktop from the header's members toggle
// (open by default, for the session only), and its rows show an avatar, the
// name and the markers as quiet line icons.

test.describe("Member panel on desktop", () => {
  test.use({ viewport: { width: 1280, height: 800 } });

  test("the header toggle hides and shows it, pressed while it shows", async ({ page }) => {
    await openRoomWithComposer(page);
    const panel = page.getByTestId("members-rail");
    const toggle = page.getByTestId("members-panel-toggle");
    const scroller = page.locator("#chat-scroll-container");
    await expect(panel).toBeVisible();
    await expect(toggle).toHaveAttribute("aria-pressed", "true");
    await expect(toggle).toHaveAttribute("aria-controls", "members-rail");
    const count = await memberRows(page).count();
    await expect(toggle).toHaveAccessibleName(`Members, ${count}`);
    await expect(toggle).toContainText(String(count));
    const narrow = (await scroller.boundingBox())!.width;

    await toggle.click();
    await expect(panel).toBeHidden();
    await expect(toggle).toHaveAttribute("aria-pressed", "false");
    // The conversation takes the space.
    await expect.poll(async () => (await scroller.boundingBox())!.width).toBeGreaterThan(narrow + 200);

    await toggle.click();
    await expect(panel).toBeVisible();
    await expect(toggle).toHaveAttribute("aria-pressed", "true");
  });

  test("the choice lasts the session only: a reload opens it again", async ({ page }) => {
    await openRoomWithComposer(page);
    await page.getByTestId("members-panel-toggle").click();
    await expect(page.getByTestId("members-rail")).toBeHidden();
    // Switching rooms keeps it closed.
    await selectRoom(page, "Team Chat Room");
    await expect(page.getByTestId("members-rail")).toBeHidden();
    await page.reload();
    await waitForApp(page);
    await selectRoom(page, "Team Chat Room");
    await expect(page.getByTestId("members-rail")).toBeVisible();
    expect(await page.evaluate(() => {
      try {
        return Object.keys(window.localStorage).filter((k) => /member/i.test(k));
      } catch {
        return [];
      }
    }), "no member-panel state in localStorage").toEqual([]);
  });

  test("toggling writes nothing to localStorage", async ({ page }) => {
    await openRoomWithComposer(page);
    await settle(page);
    const before = await localStorageSnapshot(page);
    const toggle = page.getByTestId("members-panel-toggle");
    await toggle.click();
    await expect(page.getByTestId("members-rail")).toBeHidden();
    await settle(page);
    expect(await localStorageSnapshot(page), "after hiding").toEqual(before);
    await toggle.click();
    await expect(page.getByTestId("members-rail")).toBeVisible();
    await settle(page);
    expect(await localStorageSnapshot(page), "after showing").toEqual(before);
  });

  test("hidden on desktop, the phone's Members view still works, and it stays hidden back on desktop", async ({ page }) => {
    await openRoomWithComposer(page);
    await page.getByTestId("members-panel-toggle").click();
    await expect(page.getByTestId("members-rail")).toBeHidden();

    await page.setViewportSize({ width: 390, height: 844 });
    await page.getByTestId("header-members-button").click();
    await expect(page.getByTestId("members-rail")).toBeVisible();
    await expect(memberRows(page).first()).toBeVisible();
    await page.getByTestId("members-back-to-chat-button").click();
    await expect(page.getByTestId("message-composer")).toBeVisible();

    await page.setViewportSize({ width: 1280, height: 800 });
    await expect(page.getByTestId("members-panel-toggle")).toHaveAttribute("aria-pressed", "false");
    await expect(page.getByTestId("members-rail")).toBeHidden();
  });

  // The panel changes the chat column's width, so every row rewraps. At
  // 820px the column is narrower than the 76ch measure whether the panel is
  // open or not, so every body rewraps (wider, the text sits at its measure
  // and only the odd long line changes). At 820 the panel starts hidden
  // (automatic below 1024px), so the first toggle shows it.
  test("a reader at the end stays at the end through a toggle", async ({ page }) => {
    await page.setViewportSize({ width: 820, height: 768 });
    await openRoomAtBottom(page, "Your Private Room");
    await fillHistory(page);
    const toggle = page.getByTestId("members-panel-toggle");
    for (const step of ["show", "hide"]) {
      const height = (await scrollMetrics(page)).height;
      await toggle.click();
      await expect.poll(async () => (await scrollMetrics(page)).height, { message: `premise: ${step} rewraps the rows` }).not.toBe(height);
      await settle(page);
      await expectSettledAtBottom(page, `after ${step}`);
    }
  });

  test("a parked reader keeps their row through a toggle", async ({ page }) => {
    await page.setViewportSize({ width: 820, height: 768 });
    await openRoomAtBottom(page, "Your Private Room");
    await fillHistory(page);
    // Park mid-history: well away from both ends.
    const m = await scrollMetrics(page);
    await readerScrollsWithoutGesture(page, Math.round(m.max / 2));
    await settle(page);
    await expectParkedAwayFromEnd(page);
    const toggle = page.getByTestId("members-panel-toggle");
    for (const step of ["show", "hide"]) {
      const row = (await anchorRow(page))!;
      expect(row, "premise: a row starts in view").not.toBeNull();
      const height = (await scrollMetrics(page)).height;
      await toggle.click();
      await expect.poll(async () => (await scrollMetrics(page)).height, { message: `premise: ${step} rewraps the rows` }).not.toBe(height);
      await settle(page);
      const after = await anchorRow(page, row.key);
      expect(after, `after ${step}: the reading row is still rendered`).not.toBeNull();
      expect(Math.abs(after!.top - row.top), `after ${step}: the reading row stays put`).toBeLessThanOrEqual(READING_ROW_BUDGET_PX);
    }
  });

  /// Park the reader mid-history at 820px (where rows rewrap) and return
  /// their reading row.
  async function parkMidHistory(page: import("@playwright/test").Page) {
    await page.setViewportSize({ width: 820, height: 768 });
    await openRoomAtBottom(page, "Your Private Room");
    await fillHistory(page);
    const m = await scrollMetrics(page);
    await readerScrollsWithoutGesture(page, Math.round(m.max / 2));
    await settle(page);
    await expectParkedAwayFromEnd(page);
    const row = await anchorRow(page);
    expect(row, "premise: a row starts in view").not.toBeNull();
    return row!;
  }

  test("a slow resize report does not lose the reader's row", async ({ page }) => {
    // No time limit: the correction follows the toggle's render, not a
    // resize that might come late (a slow tablet), here held over a second.
    await gateResizeObservers(page);
    const row = await parkMidHistory(page);
    const height = (await scrollMetrics(page)).height;
    await holdResizeObservations(page);
    await page.getByTestId("members-panel-toggle").click();
    await expect.poll(async () => (await scrollMetrics(page)).height, { message: "premise: the rows rewrap" }).not.toBe(height);
    await page.waitForTimeout(1_200);
    expect(await releaseResizeObservations(page), "premise: the toggle's resize reports were held").toBeGreaterThan(0);
    await settle(page);
    const after = await anchorRow(page, row.key);
    expect(Math.abs(after!.top - row.top), "the reading row stays put").toBeLessThanOrEqual(READING_ROW_BUDGET_PX);
  });

  /// After a toggle that changed nothing it can correct, the reader moves far
  /// away and the window then resizes: the old capture must not pull them
  /// back.
  async function expectNoStaleRestore(page: import("@playwright/test").Page, why: string) {
    const scroller = page.locator("#chat-scroll-container");
    const m = await scrollMetrics(page);
    await readerScrollsWithoutGesture(page, Math.max(0, m.top - 700));
    await settle(page);
    const moved = await scroller.evaluate((c) => c.scrollTop);
    const size = page.viewportSize()!;
    await page.setViewportSize({ width: size.width + 24, height: size.height });
    await settle(page);
    expect(Math.abs((await scroller.evaluate((c) => c.scrollTop)) - moved), why).toBeLessThan(300);
  }

  test("a double toggle keeps the reading row and leaves no stale correction", async ({ page }) => {
    const row = await parkMidHistory(page);
    // Two clicks before either renders: two deferred writes, each rendered
    // on its own (open, then closed again: at 820 it starts hidden), so the
    // rows rewrap twice.
    await expect(page.getByTestId("members-rail"), "premise: hidden at 820").toBeHidden();
    await page.getByTestId("members-panel-toggle").evaluate((el) => {
      (el as HTMLElement).click();
      (el as HTMLElement).click();
    });
    await settle(page);
    await expect(page.getByTestId("members-rail")).toBeHidden();
    const after = await anchorRow(page, row.key);
    expect(Math.abs(after!.top - row.top), "the reading row stays put").toBeLessThanOrEqual(READING_ROW_BUDGET_PX);
    await expectNoStaleRestore(page, "a later resize did not apply the double toggle's capture");
  });

  // A regression guard for the path where a capture could outlive its
  // toggle. Both the old (deadline) and the new (render) design pass it; it
  // does not tell them apart.
  test("a toggle at 1920, where nothing rewraps, leaves nothing for a later resize", async ({ page }) => {
    // At 1920 the text is at its measure either way.
    await page.setViewportSize({ width: 1920, height: 1080 });
    await openRoomAtBottom(page, "Your Private Room");
    await fillHistory(page);
    const m = await scrollMetrics(page);
    await readerScrollsWithoutGesture(page, Math.round(m.max / 2));
    await settle(page);
    await expectParkedAwayFromEnd(page);
    await page.getByTestId("members-panel-toggle").click();
    await expect(page.getByTestId("members-rail")).toBeHidden();
    await settle(page);
    await expectNoStaleRestore(page, "a later resize did not apply the toggle's capture");
  });

  test("an empty room's member avatars are coloured", async ({ page }) => {
    // Only the empty room is opened, so nothing in a history ever asked for
    // its salt: the member panel has to.
    await page.goto("/?empty-room=1");
    await waitForApp(page);
    await selectRoom(page, "Quiet Room");
    await expect(page.getByTestId("message-row")).toHaveCount(0);
    const avatars = page.getByTestId("members-rail").locator(".member-avatar");
    await expect(avatars.first()).toBeVisible();
    await expect
      .poll(() => avatars.evaluateAll((els) => els.map((e) => (e as HTMLElement).style.getPropertyValue("--avatar-hue"))), {
        message: "every member's disc has a hue",
      })
      .toEqual(expect.not.arrayContaining([""]));
  });

  test("a row is an avatar, the name and quiet markers; an ordinary name fits", async ({ page }) => {
    await openRoomWithComposer(page);
    // A fixed test member with an ordinary-length name.
    await callRiverTest(page, "insertMessageBeforeLast", "a member to list");
    const row = memberRows(page).filter({ hasText: "Test Sender Insert" });
    await expect(row).toHaveCount(1);
    const m = await row.evaluate((el) => {
      const name = el.querySelector('[data-testid="member-name"]') as HTMLElement;
      const avatar = el.querySelector(".msg-avatar")!;
      return {
        clipped: name.scrollWidth > name.clientWidth,
        avatar: avatar.getBoundingClientRect().width,
        avatarHidden: avatar.getAttribute("aria-hidden"),
        initials: avatar.textContent,
      };
    });
    expect(m.clipped, "an ordinary name is not truncated").toBe(false);
    expect(m.avatar).toBeCloseTo(28, 0);
    expect(m.avatarHidden).toBe("true");
    expect(m.initials).toBe("TS");

    // Your own row: a muted "you", not a glyph.
    const self = page.getByTestId("member-list-self");
    await expect(self).toHaveText("you");
    await expect(self).toHaveAttribute("title", "You");
    // The owner's crown is a line icon in the muted colour, labelled as before.
    const owner = page.getByTestId("member-list-owner");
    await expect(owner).toHaveAttribute("aria-label", "Room Owner");
    await expect(owner.locator("svg")).toHaveCount(1);
    const [ownerColour, muted] = await Promise.all([
      owner.locator("span").first().evaluate((el) => getComputedStyle(el).color),
      resolveColor(page, "var(--color-text-muted)"),
    ]);
    expect(await resolveColor(page, ownerColour)).toEqual(muted);
  });

  test("Invite someone is outlined, not a solid accent fill", async ({ page }) => {
    await openRoomWithComposer(page);
    const invite = page.getByTestId("invite-member-button");
    await expect(invite).toHaveText(/Invite someone/);
    const [bg, accent] = await Promise.all([
      invite.evaluate((el) => getComputedStyle(el).backgroundColor),
      resolveColor(page, "var(--color-accent)"),
    ]);
    expect(await resolveColor(page, bg)).not.toEqual(accent);
    expect((await resolveColor(page, bg))[3], "transparent at rest").toBe(0);
    expect(parseFloat(await invite.evaluate((el) => getComputedStyle(el).borderTopWidth)), "outlined").toBeGreaterThanOrEqual(1);
    await expect(page.getByTestId("export-id-button")).toBeVisible();
  });
});

// Automatic until the reader chooses (MEMBERS_PANEL_OPEN = None): shown on a
// window at least 1024px wide, hidden on a tablet-width one (768-1023px), by
// CSS, so rotating or resizing across 1024 just works. The toggle stores the
// opposite of what is shown, which then holds at any width for the session.
// Its pressed state and title follow what is shown, across the breakpoint.
test.describe("Member panel by width", () => {
  async function open(page: import("@playwright/test").Page, width: number, height = 900) {
    await page.setViewportSize({ width, height });
    await openRoomWithComposer(page);
  }
  const panel = (page: import("@playwright/test").Page) => page.getByTestId("members-rail");
  const toggle = (page: import("@playwright/test").Page) => page.getByTestId("members-panel-toggle");
  async function expectShown(page: import("@playwright/test").Page, shown: boolean, why: string) {
    if (shown) await expect(panel(page), why).toBeVisible();
    else await expect(panel(page), why).toBeHidden();
    await expect(toggle(page), `${why}: pressed`).toHaveAttribute("aria-pressed", shown ? "true" : "false");
    await expect(toggle(page), `${why}: title`).toHaveAttribute("title", shown ? "Hide members" : "Show members");
  }

  test("an iPad in portrait (820px) loads it hidden, and the toggle shows it", async ({ page }) => {
    await open(page, 820, 1180);
    await expectShown(page, false, "auto at 820");
    await expect(page.getByTestId("rooms-rail")).toBeVisible();
    await toggle(page).click();
    await expectShown(page, true, "after the toggle");
  });

  test("a 1280px window loads it shown", async ({ page }) => {
    await open(page, 1280);
    await expectShown(page, true, "auto at 1280");
  });

  test("automatic at 820, it shows once the window is 1180 wide (an iPad turned to landscape)", async ({ page }) => {
    await open(page, 820, 1180);
    await expectShown(page, false, "auto at 820");
    await page.setViewportSize({ width: 1180, height: 820 });
    await expectShown(page, true, "auto at 1180");
    await page.setViewportSize({ width: 820, height: 1180 });
    await expectShown(page, false, "auto back at 820");
  });

  test("closed by the reader at 1280, it stays closed at 1400 and at 900", async ({ page }) => {
    await open(page, 1280);
    await toggle(page).click();
    await expectShown(page, false, "closed at 1280");
    for (const width of [1400, 900]) {
      await page.setViewportSize({ width, height: 900 });
      await expectShown(page, false, `still closed at ${width}`);
    }
  });

  test("opened by the reader at 900, it stays open at 1400 and at 820", async ({ page }) => {
    await open(page, 900);
    await expectShown(page, false, "auto at 900");
    await toggle(page).click();
    for (const width of [1400, 820]) {
      await page.setViewportSize({ width, height: 900 });
      await expectShown(page, true, `still open at ${width}`);
    }
  });
});

test.describe("Member panel on a phone", () => {
  test.use({ viewport: { width: 390, height: 844 } });

  test("the header's members button opens the Members view, as before", async ({ page }) => {
    await openRoomWithComposer(page);
    await expect(page.getByTestId("members-panel-toggle")).toBeHidden();
    await page.getByTestId("header-members-button").click();
    await expect(page.getByTestId("members-rail")).toBeVisible();
    await expect(page.getByTestId("message-composer")).toBeHidden();
    await page.getByTestId("members-back-to-chat-button").click();
    await expect(page.getByTestId("message-composer")).toBeVisible();
  });
});
