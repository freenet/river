import { test, expect, Page } from "@playwright/test";
import { callRiverTest } from "./river-test";
import { newestVisibleRow, registerHistoryGeometry } from "./history-scroll-geometry";
import {
  endReflowResult,
  endReflowUnhide,
  registerEndReflow,
  type EndReflow,
  type EndReflowKind,
} from "./history-event-order-fixture";
import {
  ARRIVAL,
  AT_BOTTOM_EPSILON_PX,
  IN_PLACE_TOLERANCE_PX,
  afterLayoutSettles,
  animationUnderway,
  deliver,
  distanceFromBottom,
  expectVisibleRowHolds,
  expectSettledAtBottom,
  maxScrollTop,
  parkAboveTheEnd,
  scrollTop,
  viewAtRest,
} from "./history-scroll-helpers";

// "Scroll to latest messages" where the browser sends no `scrollend` (Safari
// before 17.4). The navigation is the same one native smooth scroll
// (conversation-native-scroll.spec.ts); what differs is how it ends: `install`
// asks `Reflect.has(container, "onscrollend")`, and without it the navigation
// is over once `scroll` has been quiet for NAVIGATION_QUIET_MS (250ms,
// history_scroll.rs), not at a `scrollend` at the destination. Every engine
// here has `scrollend`, so an init script takes it away before the app starts,
// and each test checks that the app saw it gone.
//
// On the browser's real clock: the animation is the browser's, and the quiet
// interval is a real timer. No claim about either's duration.

/// How far above the end the reader parks before the click.
const PARK_PX = 3_000;
/// The quiet interval without `scrollend` (NAVIGATION_QUIET_MS), and the
/// backstop with it (NAVIGATION_QUIET_WITH_SCROLLEND_MS), which must not be used.
const QUIET_MS = 250;
const BACKSTOP_MS = 1_000;

/// One quiet-interval registration the app made, on `performance.now()`.
type QuietTimer = { delay: number; at: number; fired: number | null; cleared: number | null };

type Fallback = {
  /// Event types added to `#chat-scroll-container`, in order.
  listeners: string[];
  /// `setTimeout`s of QUIET_MS or BACKSTOP_MS made while a click, or a `scroll`
  /// on the container, was being dispatched: the navigation's quiet interval
  /// is armed from exactly those.
  quiet: QuietTimer[];
  /// `scrollTo`/`scroll` calls on the container, by behavior.
  requests: { smooth: number; other: number };
  /// Run once, in the task of the next quiet interval to fire, just before the
  /// app's own callback: a test's layout change at the moment the navigation
  /// ends.
  beforeQuiet?: () => void;
};

declare global {
  interface Window {
    __riverFallback?: Fallback;
  }
}

/// Runs in the page before the app (`addInitScript`), so it is self-contained.
function withoutScrollend({ quietMs, backstopMs }: { quietMs: number; backstopMs: number }) {
  const CONTAINER_ID = "chat-scroll-container";
  const record: Fallback = {
    listeners: [],
    quiet: [],
    requests: { smooth: 0, other: 0 },
  };
  window.__riverFallback = record;
  for (const proto of [HTMLElement.prototype, Element.prototype]) {
    delete (proto as { onscrollend?: unknown }).onscrollend;
  }

  const isContainer = (t: unknown) => t instanceof Element && t.id === CONTAINER_ID;
  const add = EventTarget.prototype.addEventListener;
  EventTarget.prototype.addEventListener = function (this: EventTarget, type: string, ...rest: unknown[]) {
    if (isContainer(this)) record.listeners.push(String(type));
    return (add as (...a: unknown[]) => void).call(this, type, ...rest);
  } as typeof EventTarget.prototype.addEventListener;

  for (const name of ["scrollTo", "scroll"] as const) {
    const original = Element.prototype[name] as (...a: unknown[]) => void;
    (Element.prototype as unknown as Record<string, unknown>)[name] = function (this: Element, ...args: unknown[]) {
      if (isContainer(this)) {
        const options = args[0];
        const smooth =
          typeof options === "object" && options !== null && (options as ScrollToOptions).behavior === "smooth";
        if (smooth) record.requests.smooth++;
        else record.requests.other++;
      }
      return original.apply(this, args);
    };
  }

  const set = window.setTimeout;
  const clear = window.clearTimeout;
  const timers = new Map<number, QuietTimer>();
  window.setTimeout = function (cb: unknown, delay?: number, ...args: unknown[]) {
    const ev = (window as { event?: Event }).event;
    const navigating = !!ev && (ev.type === "click" || (ev.type === "scroll" && isContainer(ev.currentTarget)));
    if (!navigating || typeof cb !== "function" || (delay !== quietMs && delay !== backstopMs)) {
      return (set as (...a: unknown[]) => number)(cb, delay, ...args);
    }
    const timer: QuietTimer = { delay, at: performance.now(), fired: null, cleared: null };
    record.quiet.push(timer);
    const handle = (set as (...a: unknown[]) => number)(
      function (this: unknown, ...a: unknown[]) {
        timer.fired = performance.now();
        timers.delete(handle);
        const before = record.beforeQuiet;
        record.beforeQuiet = undefined;
        before?.();
        return (cb as (...a: unknown[]) => unknown).apply(this, a);
      },
      delay,
      ...args,
    );
    timers.set(handle, timer);
    return handle;
  } as typeof window.setTimeout;
  window.clearTimeout = function (handle?: number) {
    const timer = handle === undefined ? undefined : timers.get(handle);
    if (timer) {
      timer.cleared = performance.now();
      timers.delete(handle!);
    }
    return clear(handle);
  } as typeof window.clearTimeout;
}

function fallback(page: Page): Promise<Fallback> {
  return page.evaluate(() => JSON.parse(JSON.stringify(window.__riverFallback!)));
}

/// The app found no `onscrollend` and listens for everything else.
async function expectFallbackSelected(page: Page) {
  const state = await fallback(page);
  expect(
    await page.evaluate(() => Reflect.has(document.getElementById("chat-scroll-container")!, "onscrollend")),
    "premise: the container has no onscrollend",
  ).toBe(false);
  // `install` adds its listeners first and in one go, ending with `keydown`;
  // later ones are the tests' own (`readerScrollsWithoutGesture` waits on a
  // `scrollend`).
  const installed = state.listeners.slice(0, state.listeners.indexOf("keydown") + 1);
  expect(installed, `premise: the app installed its listeners (${state.listeners})`).toEqual(
    expect.arrayContaining(["scroll", "wheel", "touchstart", "pointerdown", "keydown"]),
  );
  expect(installed, "premise: the app added no scrollend listener").not.toContain("scrollend");
}

/// Park well above the end with the button showing, and count requests from here.
async function parkAndCount(page: Page) {
  await parkAboveTheEnd(page, PARK_PX);
  await expectFallbackSelected(page);
  await page.evaluate(() => {
    const record = window.__riverFallback!;
    record.requests = { smooth: 0, other: 0 };
    record.quiet = [];
  });
  return {
    parkedAt: await scrollTop(page),
    destination: await maxScrollTop(page),
  };
}

/// The navigation ended at its quiet interval: the last one armed has fired,
/// none is left pending, and no backstop-length interval was ever armed.
async function expectEndedByQuietInterval(page: Page) {
  await expect
    .poll(async () => (await fallback(page)).quiet.filter((t) => t.fired === null && t.cleared === null).length, {
      timeout: 5_000,
      message: "premise: a quiet interval is still pending after the view came to rest",
    })
    .toBe(0);
  const { quiet } = await fallback(page);
  const what = JSON.stringify(quiet);
  expect(quiet.length, `the navigation armed no quiet interval (${what})`).toBeGreaterThan(0);
  expect(
    quiet.filter((t) => t.delay !== QUIET_MS),
    `the navigation used the backstop rather than the no-scrollend interval (${what})`,
  ).toEqual([]);
  expect(quiet.at(-1)!.fired, `the last quiet interval did not fire (${what})`).not.toBeNull();
}

const button = (page: Page) => page.getByTestId("scroll-to-bottom");

test.beforeEach(async ({ page }) => {
  await registerHistoryGeometry(page);
  await registerEndReflow(page);
  await page.addInitScript(withoutScrollend, { quietMs: QUIET_MS, backstopMs: BACKSTOP_MS });
});

test.describe("Without scrollend, scroll to latest ends after a quiet interval", () => {
  test.use({ viewport: { width: 1280, height: 900 } });

  test("it reaches the end, and once over, the landing row is preserved through a resize and an arrival", async ({
    page,
  }) => {
    await parkAndCount(page);
    await button(page).click();
    await expectSettledAtBottom(page, "the native animation did not reach the end");
    await viewAtRest(page, "the view should come to rest at the end");
    expect((await fallback(page)).requests, "one smooth request for one click").toEqual({ smooth: 1, other: 0 });
    await expectEndedByQuietInterval(page);

    // Only a navigation that has ended restores the saved row: while one runs,
    // a container resize leaves the view to the animation. Shrinking the
    // container moves the row's gap unless it is restored.
    const landed = await newestVisibleRow(page);
    expect(landed, "premise: a message should be visible").not.toBeNull();
    try {
      await page.evaluate(() => {
        const c = document.getElementById("chat-scroll-container")!;
        c.style.maxHeight = `${c.clientHeight - 120}px`;
      });
      await afterLayoutSettles(page);
      await expectVisibleRowHolds(page, landed!, "a resize after the landing moved the row: the navigation never ended");
    } finally {
      await page.evaluate(() => document.getElementById("chat-scroll-container")?.style.removeProperty("max-height"));
    }
    await afterLayoutSettles(page);
    await expectVisibleRowHolds(page, landed!, "removing the resize moved the landing row");

    await deliver(page, ARRIVAL("arrival after the landing"));
    await expectVisibleRowHolds(page, landed!, "an arrival after the landing moved the view");
    expect(await distanceFromBottom(page), "the arrival should be below the view, not followed").toBeGreaterThan(
      AT_BOTTOM_EPSILON_PX,
    );
  });

  test("arrivals during the animation do not retarget it, and it lands at the end measured at the click", async ({
    page,
  }) => {
    const { parkedAt, destination } = await parkAndCount(page);
    await button(page).click();
    await animationUnderway(page, parkedAt);
    for (let i = 0; i < 3; i++) {
      await callRiverTest(page, "appendMessage", ARRIVAL(`arrival ${i} mid-flight`));
    }
    await expect(page.getByText("arrival 2 mid-flight")).toBeAttached({ timeout: 5_000 });

    const rest = await viewAtRest(page, "the view should come to rest");
    expect((await fallback(page)).requests, "the click's request is the only one").toEqual({ smooth: 1, other: 0 });
    expect(Math.abs(rest - destination), "it should land at the end measured at the click").toBeLessThanOrEqual(
      AT_BOTTOM_EPSILON_PX,
    );
    expect(await distanceFromBottom(page), "the newer arrivals stay below the view").toBeGreaterThan(200);
    await expectEndedByQuietInterval(page);

    // Nothing comes back for it later: the quiet interval is not a jump.
    const landed = await newestVisibleRow(page);
    expect(landed, "premise: a message should be visible").not.toBeNull();
    await page.waitForTimeout(1_000);
    await expectVisibleRowHolds(page, landed!, "the landing moved after the quiet interval");
    await deliver(page, ARRIVAL("arrival after the landing"));
    await expectVisibleRowHolds(page, landed!, "an arrival after the landing moved the view");
  });
});

test.describe("Without scrollend, the quiet interval handles a reflow before it captures", () => {
  test.use({ viewport: { width: 1280, height: 900 } });

  /// The growth above the view when the quiet interval fires.
  const GROW_PX = 200;

  /// Park, click, and apply `kind` in the task of the quiet interval that ends
  /// the navigation, before the app's callback and before any observer can
  /// report it. With `arrival`, a message lands below the view mid-flight, so
  /// the end at the click is no longer the latest.
  async function reflowAtQuietEnd(page: Page, kind: EndReflowKind, { arrival = false } = {}) {
    const { parkedAt, destination } = await parkAndCount(page);
    await page.evaluate(
      ({ kind, px }) => {
        window.__riverFallback!.beforeQuiet = () => window.__historyEndReflow!.run(kind, px);
      },
      { kind, px: GROW_PX },
    );
    await button(page).click();
    if (arrival) await callRiverTest(page, "appendMessage", ARRIVAL("arrival mid-flight"));
    await animationUnderway(page, parkedAt);
    await expect
      .poll(() => endReflowResult(page), { timeout: 10_000, message: "premise: the quiet interval fired" })
      .not.toBeNull();
    const run = (await endReflowResult(page)) as EndReflow;
    const what = `${JSON.stringify(run)}; destination ${destination}; ${JSON.stringify((await fallback(page)).quiet)}`;
    expect(
      Math.abs(run.top - destination),
      `premise: the animation had reached its destination when the interval fired (${what})`,
    ).toBeLessThanOrEqual(AT_BOTTOM_EPSILON_PX);
    expect(run.anchor, `premise: a message was visible at the end (${what})`).not.toBeNull();
    await expectEndedByQuietInterval(page);
    return { run, what };
  }

  test("growth above the anchor as the interval fires keeps the anchor's gap", async ({ page }) => {
    const { run, what } = await reflowAtQuietEnd(page, "grow");
    expect(
      Math.abs(run.shift - GROW_PX),
      `premise: the growth pushed the anchor down before the app saw it (${what})`,
    ).toBeLessThanOrEqual(IN_PLACE_TOLERANCE_PX);
    await afterLayoutSettles(page);
    await expectVisibleRowHolds(page, run.anchor!, `the quiet end captured the displaced view (${what})`);
    await deliver(page, ARRIVAL("arrival after the corrected end"));
    await expectVisibleRowHolds(page, run.anchor!, "an arrival after the corrected end moved the view");
    expect((await fallback(page)).requests, "nothing re-issued the animation").toEqual({ smooth: 1, other: 0 });
  });

  test("an anchor missing as the interval fires, with rows still rendered, lands at the latest message", async ({
    page,
  }) => {
    try {
      const { run, what } = await reflowAtQuietEnd(page, "hide", { arrival: true });
      expect(run.max - run.top, `premise: the arrival put the latest message below the end (${what})`).toBeGreaterThan(
        AT_BOTTOM_EPSILON_PX,
      );
      await expectSettledAtBottom(page, `a missing anchor at the quiet end should land at the latest message (${what})`);
      await viewAtRest(page, "the view should come to rest at the latest message");
      const landed = await newestVisibleRow(page);
      expect(landed, "premise: a message is visible at the landing").not.toBeNull();
      await endReflowUnhide(page);
      await deliver(page, ARRIVAL("arrival after the landing"));
      await expectVisibleRowHolds(page, landed!, "an arrival after the landing moved the view");
      expect(await distanceFromBottom(page), "the arrival was followed").toBeGreaterThan(AT_BOTTOM_EPSILON_PX);
      expect((await fallback(page)).requests, "the landing is not another animation").toEqual({ smooth: 1, other: 0 });
    } finally {
      await endReflowUnhide(page);
    }
  });
});
