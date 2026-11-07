import { test, expect, Locator, Page } from "@playwright/test";
import { clickFooter, coarsePointer, openRoomWithComposer, settle, submitMessage } from "./example-room";
import { callRiverTest } from "./river-test";

// A message's time shows in two places: beside the author's name on a group's
// first row, always visible, and in a continuation row's gutter, under a hover
// pointer only. Each describes ITS message: its own time, its own clamp mark
// (`~`, when the sender's clock ran ahead and the time shown is the arrival)
// and its own delay or clamp explanation in the full-date title. A later
// message's clamp must not mark the header, and a pending delete of the first
// message moves the header to the message now first.

type Entry = { text: string; sentSecs: number; receivedSecs: number };

/// What one message's time should say, from its sender time and arrival: the
/// instant it shows, whether it carries the clamp mark, and the explanation
/// its full-date title ends with. The date formatting itself is util.rs's.
type Expected = { datetime: string; clamped: boolean; note: string };

const CLAMP_NOTE =
  "(sender's clock may be ahead of yours — their timestamp was later than when this message reached you, so the time shown is when it arrived)";

function expectedFor([sentMs, receivedMs]: [number, number]): Expected {
  // CLOCK_SKEW_TOLERANCE_SECS
  const clamped = sentMs > receivedMs + 60_000;
  // Every delay these specs deliver is under a minute (`format_delay`), and
  // none is negative.
  const delay = Math.trunc((receivedMs - sentMs) / 1000);
  return {
    datetime: new Date(clamped ? receivedMs : sentMs).toISOString(),
    clamped,
    note: clamped ? CLAMP_NOTE : `(received after ${delay}s delay)`,
  };
}

/// The semantic time element as it renders.
function timeOf(el: Locator) {
  return el.evaluate((e) => ({
    tag: e.tagName.toLowerCase(),
    datetime: e.getAttribute("datetime"),
    shown: e.textContent,
    title: e.getAttribute("title"),
  }));
}

function expectTime(actual: Awaited<ReturnType<typeof timeOf>>, want: Expected, what: string) {
  expect(actual.tag, `${what}: a time element`).toBe("time");
  expect(actual.datetime, `${what}: its instant`).toBe(want.datetime);
  expect(actual.shown, `${what}: the clamp mark`).toMatch(want.clamped ? /^~\d{1,2}:\d{2}$/ : /^\d{1,2}:\d{2}$/);
  expect(actual.title!.endsWith(` ${want.note}`), `${what}: its explanation, in "${actual.title}"`).toBe(true);
  expect(actual.title!.includes(CLAMP_NOTE), `${what}: the clamp note only when clamped`).toBe(want.clamped);
}

function groupWith(page: Page, text: string): Locator {
  return page.locator("[data-item-key]").filter({ hasText: text });
}

/// Deliver a same-author run and return its group and each message's expected labels.
async function deliverRun(page: Page, author: 0 | 1, entries: Entry[]) {
  const times = await callRiverTest(page, "deliverTimedMessages", author, entries);
  const group = groupWith(page, entries[0].text);
  await expect(group.getByTestId("message-row"), "the run is one group").toHaveCount(entries.length);
  for (const [i, e] of entries.entries()) await expect(group.getByTestId("message-row").nth(i)).toContainText(e.text);
  const expected = times.map(expectedFor);
  return { group, expected };
}

/// An accurate first message with a later clamped one, and the reverse.
async function deliverMixedRuns(page: Page) {
  const accurateFirst = await deliverRun(page, 0, [
    { text: "accurate first, 2s in transit", sentSecs: -120, receivedSecs: -118 },
    { text: "clamped later, an hour ahead", sentSecs: 3600, receivedSecs: -60 },
  ]);
  const clampedFirst = await deliverRun(page, 1, [
    { text: "clamped first, an hour ahead", sentSecs: 3600, receivedSecs: -50 },
    { text: "accurate later, 7s in transit", sentSecs: -20, receivedSecs: -13 },
  ]);
  for (const run of [accurateFirst, clampedFirst]) {
    expect(run.expected.map((e) => e.clamped), "premise: one clamped message per run").toContain(true);
    expect(run.expected.map((e) => e.clamped), "premise: one accurate message per run").toContain(false);
  }
  return { accurateFirst, clampedFirst };
}

test.describe("Message timestamps", () => {
  test("the header time describes the first message, beside the name, without hover", async ({ page }) => {
    await openRoomWithComposer(page);
    const { accurateFirst, clampedFirst } = await deliverMixedRuns(page);
    await page.mouse.move(0, 0);

    for (const [what, run] of [
      ["accurate first, clamped later", accurateFirst],
      ["clamped first, accurate later", clampedFirst],
    ] as const) {
      const first = run.group.getByTestId("message-row").first();
      await first.scrollIntoViewIfNeeded();
      const header = first.getByTestId("message-group-header");
      const time = header.getByTestId("message-header-time");
      await expect(time, `${what}: one header time`).toHaveCount(1);
      await expect(run.group.getByTestId("message-header-time"), `${what}: only on the first row`).toHaveCount(1);
      expectTime(await timeOf(time), run.expected[0], `${what}: the header time is the first message's`);
      await expect(time).toBeVisible();
      // Shown at rest, with no hover (`toBeVisible` passes at opacity 0).
      const opacity = Number(await time.evaluate((e) => getComputedStyle(e).opacity));
      expect(opacity, `${what}: shown at rest`).toBeGreaterThan(0);

      // Beside the name: on the name's line, after it.
      const [name, box] = [await header.getByTestId("message-author-name").boundingBox(), await time.boundingBox()];
      expect(name && box, "premise: both laid out").toBeTruthy();
      expect(box!.x, `${what}: after the name`).toBeGreaterThanOrEqual(name!.x + name!.width);
      expect(box!.y, `${what}: on the name's line`).toBeLessThan(name!.y + name!.height);
      expect(box!.y + box!.height, `${what}: on the name's line`).toBeGreaterThan(name!.y);
    }
  });

  test("a continuation row's gutter time is its own message's, revealed as before", async ({ page }) => {
    await openRoomWithComposer(page);
    const { accurateFirst, clampedFirst } = await deliverMixedRuns(page);
    const coarse = await coarsePointer(page);

    for (const [what, run] of [
      ["a clamped continuation", accurateFirst],
      ["an accurate continuation", clampedFirst],
    ] as const) {
      const second = run.group.getByTestId("message-row").nth(1);
      await second.scrollIntoViewIfNeeded();
      const time = second.getByTestId("message-gutter-time");
      await expect(time, `${what}: one gutter time`).toHaveCount(1);
      await expect(run.group.getByTestId("message-gutter-time"), `${what}: none on the first row`).toHaveCount(1);
      expectTime(await timeOf(time), run.expected[1], `${what}: its own time, mark and title`);
      // Where it always was: the row's avatar gutter, left of the content.
      expect(await time.evaluate((e) => e.parentElement?.classList.contains("msg-gutter")), `${what}: in the gutter`).toBe(true);
      const [box, content] = [await time.boundingBox(), await second.getByTestId("message-content").boundingBox()];
      if (coarse) {
        // Unchanged on touch: no gutter time at all.
        expect(box, `${what}: not laid out on a touch pointer`).toBeNull();
        continue;
      }
      expect(box!.x + box!.width, `${what}: left of the content`).toBeLessThanOrEqual(content!.x + 0.5);
      const opacity = () =>
        time.evaluate(async (el) => {
          await Promise.all(el.getAnimations().map((a) => a.finished));
          return getComputedStyle(el).opacity;
        });
      await page.mouse.move(0, 0);
      expect(await opacity(), `${what}: hidden at rest`).toBe("0");
      await second.getByTestId("message-body").hover();
      await expect.poll(opacity, { message: `${what}: shown under the pointer` }).toBe("1");
    }
  });

  test("a pending delete of the first message moves the header to the next, and its failure moves it back", { tag: "@chromium-only" }, async ({ page }) => {
    await openRoomWithComposer(page);
    for (const text of ["header pair one", "header pair two"]) await expect(await submitMessage(page, text)).toHaveCount(1);
    const group = groupWith(page, "header pair two");
    const rows = group.getByTestId("message-row");
    const one = rows.filter({ hasText: "header pair one" });
    const two = rows.filter({ hasText: "header pair two" });
    await expect(rows.first(), "premise: the pair's first message starts its group").toContainText("header pair one");
    await settle(page);
    const header = () => group.getByTestId("message-header-time");
    const original = await timeOf(header());
    const own = (row: Locator) => row.evaluate((e) => e.querySelector("time")!.getAttribute("datetime"));
    expect(await own(one), "premise: the header is the first message's").toBe(original.datetime);
    const second = await own(two);
    expect(second, "premise: the two differ").not.toBe(original.datetime);

    await callRiverTest(page, "holdMessageSigning");
    await one.scrollIntoViewIfNeeded();
    await clickFooter(page, one, "message-delete-button");
    await page.getByRole("button", { name: "Delete", exact: true }).click();
    await expect(one, "the delete shows while its signature is held").toHaveCount(0);
    await expect(header(), "the header moves to the message now first").toHaveAttribute("datetime", second!);
    await expect(two.getByTestId("message-gutter-time")).toHaveCount(0);

    await callRiverTest(page, "failNextMessageSign");
    await callRiverTest(page, "releaseMessageSigning");
    await expect(one, "the failed delete is rolled back").toHaveCount(1);
    expect(await timeOf(header()), "the header is the first message's again").toEqual(original);
    expect(await own(two)).toBe(second);
  });
});
