import { test, expect, Page } from "@playwright/test";
import { callRiverTest } from "./river-test";
import { waitForApp, selectRoom, setTabVisibility } from "./example-room";
import {
  DM_VIEW,
  NEWEST_IN_VIEW_SLACK_PX,
  newestBelowView,
  scrollMetrics,
  settle,
} from "./history-geometry";
import {
  DM_PEER,
  DmPeer,
  HISTORY,
  LATEST,
  OTHER_DM_PEER,
  closeThread,
  deliverDm,
  expectAtEnd,
  expectStill,
  newestHistoryDm,
  openThread,
  railBadge,
  recordPageErrors,
  recordScrolls,
  scrollThreadTo,
} from "./dm-thread";

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
// nothing else can mark it read before the release. Hook-driven, so it runs on
// one engine.

const A = DM_PEER;
const B = OTHER_DM_PEER;

/// Team Chat Room with `HISTORY` inbound DMs from each of `peers`, every one
/// of them unread.
async function seedThreads(page: Page, ...peers: DmPeer[]) {
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

/// Open `peer`'s seeded thread and wait for its newest history DM.
function openSeeded(page: Page, peer: DmPeer) {
  return openThread(page, peer, newestHistoryDm(peer));
}

/// The rail's unread count for `peer`, read once (0 when no badge shows).
async function unreadCount(page: Page, peer: DmPeer): Promise<number> {
  const badge = railBadge(page, peer);
  return (await badge.count()) === 0 ? 0 : Number(await badge.textContent());
}

/// Park the open thread well above its end, with its newest DM off screen.
async function parkAboveEnd(page: Page) {
  await scrollThreadTo(page, Math.round((await scrollMetrics(page, DM_VIEW)).max / 2));
  await settle(page);
  expect(await newestBelowView(page, DM_VIEW), "premise: the newest DM is off screen").toBeGreaterThan(
    NEWEST_IN_VIEW_SLACK_PX,
  );
  await expect(page.getByTestId(LATEST)).toBeVisible();
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
  await openSeeded(page, B);
  await expectAtEnd(page, "premise: B's own opening placement landed");
  await setTabVisibility(page, "visible");
  await expect(railBadge(page, B), "premise: B's opening at its end marked it read").toHaveCount(0, {
    timeout: 5_000,
  });
  await parkAboveEnd(page);
  await deliverDm(page, "unread for B", B);
  await settle(page);
  await expect(railBadge(page, B), "premise: B's DM below the fold is unread").toHaveText("1");
}

test.describe("DM thread placement after unmount", { tag: "@chromium-only" }, () => {
  test.afterEach(async ({ page }) => {
    await setTabVisibility(page, "visible").catch(() => {});
  });

  test("a queued opening of a closed thread neither moves nor marks read the thread that replaced it", async ({
    page,
  }) => {
    const errors = recordPageErrors(page);
    await seedThreads(page, A, B);
    await setTabVisibility(page, "hidden");
    await holdPlacementOf(page, () => openSeeded(page, A));
    await closeThread(page);

    await replaceWithParkedB(page);
    const aUnread = await unreadCount(page, A);
    expect(aUnread, "premise: A is still unread").toBeGreaterThan(0);
    await recordScrolls(page);

    await releaseHeldPlacement(page);

    await expectStill(page, "A's stale opening moved B", { soft: true, holdMs: 0 });
    expect.soft(await unreadCount(page, A), "A's stale opening marked A read from B's end").toBe(aUnread);
    expect.soft(await unreadCount(page, B), "A's stale opening marked B's DM below the fold read").toBe(1);
    expect(errors).toEqual([]);
  });

  test("a queued opening of a closed thread is ignored by a reopened instance of the same thread", async ({
    page,
  }) => {
    const errors = recordPageErrors(page);
    await seedThreads(page, A);
    await setTabVisibility(page, "hidden");
    await holdPlacementOf(page, () => openSeeded(page, A));
    await closeThread(page);

    // Same (room, peer), new instance, its own opening unheld.
    await openSeeded(page, A);
    await expectAtEnd(page, "premise: the reopened thread's own placement landed");
    await parkAboveEnd(page);
    await setTabVisibility(page, "visible");
    await settle(page);
    expect(
      await newestBelowView(page, DM_VIEW),
      "premise: the reopened thread's newest DM is off screen",
    ).toBeGreaterThan(NEWEST_IN_VIEW_SLACK_PX);
    const aUnread = await unreadCount(page, A);
    expect(aUnread, "premise: A is unread").toBe(HISTORY);
    await recordScrolls(page);

    await releaseHeldPlacement(page);

    await expectStill(page, "the closed instance's opening moved the reopened thread", { soft: true, holdMs: 0 });
    expect
      .soft(await unreadCount(page, A), "the closed instance's opening marked the reopened thread read")
      .toBe(aUnread);
    expect(errors).toEqual([]);
  });

  test("a queued placement still lands while its own thread stays open", async ({ page }) => {
    const errors = recordPageErrors(page);
    await seedThreads(page, A);
    await setTabVisibility(page, "hidden");
    await holdPlacementOf(page, () => openSeeded(page, A));
    await setTabVisibility(page, "visible");
    await settle(page);
    expect(
      await newestBelowView(page, DM_VIEW),
      "premise: the held opening left the newest DM off screen",
    ).toBeGreaterThan(NEWEST_IN_VIEW_SLACK_PX);
    expect(await unreadCount(page, A), "premise: A is unread").toBe(HISTORY);

    await releaseHeldPlacement(page);

    await expectAtEnd(page, "the released opening should land on the newest DM");
    await expect(railBadge(page, A), "the released opening reached the end and left A unread").toHaveCount(0);
    expect(errors).toEqual([]);
  });
});
