import { test, expect, Page } from "@playwright/test";
import { callRiverTest } from "./river-test";
import {
  newestVisibleRow,
  registerHistoryGeometry,
  savedRowDrift,
  type RowPosition,
} from "./history-scroll-geometry";
import {
  AT_BOTTOM_EPSILON_PX,
  afterLayoutSettles,
  deliver,
  distanceFromBottom,
  expectSettledAtBottom,
  parkAboveTheEnd,
  scrollTop,
  viewportHeight,
} from "./history-scroll-helpers";

// "Scroll to latest messages" is ONE native smooth scroll to the end measured at
// the click (ui/src/components/conversation/history_scroll.rs). The browser owns
// its duration and easing; these tests run on the browser's real clock and make
// no claim about either. What they hold it to:
//
//   * one `scrollTo({behavior: "smooth"})` per click, never re-aimed, never
//     followed by a corrective jump to an end that moved on;
//   * where it lands (or where the reader's input, a hide, or a second click
//     stops it) is preserved through later arrivals like any other position;
//   * a hide cancels it for good: the reveal restores, it does not resume.
//
// Arrivals are INBOUND (`window.__riverTest`, example-data build).

/// How far above the end the reader parks before the click.
const PARK_PX = 3_000;
/// A row's gap may move this much and still be "in place" (whole-pixel reads,
/// fractional rows), far below a row's height.
const IN_PLACE_TOLERANCE_PX = 4;

const button = (page: Page) => page.getByTestId("scroll-to-bottom");

type ScrollRequests = { smooth: number; other: number };

declare global {
  interface Window {
    __riverScrollRequests?: ScrollRequests;
  }
}

/// Count `scrollTo`/`scroll` calls on the history from here on, by behavior.
/// `scrollTop` writes (anchor corrections, cancelling) are not counted: only a
/// smooth request animates.
async function recordScrollRequests(page: Page) {
  await page.evaluate(() => {
    const record: ScrollRequests = { smooth: 0, other: 0 };
    window.__riverScrollRequests = record;
    const container = document.getElementById("chat-scroll-container")!;
    for (const name of ["scrollTo", "scroll"] as const) {
      const original = container[name].bind(container) as (...args: unknown[]) => void;
      (container as unknown as Record<string, unknown>)[name] = (...args: unknown[]) => {
        const options = args[0];
        const smooth =
          typeof options === "object" && options !== null && (options as ScrollToOptions).behavior === "smooth";
        if (smooth) record.smooth++;
        else record.other++;
        original(...args);
      };
    }
  });
}

function scrollRequests(page: Page): Promise<ScrollRequests> {
  return page.evaluate(() => window.__riverScrollRequests!);
}

/// The live end, `scrollHeight - clientHeight`.
function maxScrollTop(page: Page): Promise<number> {
  return page.evaluate(() => {
    const el = document.getElementById("chat-scroll-container")!;
    return el.scrollHeight - el.clientHeight;
  });
}

/// Park well above the end with the button showing, then start recording.
async function parkAndRecord(page: Page) {
  await parkAboveTheEnd(page, PARK_PX);
  await recordScrollRequests(page);
  return { parkedAt: await scrollTop(page), destination: await maxScrollTop(page) };
}

/// Wait until the native animation has visibly moved the view.
async function animationUnderway(page: Page, from: number) {
  await expect
    .poll(() => scrollTop(page), { timeout: 5_000, message: "premise: the native animation should start" })
    .toBeGreaterThan(from + 40);
}

/// The view has stopped moving: two reads 300ms apart agree.
async function viewAtRest(page: Page, why: string): Promise<number> {
  let last = Number.NaN;
  await expect
    .poll(
      async () => {
        const before = await scrollTop(page);
        await page.waitForTimeout(300);
        last = await scrollTop(page);
        return Math.abs(last - before);
      },
      { timeout: 10_000, message: why },
    )
    .toBeLessThanOrEqual(1);
  return last;
}

/// The row recorded in `before` is still at its gap, over five samples (500ms).
async function expectRowHolds(page: Page, before: RowPosition, why: string) {
  await expect.poll(() => savedRowDrift(page, before), { timeout: 5_000, message: why }).toBeLessThanOrEqual(
    IN_PLACE_TOLERANCE_PX,
  );
  const drifts: number[] = [];
  for (let i = 0; i < 5; i++) {
    await page.waitForTimeout(100);
    drifts.push(await savedRowDrift(page, before));
  }
  expect(Math.max(...drifts), `${why} (samples: ${drifts.join(", ")})`).toBeLessThanOrEqual(IN_PLACE_TOLERANCE_PX);
}

const ARRIVAL = (what: string) => `${what}: ${"v".repeat(200)}`;

test.beforeEach(async ({ page }) => {
  await registerHistoryGeometry(page);
});

test.describe("Scroll to latest is one native smooth scroll", () => {
  test.use({ viewport: { width: 1280, height: 900 } });

  test("with no arrivals it reaches the end, and the landing is preserved through the next arrival", async ({
    page,
  }) => {
    await parkAndRecord(page);
    await button(page).click();
    await expectSettledAtBottom(page, "the native animation did not reach the end");
    await viewAtRest(page, "the view should come to rest at the end");
    expect(await scrollRequests(page), "one smooth request for one click").toEqual({ smooth: 1, other: 0 });

    const landed = await newestVisibleRow(page);
    expect(landed, "premise: a message should be visible").not.toBeNull();
    await deliver(page, ARRIVAL("arrival after the landing"));
    await expectRowHolds(page, landed!, "an arrival after the landing moved the view");
    expect(await distanceFromBottom(page), "the arrival should be below the view, not followed").toBeGreaterThan(
      AT_BOTTOM_EPSILON_PX,
    );
  });

  test("arrivals during the animation do not retarget it or snap it to the new end", async ({ page }) => {
    const { parkedAt, destination } = await parkAndRecord(page);
    await button(page).click();
    await animationUnderway(page, parkedAt);
    for (let i = 0; i < 3; i++) {
      await callRiverTest(page, "appendMessage", ARRIVAL(`arrival ${i} mid-flight`));
    }
    await expect(page.getByText("arrival 2 mid-flight")).toBeAttached({ timeout: 5_000 });

    const rest = await viewAtRest(page, "the view should come to rest");
    expect(await scrollRequests(page), "the click's request is the only one").toEqual({ smooth: 1, other: 0 });
    expect(Math.abs(rest - destination), "it should land at the end measured at the click").toBeLessThanOrEqual(
      AT_BOTTOM_EPSILON_PX,
    );
    expect(await distanceFromBottom(page), "the newer arrivals stay below the view").toBeGreaterThan(200);
    await expect(button(page), "the button stays offered for the newer messages").toBeVisible();

    // Nothing comes back for it later: no completion snap, no quiet-interval jump.
    const landed = await newestVisibleRow(page);
    await page.waitForTimeout(1_500);
    await expectRowHolds(page, landed!, "the landing moved after the animation ended");
    await deliver(page, ARRIVAL("arrival after the landing"));
    await expectRowHolds(page, landed!, "an arrival after the landing moved the view");
  });

  test("a second click asks for the end as it is then", async ({ page }) => {
    const { parkedAt } = await parkAndRecord(page);
    await button(page).click();
    await animationUnderway(page, parkedAt);
    await callRiverTest(page, "appendMessage", ARRIVAL("arrival before the second click"));
    await expect(page.getByText("arrival before the second click")).toBeAttached({ timeout: 5_000 });
    await viewAtRest(page, "the first navigation should come to rest");
    expect(await distanceFromBottom(page), "premise: the arrival is below the first landing").toBeGreaterThan(
      AT_BOTTOM_EPSILON_PX,
    );

    await button(page).click();
    await expectSettledAtBottom(page, "the second click did not reach the end as it was then");
    await viewAtRest(page, "the second navigation should come to rest");
    expect(await scrollRequests(page), "one smooth request per click").toEqual({ smooth: 2, other: 0 });
  });

  test("a click replacing one still in flight wins, and nothing left from the first moves the view", async ({
    page,
  }) => {
    const { parkedAt } = await parkAndRecord(page);
    await button(page).click();
    await animationUnderway(page, parkedAt);
    await callRiverTest(page, "appendMessage", ARRIVAL("arrival between the clicks"));
    await expect(page.getByText("arrival between the clicks")).toBeAttached({ timeout: 5_000 });
    await button(page).click();
    await expectSettledAtBottom(page, "the replacing click did not reach the end as it was then");
    await viewAtRest(page, "the view should come to rest");
    const landed = await newestVisibleRow(page);
    await page.waitForTimeout(1_500);
    await expectRowHolds(page, landed!, "something left over from the first click moved the view");
  });

  test("the reader's input stops it where it is, and that position is preserved", async ({ page }) => {
    const { parkedAt } = await parkAndRecord(page);
    await button(page).click();
    await animationUnderway(page, parkedAt);
    await page.evaluate(() =>
      document
        .getElementById("chat-scroll-container")!
        .dispatchEvent(new WheelEvent("wheel", { bubbles: true, deltaY: -1 })),
    );
    const rest = await viewAtRest(page, "the view should stop after the reader's input");
    expect(await distanceFromBottom(page), "the reader's input should have stopped it short").toBeGreaterThan(200);
    expect(rest).toBeGreaterThan(parkedAt);

    const stopped = await newestVisibleRow(page);
    expect(stopped, "premise: a message should be visible").not.toBeNull();
    await page.waitForTimeout(1_500);
    await deliver(page, ARRIVAL("arrival after the interruption"));
    await expectRowHolds(page, stopped!, "the interrupted position was not preserved");
  });

  test("with reduced motion it goes straight to the end", async ({ page }) => {
    await page.emulateMedia({ reducedMotion: "reduce" });
    await parkAndRecord(page);
    await button(page).click();
    expect(await distanceFromBottom(page), "reduced motion should jump, not animate").toBeLessThanOrEqual(
      AT_BOTTOM_EPSILON_PX,
    );
    expect((await scrollRequests(page)).smooth, "no animation was requested").toBe(0);
  });
});

test.describe("Hiding the chat cancels the animation", () => {
  test.use({ viewport: { width: 390, height: 844 } });

  const chat = (page: Page) => page.locator("#chat-scroll-container");

  async function chatHidden(page: Page) {
    await expect(chat(page)).toBeHidden({ timeout: 5_000 });
    await expect.poll(() => viewportHeight(page), { message: "premise: the hidden chat has no height" }).toBe(0);
    await afterLayoutSettles(page);
  }

  for (const hide of [
    { by: "the rooms button", opener: "hamburger-rooms-button", back: "rooms-back-button" },
    { by: "the members button", opener: "header-members-button", back: "members-back-button" },
  ] as const) {
    test(`hidden mid-flight by ${hide.by}, with arrivals while hidden, it does not resume on reveal`, async ({
      page,
    }) => {
      const { parkedAt } = await parkAndRecord(page);
      await button(page).click();
      await animationUnderway(page, parkedAt);
      await page.getByTestId(hide.opener).filter({ visible: true }).click();
      await chatHidden(page);
      for (let i = 0; i < 3; i++) {
        const text = ARRIVAL(`hidden arrival ${i}`);
        await callRiverTest(page, "appendMessage", text);
        await expect(page.getByText(text)).toBeAttached({ timeout: 5_000 });
      }
      // Longer than any quiet interval the navigation could have left behind.
      await page.waitForTimeout(1_500);

      await page.getByTestId(hide.back).click();
      await expect(chat(page)).toBeVisible();
      await afterLayoutSettles(page);
      const revealed = await newestVisibleRow(page);
      expect(revealed, "premise: a message should be visible").not.toBeNull();
      expect(await distanceFromBottom(page), "the reveal should not have finished the animation").toBeGreaterThan(
        200,
      );
      await expectRowHolds(page, revealed!, "the animation resumed, or something moved the view, after the reveal");
      expect((await scrollRequests(page)).smooth, "nothing re-issued the animation").toBe(1);
    });
  }
});
