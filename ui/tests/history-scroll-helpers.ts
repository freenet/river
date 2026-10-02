import { expect, Page } from "@playwright/test";
import { callRiverTest } from "./river-test";
import { waitForApp, selectRoom } from "./example-room";

// Node-side helpers shared by the conversation-* specs. A spec cannot import
// another without registering its tests twice, so they live here.
//
// The waits here run on the browser's own clock. The geometry reads return NaN
// when `#chat-scroll-container` is missing, so a caller comparing two reads for
// equality must first rule NaN out.

/// Matches BOTTOM_THRESHOLD_PX in ui/src/components/conversation.rs: how close
/// to the end hides the scroll-to-latest button. Button presentation only; no
/// distance from the end makes the view follow arrivals.
export const BOTTOM_THRESHOLD_PX = 100;

/// Slack for fractional layout after a scroll that did land at the bottom.
export const AT_BOTTOM_EPSILON_PX = 4;

/// scrollHeight - scrollTop - clientHeight: how far the end of the history is
/// below the visible area. 0 means the newest message is fully in view.
export function distanceFromBottom(page: Page): Promise<number> {
  return page.evaluate(() => {
    const el = document.getElementById("chat-scroll-container");
    if (!el) return Number.NaN;
    return el.scrollHeight - el.scrollTop - el.clientHeight;
  });
}

/// Total height of the rendered history, independent of where it is scrolled.
export function historyHeight(page: Page): Promise<number> {
  return page.evaluate(() => {
    const el = document.getElementById("chat-scroll-container");
    return el ? el.scrollHeight : Number.NaN;
  });
}

/// Height of the WINDOW onto the history. Shrinks when the composer grows.
export function viewportHeight(page: Page): Promise<number> {
  return page.evaluate(() => {
    const el = document.getElementById("chat-scroll-container");
    return el ? el.clientHeight : Number.NaN;
  });
}

/// The `scrollTop` that leaves the view `px` above its end.
export async function endMinus(page: Page, px: number) {
  return (await historyHeight(page)) - (await viewportHeight(page)) - px;
}

export function scrollTop(page: Page): Promise<number> {
  return page.evaluate(() => {
    const el = document.getElementById("chat-scroll-container");
    return el ? el.scrollTop : Number.NaN;
  });
}

export async function expectSettledAtBottom(page: Page, why: string) {
  await expect
    .poll(() => distanceFromBottom(page), {
      timeout: 5_000,
      message: why,
    })
    .toBeLessThanOrEqual(AT_BOTTOM_EPSILON_PX);
}

/// Deliver an inbound message, exactly as an arriving network update does, and
/// wait until it is actually on the page.
///
/// Waiting on the text rather than on a fixed delay matters for the tests that
/// assert the view did NOT move: a timeout that expires before the render lands
/// passes whether or not the bug is present, and `retries: 2` would keep that
/// invisible.
export async function deliver(page: Page, text: string) {
  await callRiverTest(page, "appendMessage", text);
  await expect(page.getByText(text, { exact: false }).last()).toBeVisible({
    timeout: 5_000,
  });
}

/// Add enough history to have somewhere to scroll back through: `count` plain
/// messages (alternating authors, so one row each; no reactions or replies),
/// each on the page before the next is sent. Arrivals do not move the view, so
/// the reader then scrolls to the end themselves (`readerScrollsToEnd`). On the
/// running clock.
export async function fillHistory(page: Page, count = 8) {
  for (let i = 0; i < count; i++) {
    await deliver(page, `filler ${i}: ${"y".repeat(200)}`);
  }
  await readerScrollsToEnd(page);
}

/// The reader scrolls to the very end themselves (no button), and the scroll
/// has landed there. A no-op if the view is already at the end.
export async function readerScrollsToEnd(page: Page) {
  if ((await distanceFromBottom(page)) > AT_BOTTOM_EPSILON_PX) {
    await readerScrollsWithoutGesture(page, await historyHeight(page));
  }
  await expectSettledAtBottom(page, "premise: the reader's scroll should land at the end");
}

/// Open a room and wait until the history has settled at its newest message.
///
/// `path` lets a test opt into fixture variants the default build hides —
/// the windowed-history tests load `/?deep-history-room=1` to get a room
/// deeper than the render window without changing what every other spec sees.
export async function openRoomAtBottom(page: Page, roomName: string, path = "/") {
  await page.goto(path);
  await waitForApp(page);
  await selectRoom(page, roomName);
  // Mobile projects run at the desktop viewport, so the chat panel is visible;
  // asserted since a hidden panel would make every geometry read below return 0.
  await expect(page.locator("#chat-scroll-container")).toBeVisible({ timeout: 5_000 });
  await expectSettledAtBottom(page, "opening a room should land on its newest message");
}

/// Fill Team Chat Room with tall rows and park the reader `px` above the end.
export async function parkAboveTheEnd(page: Page, px: number) {
  await openRoomAtBottom(page, "Team Chat Room");
  await page.evaluate(async () => {
    for (let i = 0; i < 30; i++) {
      window.__riverTest!.appendMessage(`park filler ${i}: ${"w ".repeat(450)}`);
      await new Promise((r) => requestAnimationFrame(() => requestAnimationFrame(r)));
    }
  });
  await expect(page.getByText("park filler 29:")).toBeAttached({ timeout: 5_000 });
  await readerScrollsWithoutGesture(page, await endMinus(page, px));
  await afterLayoutSettles(page);
  expect(await distanceFromBottom(page), "premise: the reader should be parked far up").toBeGreaterThan(
    px - BOTTOM_THRESHOLD_PX,
  );
  await expect(page.getByTestId("scroll-to-bottom")).toBeVisible({ timeout: 5_000 });
}

/// A reader scroll with NO gesture event at all (conversation-history-position.spec.ts's
/// `readerScrollsTo` dispatches a synthetic `wheel` first).
///
/// Not a contrivance: a native scrollbar drag dispatches no pointer event to
/// the content on Firefox, and find-in-page, focus-driven scrolling and browser
/// scroll restoration produce none either. Unlike `readerScrollsTo`, this waits
/// for `scrollend`, so the scroll has landed before the test acts. The pending
/// scroll case is covered on purpose by the batched at-cap test, which does not
/// wait.
///
/// Resolves with `scrollTop` as it is right after the app has handled the
/// `scrollend` (its listener was installed first).
export function readerScrollsWithoutGesture(page: Page, top: number): Promise<number> {
  return page.evaluate(
    (t) =>
      new Promise<number>((resolve, reject) => {
        const el = document.getElementById("chat-scroll-container")!;
        const timer = setTimeout(() => reject(new Error("the scroll never settled")), 5_000);
        el.addEventListener(
          "scrollend",
          () => {
            clearTimeout(timer);
            resolve(el.scrollTop);
          },
          { once: true },
        );
        el.scrollTop = t;
      }),
    top,
  );
}

/// Wait out the layout a test just provoked.
export async function afterLayoutSettles(page: Page) {
  // Past the frame that carries the scroll event and the ResizeObserver.
  await page.evaluate(
    () =>
      new Promise<void>((resolve) =>
        requestAnimationFrame(() => requestAnimationFrame(() => setTimeout(resolve, 300))),
      ),
  );
}
