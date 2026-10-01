import { expect, Page } from "@playwright/test";
import { callRiverTest } from "./river-test";
import { waitForApp, selectRoom } from "./example-room";

// Node-side helpers shared by conversation-autoscroll.spec.ts and
// conversation-seek-speed.spec.ts. A spec cannot import another without
// registering its tests twice, so they live here.
//
// They run on the browser's own clock. The debounce spec, whose clock is
// paused, has its own delivery and frame waits in history-scroll-fixture.ts.

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

/// A reader scroll with NO gesture event at all (conversation-autoscroll.spec.ts's
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
/// `scrollend` (its listener was installed first): whatever the settle did, it
/// did synchronously.
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
