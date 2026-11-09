import { test, expect, Locator, Page } from "@playwright/test";
import {
  coarsePointer,
  committedRow,
  inlineFooter,
  nextFrames,
  openInlinePicker,
  openOwnMessageEdit,
  openRoomWithComposer,
  reactTo,
  revealFooter,
  settle,
  submitMessage,
} from "./example-room";
import { expectAtReact, measurePicker } from "./reaction-picker-geometry";
import { callRiverTest } from "./river-test";

// On a desktop-width window with a mouse, a message with no reactions keeps no
// footer line: its controls show only on hover, so a reserved line left every
// message double-spaced at rest. They sit at the end of the message's last
// line instead, and an invisible spacer ending the text makes a last line that
// would run under them wrap (main.css, "Controls at the end of the last
// line"). A row with reactions keeps its footer line, and so do narrow windows
// and touch pointers.

type Box = { left: number; right: number; top: number; bottom: number };

/// Where `row`'s controls and text are, and how its message ends. Text is
/// every non-blank text node in the row's content outside the footer, one box
/// per line it occupies.
function rowLayout(row: Locator) {
  return row.evaluate((el) => {
    const box = (r: DOMRect) => ({ left: r.left, right: r.right, top: r.top, bottom: r.bottom });
    const content = el.querySelector('[data-testid="message-content"]')!;
    const footer = el.querySelector('[data-testid="message-footer"]') as HTMLElement;
    const body = el.querySelector('[data-testid="message-body"]')!;
    const textLines = (root: Node) => {
      const out: { text: string; box: ReturnType<typeof box> }[] = [];
      const walker = document.createTreeWalker(root, NodeFilter.SHOW_TEXT);
      for (let t = walker.nextNode(); t; t = walker.nextNode()) {
        if (!t.textContent?.trim() || footer.contains(t)) continue;
        const range = document.createRange();
        range.selectNodeContents(t);
        for (const r of Array.from(range.getClientRects())) {
          if (r.width > 0 && r.height > 0) out.push({ text: t.textContent.trim().slice(0, 40), box: box(r) });
        }
      }
      return out;
    };
    // How the message ends, and the inline spacer, wherever the CSS put it.
    const prose = body.querySelector(":scope > .prose");
    const last = prose?.lastElementChild ?? null;
    let ending = "other";
    if (body.lastElementChild !== prose) ending = "edited";
    else if (!last) ending = "empty";
    else if (/^(P|H[1-6])$/.test(last.tagName)) ending = "paragraph";
    else if (/^(UL|OL)$/.test(last.tagName)) ending = "list";
    else if (last.tagName === "BLOCKQUOTE") ending = "quote";
    else if (last.tagName === "PRE") ending = "code";
    const spacers = [prose, ...Array.from(prose?.querySelectorAll("*") ?? [])]
      .filter((e): e is Element => !!e)
      .map((e) => getComputedStyle(e, "::after"))
      .filter((s) => s.content !== "none" && s.content !== "normal" && s.display === "inline-block");
    const after = spacers.at(-1) ?? null;
    // Where the CSS promises an inline spacer: a last paragraph or heading, a
    // last list's last item holding its text (itself, or in a last
    // paragraph), a last quote's last paragraph or heading.
    const textual = (e: Element | null | undefined) => !!e && /^(P|H[1-6])$/.test(e.tagName);
    const lastItem = last && /^(UL|OL)$/.test(last.tagName) ? last.querySelector(":scope > li:last-child") : null;
    const expectsSpacer =
      textual(last) ||
      (!!lastItem && (textual(lastItem.lastElementChild) || !lastItem.querySelector(":scope > :is(p, ul, ol, pre, blockquote, table, hr)"))) ||
      (last?.tagName === "BLOCKQUOTE" && textual(last.lastElementChild));
    // The code block the message ends with, at the top level or in a last quote.
    const lastPre =
      last?.tagName === "PRE" ? last : last?.tagName === "BLOCKQUOTE" && last.lastElementChild?.tagName === "PRE" ? last.lastElementChild : null;
    const controls = Array.from(footer.querySelectorAll("button")).map((b) => ({
      id: b.getAttribute("data-testid") ?? "?",
      box: box(b.getBoundingClientRect()),
    }));
    const f = footer.getBoundingClientRect();
    const hit = document.elementFromPoint(f.left + f.width / 2, f.top + f.height / 2);
    const bodyText = textLines(body);
    return {
      footerPosition: getComputedStyle(footer).position,
      footer: box(f),
      footerTakesHit: !!hit && footer.contains(hit),
      controls,
      controlsSpan: controls.length ? controls[controls.length - 1].box.right - controls[0].box.left : 0,
      lines: textLines(content),
      lastTextBottom: bodyText.length ? Math.max(...bodyText.map((l) => l.box.bottom)) : null,
      row: box(el.getBoundingClientRect()),
      content: box(content.getBoundingClientRect()),
      body: box(body.getBoundingClientRect()),
      // The last code block, if the message ends in one (at the top level or
      // in a last quote).
      pre: lastPre ? box(lastPre.getBoundingClientRect()) : null,
      spacerCount: spacers.length,
      expectsSpacer: body.lastElementChild === prose && expectsSpacer,
      ending,
      spacer: after ? parseFloat(after.width) : null,
      text: (body.textContent ?? "").trim().replace(/\s+/g, " ").slice(0, 40),
    };
  });
}

/// A control covers a line when they overlap across, and by more than 4px
/// down: a 28px control on a 22px line reaches 3px into the lines either side.
function covers(control: Box, line: Box) {
  const across = Math.min(control.right, line.right) - Math.max(control.left, line.left);
  const down = Math.min(control.bottom, line.bottom) - Math.max(control.top, line.top);
  return across > 0.5 && down > 4;
}

const WORDS = "one two six ten red tan big low far new old cup".split(" ");

/// A room with every kind of reaction-less row: received messages whose last
/// line grows a short word at a time across a whole line (so some would run
/// under the controls without the spacer), your own messages likewise, and
/// messages ending in a heading, a list, a quote, a rule, code, a long URL,
/// nothing, an edit and a reply.
async function openRoomWithEndings(page: Page) {
  await openRoomWithComposer(page);
  const base = "A received message whose last line lands somewhere along the column, a short word at a time:";
  for (let i = 0; i < 32; i++) {
    await callRiverTest(page, "appendMessage", `${base} ${Array.from({ length: i }, (_, j) => WORDS[j % WORDS.length]).join(" ")} end${i}.`);
  }
  await callRiverTest(page, "appendMessage", "Short.");
  for (const text of [
    "Ends in a heading:\n\n## A heading long enough to run most of the way across the column, to the controls",
    "Ends in a list:\n\n- first item\n- a last item long enough to run to the column's edge, where the controls sit on hover",
    "Ends in a loose list:\n\n- first item\n\n- a last item long enough to run to the column's edge, where the controls sit on hover",
    "Ends in a nested list:\n\n- first item\n  - a nested last item long enough to run to the column's edge, where the controls sit",
    "Ends in a quote:\n\n> a quoted last line long enough to run to the column's edge, where the controls sit on hover",
    "Ends in quoted code:\n\n> quoted\n>\n> ```\n> let quoted_code_runs_past_the_column = and_scrolls_sideways(rather_than_wrapping, under_controls);\n> ```",
    "Ends in a quoted list:\n\n> - a quoted last item long enough to run to the column's edge, where the controls sit on hover",
    "Ends in a rule:\n\n---",
    "Ends in code:\n\n```\nlet the_last_line_of_code_runs_past_the_column = and_scrolls_sideways(rather_than_wrapping, under_the_controls_or_not);\n```",
    `Ends in a long link: https://example.com/${"long".repeat(60)}`,
    "[an unused link reference]: https://example.com/",
    "Last of the endings.",
  ]) {
    await callRiverTest(page, "appendMessage", text);
  }
  await expect(committedRow(page, "Last of the endings.")).toHaveCount(1);
  const own = "Your own message, with four controls to keep clear of:";
  for (let i = 0; i < 6; i++) {
    await expect(await submitMessage(page, `${own} ${Array.from({ length: 6 * i }, (_, j) => WORDS[j % WORDS.length]).join(" ")} own${i}.`)).toHaveCount(1);
  }
  const edited = await submitMessage(page, "To be edited, with a long last line that runs most of the way to the column's edge, wrapping.");
  await expect(edited).toHaveCount(1);
  const editArea = await openOwnMessageEdit(page, edited);
  await editArea.fill("Edited, with a long last line that runs most of the way to the column's edge, wrapping onto a second.");
  await editArea.press("Enter");
  await expect(committedRow(page, "Edited, with a long last line")).toContainText("(edited)");
  const first = committedRow(page, "own0.");
  await revealFooter(page, first);
  await first.getByTestId("message-reply-button").click();
  await expect(await submitMessage(page, "A reply, with a quote above its text.")).toHaveCount(1);
  await page.mouse.move(0, 0);
  await settle(page);
}

test.describe("Controls at the end of the last line on a desktop with a mouse", () => {
  test.beforeEach(async ({ page }) => {
    await page.setViewportSize({ width: 1280, height: 900 });
    test.skip(!(await inlineFooter(page)), "touch pointers keep the footer line");
  });

  test("a row with no reactions keeps no footer line, wastes no line, and its controls cover no text", async ({ page, browserName }) => {
    test.setTimeout(180_000);
    await openRoomWithEndings(page);
    const short = await rowLayout(committedRow(page, "Short."));
    expect(short.body.bottom - short.body.top, "a one-line message stays one line").toBeLessThan(23);

    // Page the whole history in first. Scrolling near the top otherwise pages
    // older messages in above a row while it is hovered, moving it out from
    // under the pointer mid-check.
    let height = -1;
    for (let tries = 0; tries < 20; tries++) {
      const now = await page.locator("#chat-scroll-container").evaluate((c) => {
        c.scrollTop = 0;
        return c.scrollHeight;
      });
      if (now === height) break;
      height = now;
      await settle(page);
    }
    // By id, not position, so a row is the same row throughout its check.
    const ids = await page
      .getByTestId("conversation-history")
      .getByTestId("message-row")
      .and(page.locator('[data-has-reactions="false"]'))
      .evaluateAll((els) => els.map((el) => el.id));
    const endings = new Map<string, number>();
    let paragraphs = 0;
    let wrapped = 0;
    for (const id of ids) {
      const row = page.locator(`[id="${id}"]`);
      await row.evaluate((el) => el.scrollIntoView({ block: "center" }));
      // Off every row, inside the page.
      await page.mouse.move(40, 450);
      // Near the top, older history may page in and move the rows.
      await settle(page);
      // On Firefox, a row the history cannot scroll away from its very top is
      // left out: there, under parallel load, Firefox intermittently matches
      // no `:hover` at all (not even on <html>) with the pointer on the row,
      // though the row is what `elementFromPoint` finds and nothing in the
      // history re-renders. Chromium and WebKit check it. Every kind of ending
      // is among the rows below it.
      const pinnedAtTop =
        browserName === "firefox" &&
        (await row.evaluate((el) => {
          const c = document.getElementById("chat-scroll-container")!;
          return c.scrollTop === 0 && el.getBoundingClientRect().top - c.getBoundingClientRect().top < 100;
        }));
      if (pinnedAtTop) continue;
      const rest = await rowLayout(row);
      const what = `"${rest.text}" (${rest.ending})`;
      endings.set(rest.ending, (endings.get(rest.ending) ?? 0) + 1);
      expect.soft(rest.footerPosition, `${what}: the footer takes no line`).toBe("absolute");
      expect.soft(rest.footerTakesHit, `${what}: at rest the footer takes no hits`).toBe(false);
      // Under 12px (a footer line was 28px); a code block keeps 26px below it
      // for the controls, clear of its scrollbar.
      expect.soft(rest.row.bottom - rest.body.bottom, `${what}: nothing reserved below the message`).toBeLessThan(rest.pre ? 12 + 26 : 12);
      if (rest.ending === "paragraph") {
        paragraphs++;
        if (rest.lastTextBottom !== null && rest.body.bottom - rest.lastTextBottom > 15) wrapped++;
      }

      await revealFooter(page, row);
      await expect(row.getByTestId("message-reply-button"), `${what}: shown on hover`).toHaveCSS("opacity", "1");
      const shown = await rowLayout(row);
      expect.soft(shown.row, `${what}: showing the controls moves nothing`).toEqual(rest.row);
      expect.soft(shown.spacerCount, `${what}: at most one spacer`).toBeLessThanOrEqual(1);
      if (shown.expectsSpacer) expect.soft(shown.spacer, `${what}: a spacer ends the text`).not.toBeNull();
      if (shown.spacer !== null) expect.soft(shown.spacer, `${what}: the spacer is as wide as the controls`).toBeGreaterThanOrEqual(shown.controlsSpan);
      for (const c of shown.controls) {
        for (const l of shown.lines) expect.soft(covers(c.box, l.box), `${what}: ${c.id} covers "${l.text}"`).toBe(false);
        expect.soft(Math.abs(c.box.bottom - shown.row.bottom), `${what}: ${c.id} at the row's foot`).toBeLessThan(1);
        expect.soft(c.box.right, `${what}: ${c.id} inside the content column`).toBeLessThanOrEqual(shown.content.right + 0.5);
        if (shown.pre) expect.soft(c.box.top, `${what}: ${c.id} below the code block and its scrollbar`).toBeGreaterThanOrEqual(shown.pre.bottom - 0.5);
      }
    }
    for (const kind of ["paragraph", "list", "quote", "code", "edited", "empty", "other"]) {
      expect(endings.get(kind) ?? 0, `premise: a message ending in ${kind} was checked (${JSON.stringify([...endings])})`).toBeGreaterThan(0);
    }
    expect(wrapped, "premise: some last lines would have run under the controls, and wrapped").toBeGreaterThan(0);
    expect(wrapped, `only those: ${wrapped} of ${paragraphs} paragraphs wrapped`).toBeLessThan(paragraphs / 2);
  });

  test("a row with reactions keeps its footer line with the controls on it, and goes back when they go", async ({ page }) => {
    await openRoomWithComposer(page);
    const row = await submitMessage(page, "a message that gets a reaction");
    await expect(row).toHaveCount(1);
    await page.mouse.move(0, 0);
    await settle(page);
    const before = await rowLayout(row);
    expect(before.footerPosition, "premise: no footer line before the reaction").toBe("absolute");
    expect(before.spacer, "premise: a spacer before the reaction").not.toBeNull();

    const chip = await reactTo(page, row);
    await expect(row).toHaveAttribute("data-has-reactions", "true");
    await page.mouse.move(0, 0);
    await settle(page);
    await revealFooter(page, row);
    const after = await rowLayout(row);
    expect(after.footerPosition).toBe("static");
    expect(after.spacer, "no spacer once the footer has its line").toBeNull();
    expect(after.footer.top, "the footer is under the message").toBeGreaterThanOrEqual(after.body.bottom - 0.5);
    expect(after.footer.bottom - after.footer.top).toBeGreaterThanOrEqual(26);
    for (const c of after.controls) {
      expect(c.box.top >= after.footer.top - 0.5 && c.box.bottom <= after.footer.bottom + 0.5, `${c.id} on the footer line`).toBe(true);
    }

    // Taking the reaction back returns the row to its first layout.
    await chip.click();
    await expect(row).toHaveAttribute("data-has-reactions", "false");
    await page.mouse.move(0, 0);
    await settle(page);
    const back = await rowLayout(row);
    expect(back.footerPosition).toBe("absolute");
    expect(back.row.bottom - back.row.top).toBeCloseTo(before.row.bottom - before.row.top, 0);
  });

  test("the first reaction moves React down to the footer line, and a picker opened upward goes with it", async ({ page }) => {
    await openRoomWithComposer(page);
    const row = await submitMessage(page, "first reaction moves React");
    await expect(row).toHaveCount(1);
    await expect(row).toBeInViewport();
    await openInlinePicker(page, row);
    await expect(page.getByTestId("emoji-picker")).toBeVisible();
    // React's place in its row: the history may scroll as the row grows.
    const inRow = (m: Awaited<ReturnType<typeof measurePicker>>) => row.evaluate((el, top) => top - el.getBoundingClientRect().top, m.react.top);
    const before = await measurePicker(row);
    expect(before.picker.bottom, "premise: it opened upward").toBeLessThanOrEqual(before.react.top + 0.5);
    expectAtReact(before);
    const beforeInRow = await inRow(before);
    await callRiverTest(page, "addReactionsToNewest", ["🎉"]);
    await expect(row.getByTestId("reaction-chip")).toHaveCount(1);
    await nextFrames(page);
    const after = await measurePicker(row);
    expect((await inRow(after)) - beforeInRow, "premise: React moved down to the footer line").toBeGreaterThan(20);
    await expect(page.getByTestId("emoji-picker"), "still open").toBeVisible();
    expectAtReact(after);
    expect(after.picker.bottom, "still upward").toBeLessThanOrEqual(after.react.top + 0.5);
    await page.keyboard.press("Escape");
  });
});

test.describe("Controls keep the footer line on a narrow window", () => {
  test("below 48rem a row with no reactions keeps its footer line", async ({ page }) => {
    await page.setViewportSize({ width: 760, height: 900 });
    test.skip(await coarsePointer(page), "touch pointers keep the footer line anyway");
    await openRoomWithComposer(page);
    const row = await submitMessage(page, "narrow window footer");
    await expect(row).toHaveCount(1);
    await expect(row).toHaveAttribute("data-has-reactions", "false");
    const layout = await rowLayout(row);
    expect(layout.footerPosition).toBe("static");
    expect(layout.footer.bottom - layout.footer.top).toBeGreaterThanOrEqual(26);
    expect(layout.footer.top, "the footer is under the message").toBeGreaterThanOrEqual(layout.body.bottom - 0.5);
  });
});

test.describe("Controls keep the footer line with a touch pointer on a wide window", () => {
  test.use({ viewport: { width: 1024, height: 768 }, hasTouch: true });

  test("a touch laptop or tablet keeps the footer line, with the controls shown at rest", async ({ page }) => {
    await openRoomWithComposer(page);
    const wide = await page.evaluate(() => window.matchMedia("(min-width: 48rem)").matches);
    test.skip(!wide || !(await coarsePointer(page)), "this engine does not report a touch pointer here");
    const row = await submitMessage(page, "touch laptop footer");
    await expect(row).toHaveCount(1);
    await page.mouse.move(0, 0);
    await settle(page);
    const layout = await rowLayout(row);
    expect(layout.footerPosition).toBe("static");
    expect(layout.spacer).toBeNull();
    expect(layout.footer.top, "the footer is under the message").toBeGreaterThanOrEqual(layout.body.bottom - 0.5);
    await expect(row.getByTestId("message-reply-button")).toHaveCSS("opacity", "1");
  });
});
