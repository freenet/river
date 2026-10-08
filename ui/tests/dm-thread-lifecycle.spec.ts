import { test, expect, Page } from "@playwright/test";
import { callRiverTest } from "./river-test";
import { waitForApp, selectRoom, setTabVisibility } from "./example-room";
import { AT_BOTTOM_EPSILON_PX, NEWEST_IN_VIEW_SLACK_PX, settle } from "./history-geometry";

// A DM thread's opening and own-send placements run in a task queued after
// the render (`safe_spawn_local`), and find the thread through the global
// `dm-scroll-container` / `dm-bottom-sentinel` ids. If the thread unmounts
// before that task runs, the placement belongs to a thread that is gone: it
// must not scroll whatever thread is open by then, and must not count that
// thread's visible end as evidence that the gone one was read.
//
// The race is made deterministic with the `holdNextDmPlacement` test hook,
// which holds the real queued placement inside its task until
// `releaseHeldDmPlacement`. The tab is hidden while a held thread is open, so
// nothing else can mark it read before the release.

/// test_hooks.rs `DM_PEERS`: index and nickname.
const A = { index: 0, name: "DM Test Peer", history: "dm history" } as const;
const B = { index: 1, name: "Other DM Peer", history: "other dm history" } as const;
type Peer = typeof A | typeof B;

/// DMs seeded per thread, enough to scroll on every test viewport.
const HISTORY = 30;
const THREAD = "#dm-scroll-container";
const LATEST = "dm-scroll-to-latest";
/// How far a view that must not move may still drift (subpixel rounding).
const STILL_PX = 1;

/// Uncaught page errors and console errors from now on: a WASM panic or a
/// RefCell borrow error surfaces as either.
function recordPageErrors(page: Page) {
  const errors: string[] = [];
  page.on("pageerror", (e) => errors.push(e.message));
  page.on("console", (msg) => {
    if (msg.type() === "error") errors.push(msg.text());
  });
  return errors;
}

/// Team Chat Room with `HISTORY` inbound DMs from each of `peers`, every one
/// of them unread.
async function seedThreads(page: Page, ...peers: Peer[]) {
  await page.goto("/");
  await waitForApp(page);
  await selectRoom(page, "Team Chat Room");
  for (const peer of peers) {
    await callRiverTest(page, "appendDmsForPeer", peer.index, HISTORY);
    await expect(railBadge(page, peer), `premise: ${peer.name}'s history is unread`).toHaveText(String(HISTORY), {
      timeout: 5_000,
    });
  }
}

function railRow(page: Page, peer: Peer) {
  return page.locator(".dm-rail-row-btn", { hasText: peer.name });
}

/// The thread's unread count in the DM rail. In the DOM even while the modal
/// covers it, and on phones while the rooms panel is shut.
function railBadge(page: Page, peer: Peer) {
  return railRow(page, peer).getByTestId("dm-rail-unread-badge");
}

/// The rail's unread count for `peer`, read once (0 when no badge shows).
async function unreadCount(page: Page, peer: Peer): Promise<number> {
  const badge = railBadge(page, peer);
  return (await badge.count()) === 0 ? 0 : Number(await badge.textContent());
}

/// Open `peer`'s thread from its rail row and wait for its newest history DM.
async function openThread(page: Page, peer: Peer) {
  const row = railRow(page, peer);
  // Phone widths keep the DM rail in the rooms panel, behind the hamburger.
  const hamburger = page.getByTestId("hamburger-rooms-button").filter({ visible: true });
  if (!(await row.isVisible()) && (await hamburger.count()) > 0) await hamburger.click();
  await row.click();
  const modal = page.getByTestId("dm-thread-modal");
  await expect(modal.locator("h2")).toContainText(peer.name, { timeout: 5_000 });
  await expect(dm(page, `${peer.history} ${HISTORY - 1}`)).toHaveCount(1, { timeout: 5_000 });
}

async function closeThread(page: Page) {
  await page.getByTestId("dm-thread-close-button").click();
  await expect(page.getByTestId("dm-thread-modal")).toHaveCount(0);
}

function dm(page: Page, text: string) {
  return page.locator(THREAD).getByText(text, { exact: true });
}

/// Deliver an inbound DM from `peer` and wait for it to render in the open
/// thread. Attached, not visible: it may land below the view.
async function deliverDm(page: Page, peer: Peer, text: string) {
  await callRiverTest(page, "deliverDmForPeer", peer.index, text);
  await expect(dm(page, text)).toHaveCount(1, { timeout: 5_000 });
}

function threadGeometry(page: Page) {
  return page.evaluate((sel) => {
    const el = document.querySelector(sel)!;
    return {
      top: el.scrollTop,
      max: el.scrollHeight - el.clientHeight,
      toEnd: el.scrollHeight - el.scrollTop - el.clientHeight,
    };
  }, THREAD);
}

/// How far the newest DM's bottom (the sentinel's top) sits below the
/// thread's visible area. Negative: above its bottom edge.
function newestBelowFold(page: Page) {
  return page.evaluate((sel) => {
    const thread = document.querySelector(sel)!;
    const sentinel = document.getElementById("dm-bottom-sentinel")!;
    return sentinel.getBoundingClientRect().top - thread.getBoundingClientRect().bottom;
  }, THREAD);
}

async function expectAtEnd(page: Page, why: string) {
  await expect
    .poll(async () => (await threadGeometry(page)).toEnd, { timeout: 5_000, message: why })
    .toBeLessThanOrEqual(AT_BOTTOM_EPSILON_PX);
}

/// The reader scrolls the thread to `top` and the move lands.
async function readerScrollsTo(page: Page, top: number) {
  await page.evaluate(
    async ({ sel, top }) => {
      const el = document.querySelector(sel)!;
      if (Math.abs(el.scrollTop - top) < 1) return;
      const landed = new Promise((resolve) =>
        el.addEventListener("scroll", () => requestAnimationFrame(resolve), { once: true }),
      );
      el.scrollTop = top;
      await landed;
    },
    { sel: THREAD, top },
  );
}

/// Park the open thread well above its end, with its newest DM off screen.
async function parkAboveEnd(page: Page) {
  await readerScrollsTo(page, Math.round((await threadGeometry(page)).max / 2));
  await settle(page);
  expect(await newestBelowFold(page), "premise: the newest DM is off screen").toBeGreaterThan(
    NEWEST_IN_VIEW_SLACK_PX,
  );
  await expect(page.getByTestId(LATEST)).toBeVisible();
}

/// Record every position the open thread passes through from now on.
async function recordScrolls(page: Page) {
  await page.evaluate((sel) => {
    const el = document.querySelector(sel)!;
    const log = { start: el.scrollTop, seen: [] as number[] };
    (window as unknown as { __dmScrolls: typeof log }).__dmScrolls = log;
    el.addEventListener("scroll", () => log.seen.push(el.scrollTop));
  }, THREAD);
}

/// The open thread never left where it was when `recordScrolls` started, not
/// even in passing. Soft, so the read assertions after it still report.
async function expectStill(page: Page, why: string) {
  const { start, seen } = await page.evaluate(
    () => (window as unknown as { __dmScrolls: { start: number; seen: number[] } }).__dmScrolls,
  );
  const { top } = await threadGeometry(page);
  const furthest = Math.max(0, ...[...seen, top].map((t) => Math.abs(t - start)));
  expect.soft(furthest, why).toBeLessThanOrEqual(STILL_PX);
}

/// Arm the hold, run `open` (which queues a placement), and wait until that
/// placement is the one held.
async function holdPlacementOf(page: Page, open: () => Promise<void>) {
  expect(await callRiverTest(page, "heldDmPlacementCount"), "premise: nothing held yet").toBe(0);
  await callRiverTest(page, "holdNextDmPlacement");
  await open();
  await expect
    .poll(() => callRiverTest(page, "heldDmPlacementCount"), { message: "the placement was never held" })
    .toBe(1);
}

/// Run the held placement, wait until it has, and let whatever it deferred
/// (a read mark, the rail's re-render) land.
async function releaseHeldPlacement(page: Page) {
  const before = await callRiverTest(page, "releasedDmPlacementsRun");
  await callRiverTest(page, "releaseHeldDmPlacement");
  await expect
    .poll(() => callRiverTest(page, "releasedDmPlacementsRun"), { message: "the released placement never ran" })
    .toBe(before + 1);
  await settle(page);
}

/// With the tab hidden and A closed: open B with its own placement unheld,
/// let it land, show the tab so B is read, then park B above its end and
/// deliver B a DM that lands below the view and stays unread.
async function replaceWithParkedB(page: Page) {
  await openThread(page, B);
  await expectAtEnd(page, "premise: B's own opening placement landed");
  await setTabVisibility(page, "visible");
  await expect(railBadge(page, B), "premise: B's opening at its end marked it read").toHaveCount(0, {
    timeout: 5_000,
  });
  await parkAboveEnd(page);
  await deliverDm(page, B, "unread for B");
  await settle(page);
  await expect(railBadge(page, B), "premise: B's DM below the fold is unread").toHaveText("1");
}

test.describe("DM thread placement after unmount", () => {
  test.afterEach(async ({ page }) => {
    await setTabVisibility(page, "visible").catch(() => {});
  });

  test("a queued opening of a closed thread neither moves nor marks read the thread that replaced it", async ({
    page,
  }) => {
    const errors = recordPageErrors(page);
    await seedThreads(page, A, B);
    await setTabVisibility(page, "hidden");
    await holdPlacementOf(page, () => openThread(page, A));
    await closeThread(page);

    await replaceWithParkedB(page);
    const aUnread = await unreadCount(page, A);
    expect(aUnread, "premise: A is still unread").toBeGreaterThan(0);
    await recordScrolls(page);

    await releaseHeldPlacement(page);

    await expectStill(page, "A's stale opening moved B");
    expect.soft(await unreadCount(page, A), "A's stale opening marked A read from B's end").toBe(aUnread);
    expect.soft(await unreadCount(page, B), "A's stale opening marked B's DM below the fold read").toBe(1);
    expect(errors).toEqual([]);
  });

  test("a queued own send of a closed thread neither moves nor marks read the thread that replaced it", async ({
    page,
  }) => {
    const errors = recordPageErrors(page);
    await seedThreads(page, A, B);
    await openThread(page, A);
    await expectAtEnd(page, "premise: A opened on its newest DM");
    await expect(railBadge(page, A), "premise: A's opening marked it read").toHaveCount(0, { timeout: 5_000 });
    await readerScrollsTo(page, 0);
    await setTabVisibility(page, "hidden");

    const composer = page.getByPlaceholder("Type a direct message...");
    await composer.fill("sent from A");
    await holdPlacementOf(page, () => composer.press("Enter"));
    await expect(dm(page, "sent from A")).toHaveCount(1, { timeout: 5_000 });
    // Stamped after the send (deliverDmForPeer orders it after the pair's
    // newest DM), so the held placement's witness has an unread DM to mark,
    // and not one from the same second as anything A has seen.
    await deliverDm(page, A, "unread for A");
    await settle(page);
    await expect(railBadge(page, A), "premise: A's DM that arrived while hidden is unread").toHaveText("1");
    await closeThread(page);

    await replaceWithParkedB(page);
    await expect(railBadge(page, A), "premise: A is still unread").toHaveText("1");
    await recordScrolls(page);

    await releaseHeldPlacement(page);

    await expectStill(page, "A's stale own-send placement moved B");
    expect.soft(await unreadCount(page, A), "A's stale own-send placement marked A read from B's end").toBe(1);
    expect.soft(await unreadCount(page, B), "A's stale own-send placement marked B's DM below the fold read").toBe(1);
    expect(errors).toEqual([]);
  });

  test("a queued opening of a closed thread is ignored by a reopened instance of the same thread", async ({
    page,
  }) => {
    const errors = recordPageErrors(page);
    await seedThreads(page, A);
    await setTabVisibility(page, "hidden");
    await holdPlacementOf(page, () => openThread(page, A));
    await closeThread(page);

    // Same (room, peer), new instance, its own opening unheld.
    await openThread(page, A);
    await expectAtEnd(page, "premise: the reopened thread's own placement landed");
    await parkAboveEnd(page);
    await setTabVisibility(page, "visible");
    await settle(page);
    expect(await newestBelowFold(page), "premise: the reopened thread's newest DM is off screen").toBeGreaterThan(
      NEWEST_IN_VIEW_SLACK_PX,
    );
    const aUnread = await unreadCount(page, A);
    expect(aUnread, "premise: A is unread").toBe(HISTORY);
    await recordScrolls(page);

    await releaseHeldPlacement(page);

    await expectStill(page, "the closed instance's opening moved the reopened thread");
    expect
      .soft(await unreadCount(page, A), "the closed instance's opening marked the reopened thread read")
      .toBe(aUnread);
    expect(errors).toEqual([]);
  });

  test("a queued opening of a thread closed with no replacement does nothing", async ({ page }) => {
    const errors = recordPageErrors(page);
    await seedThreads(page, A);
    await holdPlacementOf(page, () => openThread(page, A));
    await closeThread(page);

    await releaseHeldPlacement(page);

    await expect(page.getByTestId("dm-thread-modal")).toHaveCount(0);
    expect(await unreadCount(page, A), "the closed thread's opening marked it read").toBe(HISTORY);
    expect(errors).toEqual([]);
  });

  test("a queued placement still lands while its own thread stays open", async ({ page }) => {
    const errors = recordPageErrors(page);
    await seedThreads(page, A);
    await setTabVisibility(page, "hidden");
    await holdPlacementOf(page, () => openThread(page, A));
    await setTabVisibility(page, "visible");
    await settle(page);
    expect(await newestBelowFold(page), "premise: the held opening left the newest DM off screen").toBeGreaterThan(
      NEWEST_IN_VIEW_SLACK_PX,
    );
    expect(await unreadCount(page, A), "premise: A is unread").toBe(HISTORY);

    await releaseHeldPlacement(page);

    await expectAtEnd(page, "the released opening should land on the newest DM");
    await expect(railBadge(page, A), "the released opening reached the end and left A unread").toHaveCount(0);
    expect(errors).toEqual([]);
  });
});
