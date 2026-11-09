import { test, expect, Locator, Page } from "@playwright/test";
import {
  coarsePointer,
  committedRow,
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

/// Where `row`'s controls and text are. Text is every non-blank text node in
/// the row's content outside the footer, one box per line it occupies.
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
    const prose = body.querySelector(".prose");
    return {
      footerPosition: getComputedStyle(footer).position,
      controls: Array.from(footer.querySelectorAll("button")).map((b) => ({
        id: b.getAttribute("data-testid") ?? "?",
        box: box(b.getBoundingClientRect()),
      })),
      lines: textLines(content),
      bodyLines: textLines(body),
      row: box(el.getBoundingClientRect()),
      content: box(content.getBoundingClientRect()),
      body: box(body.getBoundingClientRect()),
      endsWithParagraph: prose?.lastElementChild?.tagName === "P" && body.lastElementChild === prose,
      text: (body.textContent ?? "").trim().replace(/\s+/g, " ").slice(0, 40),
    };
  });
}

function overlapsText(control: Box, line: Box) {
  const middle = (line.top + line.bottom) / 2;
  return line.right > control.left + 0.5 && line.left < control.right - 0.5 && middle > control.top && middle < control.bottom;
}

async function desktopMouse(page: Page) {
  const wide = await page.evaluate(() => window.matchMedia("(min-width: 48rem)").matches);
  return wide && !(await coarsePointer(page));
}

const WORDS = "one two six ten red tan big low far new old cup".split(" ");

/// A room with every kind of reaction-less row: received messages whose last
/// line grows a short word at a time across a whole line (so some would run
/// under the controls without the spacer), your own long messages, and
/// messages ending in a code block, a list, a quote, a reply and an edit.
async function openRoomWithEndings(page: Page) {
  await openRoomWithComposer(page);
  const base = "A received message whose last line lands somewhere along the column, a short word at a time:";
  for (let i = 0; i < 32; i++) {
    await callRiverTest(page, "appendMessage", `${base} ${Array.from({ length: i }, (_, j) => WORDS[j % WORDS.length]).join(" ")} end${i}.`);
  }
  await expect(committedRow(page, "end31.")).toHaveCount(1);
  const own = "Your own message, with four controls to keep clear of:";
  for (let i = 0; i < 12; i++) {
    const row = await submitMessage(page, `${own} ${Array.from({ length: 3 * i }, (_, j) => WORDS[j % WORDS.length]).join(" ")} own${i}.`);
    await expect(row).toHaveCount(1);
  }
  // Markdown renders differently from its source, so each is found by its
  // first line.
  for (const [marker, text] of [
    ["Ends in code:", "Ends in code:\n\n```\nlet the_last_line_of_code = run_under_the_controls(if_it_could, wrap_or_scroll_sideways);\n```"],
    ["Ends in a list:", "Ends in a list:\n\n- first item\n- a last item long enough to run to the column's edge, where the controls sit on hover"],
    ["Ends in a quote:", "Ends in a quote:\n\n> a quoted last line long enough to run to the column's edge, where the controls sit on hover"],
  ]) {
    const input = page.getByTestId("message-input");
    await input.fill(text);
    await input.press("Enter");
    await expect(committedRow(page, marker)).toHaveCount(1);
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
    test.skip(!(await desktopMouse(page)), "touch pointers keep the footer line");
  });

  test("a row with no reactions keeps no footer line, and its controls sit on its last line and cover no text", async ({ page }) => {
    test.setTimeout(180_000);
    await openRoomWithEndings(page);
    const rows = page.getByTestId("conversation-history").getByTestId("message-row").and(page.locator('[data-has-reactions="false"]'));
    const count = await rows.count();
    let wrapped = 0;
    let checked = 0;
    for (let i = 0; i < count; i++) {
      const row = rows.nth(i);
      await row.evaluate((el) => el.scrollIntoView({ block: "center" }));
      await page.mouse.move(0, 0);
      await nextFrames(page);
      const rest = await rowLayout(row);
      const what = `"${rest.text}"`;
      expect(rest.footerPosition, `${what}: the footer takes no line`).toBe("absolute");
      // Under 12px: a footer line would be 28px. (A list's last item keeps its
      // own bottom margin, which collapses out of the body, as before.)
      expect(rest.row.bottom - rest.body.bottom, `${what}: nothing reserved below the message`).toBeLessThan(12);
      // A paragraph whose spacer wrapped ends with an empty line.
      const lastText = Math.max(...rest.bodyLines.map((l) => l.box.bottom));
      if (rest.endsWithParagraph && rest.body.bottom - lastText > 15) wrapped++;

      await revealFooter(page, row);
      await expect(row.getByTestId("message-reply-button")).toHaveCSS("opacity", "1");
      const shown = await rowLayout(row);
      expect(shown.row, `${what}: showing the controls moves nothing`).toEqual(rest.row);
      for (const c of shown.controls) {
        for (const l of shown.lines) {
          expect(overlapsText(c.box, l.box), `${what}: ${c.id} covers "${l.text}"`).toBe(false);
        }
        // A message ending in a list, quote or code block keeps that block's
        // trailing margin, which puts the controls a little lower, in the
        // empty line its spacer made.
        const middle = (c.box.top + c.box.bottom) / 2;
        const tolerance = rest.endsWithParagraph || rest.text.includes("(edited)") ? 4 : 10;
        expect(Math.abs(middle - (shown.body.bottom - 11)), `${what}: ${c.id} on the last line`).toBeLessThan(tolerance);
        expect(c.box.right, `${what}: ${c.id} inside the content column`).toBeLessThanOrEqual(shown.content.right + 0.5);
        expect(c.box.bottom, `${what}: ${c.id} inside its row`).toBeLessThanOrEqual(shown.row.bottom + 0.5);
      }
      checked++;
    }
    expect(checked, "premise: the rows were checked").toBeGreaterThan(40);
    expect(wrapped, "premise: some last lines would have run under the controls, and wrapped").toBeGreaterThan(0);
  });

  test("a row with reactions keeps its footer line, with the controls on it", async ({ page }) => {
    await openRoomWithComposer(page);
    const row = await submitMessage(page, "a message that gets a reaction");
    await expect(row).toHaveCount(1);
    expect((await rowLayout(row)).footerPosition, "premise: no footer line before the reaction").toBe("absolute");
    await reactTo(page, row);
    await expect(row).toHaveAttribute("data-has-reactions", "true");
    await page.mouse.move(0, 0);
    await settle(page);
    const after = await rowLayout(row);
    expect(after.footerPosition).toBe("static");
    const footer = (await row.getByTestId("message-footer").boundingBox())!;
    expect(footer.y, "the footer is under the message").toBeGreaterThanOrEqual(after.body.bottom - 0.5);
    expect(footer.height).toBeGreaterThanOrEqual(26);
  });

  test("the first reaction moves React down to the footer line, and an open picker goes with it", async ({ page }) => {
    await openRoomWithComposer(page);
    const row = await submitMessage(page, "first reaction moves React");
    await expect(row).toHaveCount(1);
    await row.evaluate((el) => el.scrollIntoView({ block: "center" }));
    await openInlinePicker(page, row);
    await expect(page.getByTestId("emoji-picker")).toBeVisible();
    // React's place in its row: the history may scroll as the row grows.
    const inRow = (m: Awaited<ReturnType<typeof measurePicker>>) => row.evaluate((el, top) => top - el.getBoundingClientRect().top, m.react.top);
    const before = await measurePicker(row);
    expectAtReact(before);
    const beforeInRow = await inRow(before);
    await callRiverTest(page, "addReactionsToNewest", ["🎉"]);
    await expect(row.getByTestId("reaction-chip")).toHaveCount(1);
    await nextFrames(page);
    const after = await measurePicker(row);
    expect((await inRow(after)) - beforeInRow, "premise: React moved down to the footer line").toBeGreaterThan(20);
    await expect(page.getByTestId("emoji-picker"), "still open").toBeVisible();
    expectAtReact(after);
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
    const footer = (await row.getByTestId("message-footer").boundingBox())!;
    expect(footer.height).toBeGreaterThanOrEqual(26);
    expect(footer.y, "the footer is under the message").toBeGreaterThanOrEqual(layout.body.bottom - 0.5);
  });
});
