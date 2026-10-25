import { test, expect, Locator, Page } from "@playwright/test";
import { coarsePointer, openInlinePicker, openRoomWithComposer, reactTo, resolveColor, selectRoom, touchOnly, waitForApp } from "./example-room";
import { deliver, deliverOffscreen, readerLeavesTheEnd, readerScrollsWithoutGesture, scrollMetrics } from "./history-geometry";
import { callRiverTest } from "./river-test";

// Grouped chat rows: an avatar gutter, then one content column for every row.
// A group shows its avatar and header once, on its first row. On a hover
// pointer the action bar floats over a row's top right, so no gutter is kept
// for it; touch keeps a kebab in a narrow right gutter.

/// The history item (a group of one author's rows) holding `text`.
function groupWith(page: Page, text: string): Locator {
  return page.locator("[data-item-key]").filter({ hasText: text });
}

/// A received pair (one fixed test identity, inserted before the newest
/// message) and an own pair (two sends from the composer). Example data picks
/// its authors at random, so neither pair is left to chance.
async function openRoomWithPairs(page: Page): Promise<{ received: Locator; own: Locator }> {
  await openRoomWithComposer(page);
  for (const text of ["received pair one", "received pair two"]) {
    await callRiverTest(page, "insertMessageBeforeLast", text);
  }
  const input = page.getByTestId("message-input");
  for (const text of ["own pair one", "own pair two"]) {
    await input.fill(text);
    await input.press("Enter");
    await expect(page.getByTestId("conversation-history")).toContainText(text);
  }
  const received = groupWith(page, "received pair one");
  const own = groupWith(page, "own pair one");
  await expect(received).toContainText("received pair two");
  await expect(own).toContainText("own pair two");
  return { received, own };
}

const width = (b: { left: number; right: number }) => b.right - b.left;

function rowGeometry(group: Locator) {
  return group.evaluate((el) => {
    const box = (e: Element) => {
      const r = e.getBoundingClientRect();
      return { left: r.left, right: r.right, top: r.top, bottom: r.bottom };
    };
    return Array.from(el.querySelectorAll('[data-testid="message-row"]')).map((row) => ({
      self: row.getAttribute("data-message-self"),
      rowLeft: row.getBoundingClientRect().left,
      contentLeft: row.querySelector('[data-testid="message-content"]')!.getBoundingClientRect().left,
      avatars: Array.from(row.querySelectorAll('[data-testid="message-author-avatar"]')).map(box),
      headers: Array.from(row.querySelectorAll('[data-testid="message-group-header"]')).map(box),
      names: Array.from(row.querySelectorAll('[data-testid="message-author-name"]')).map(box),
      body: box(row.querySelector('[data-testid="message-body"]')!),
    }));
  });
}

test.describe("Message rows", () => {
  test("own and received groups share one row layout, with one avatar and header per group", async ({ page }) => {
    const { received, own } = await openRoomWithPairs(page);
    const groups = { received: await rowGeometry(received), own: await rowGeometry(own) };

    const bodyLefts: number[] = [];
    for (const [kind, rows] of Object.entries(groups)) {
      expect(rows, `${kind}: a two-message group`).toHaveLength(2);
      expect(rows.map((r) => r.self)).toEqual(Array(2).fill(kind === "own" ? "true" : "false"));
      // One avatar and one header, in the first row.
      expect(rows.map((r) => r.avatars.length), `${kind}: avatars per row`).toEqual([1, 0]);
      expect(rows.map((r) => r.headers.length), `${kind}: headers per row`).toEqual([1, 0]);

      const [first, second] = rows;
      // The content column starts past the avatar gutter, and the avatar sits in it.
      expect(first.contentLeft - first.rowLeft, `${kind}: the gutter`).toBeGreaterThanOrEqual(48);
      expect(first.avatars[0].left, `${kind}: avatar inside the row`).toBeGreaterThanOrEqual(first.rowLeft - 0.5);
      expect(first.avatars[0].right, `${kind}: avatar in the gutter`).toBeLessThanOrEqual(first.contentLeft + 0.5);
      // The header and every body start at the content edge.
      expect(first.headers[0].left, `${kind}: header at the content edge`).toBeCloseTo(first.contentLeft, 0);
      expect(first.body.left, `${kind}: body at the content edge`).toBeCloseTo(first.contentLeft, 0);
      expect(second.body.left, `${kind}: continuation aligned`).toBeCloseTo(first.body.left, 0);

      bodyLefts.push(...rows.map((r) => r.body.left));
    }
    // Same left edge for both authors: no right-aligned own group.
    expect(Math.max(...bodyLefts) - Math.min(...bodyLefts)).toBeLessThanOrEqual(0.5);
  });

  test("the author name is a button, by click and by Enter, and the avatar opens the same info", async ({ page }) => {
    await openRoomWithComposer(page);
    await callRiverTest(page, "insertMessageBeforeLast", "name opens info");
    const group = groupWith(page, "name opens info");
    const modal = page.getByTestId("member-info-modal");

    await group.getByRole("button", { name: "Test Sender Insert" }).click();
    await expect(modal).toContainText("Test Sender Insert");
    await page.keyboard.press("Escape");
    await expect(modal).toHaveCount(0);

    // Keyboard: focus and Enter, the native button's own activation.
    await group.getByTestId("message-author-name").focus();
    await page.keyboard.press("Enter");
    await expect(modal).toContainText("Test Sender Insert");
    await page.keyboard.press("Escape");
    await expect(modal).toHaveCount(0);

    // The avatar is a pointer shortcut only: hidden from assistive tech and no tab stop.
    const avatar = group.getByTestId("message-author-avatar");
    await expect(avatar).toHaveAttribute("aria-hidden", "true");
    expect(await avatar.evaluate((el) => (el as HTMLElement).tabIndex), "the avatar is not a tab stop").toBe(-1);
    await avatar.click();
    await expect(modal).toContainText("Test Sender Insert");
  });

  // freenet/river#791 review: without bubbles, a body could imitate a group
  // header. A body's first line sits beside the gutter of a continuation row,
  // which never holds an avatar, and it can never reach into the gutter.
  test("a body that opens like a header has no avatar and cannot reach the gutter", async ({ page }) => {
    await openRoomWithComposer(page);
    await callRiverTest(page, "insertMessageBeforeLast", "the real header");
    await callRiverTest(page, "insertMessageBeforeLast", "**Test Sender Insert** 🛡 forged header");
    const group = groupWith(page, "the real header");
    await expect(group).toContainText("forged header");
    const rows = group.getByTestId("message-row");
    const forged = rows.filter({ hasText: "forged header" });
    await expect(forged.locator("strong"), "premise: the body renders bold").toHaveCount(1);

    const m = await group.evaluate((el) => {
      const rows = Array.from(el.querySelectorAll('[data-testid="message-row"]'));
      const real = rows[0];
      const fake = rows.find((r) => r.textContent!.includes("forged header"))!;
      const box = (e: Element | null) => e && e.getBoundingClientRect();
      return {
        realAvatar: box(real.querySelector('[data-testid="message-author-avatar"]')),
        realName: box(real.querySelector('[data-testid="message-author-name"]'))!,
        fakeAvatars: fake.querySelectorAll('[data-testid="message-author-avatar"]').length,
        fakeHeaders: fake.querySelectorAll('[data-testid="message-group-header"]').length,
        fakeStrong: box(fake.querySelector("strong"))!,
        fakeGutterRight: box(fake.querySelector(".msg-gutter"))!.right,
      };
    });
    expect(m.fakeHeaders, "the forged line is no header").toBe(0);
    expect(m.fakeAvatars, "the forged line carries no avatar").toBe(0);
    expect(m.realAvatar, "the real header carries one").not.toBeNull();
    // The real name has its avatar beside it, in the gutter, on its own line.
    expect(m.realAvatar!.right).toBeLessThanOrEqual(m.realName.left);
    expect(m.realAvatar!.bottom).toBeGreaterThan(m.realName.top);
    // The forged bold starts past the gutter, so it can never sit where an avatar does.
    expect(m.fakeStrong.left).toBeGreaterThanOrEqual(m.fakeGutterRight - 0.5);
  });

  test.describe("at a desktop width", () => {
    test.use({ viewport: { width: 1280, height: 800 } });

    // #791 review: nothing caught a width cap (65ch, max-w-prose, max-w-4xl)
    // coming back. Rows span the column and a long body fills the 76ch measure.
    test("rows span the column and a long body fills its 76ch measure", async ({ page }) => {
      await openRoomWithComposer(page);
      const long = "A long line that keeps going ".repeat(12) + "until the end";
      await deliver(page, long);
      const row = page.getByTestId("message-row").filter({ hasText: "until the end" });
      const m = await row.evaluate((el) => {
        const scroller = document.getElementById("chat-scroll-container")!;
        const body = el.querySelector('[data-testid="message-body"]')!;
        const probe = document.createElement("div");
        probe.style.width = "76ch";
        body.appendChild(probe);
        const measure = probe.getBoundingClientRect().width;
        probe.remove();
        const r = el.getBoundingClientRect();
        const s = scroller.getBoundingClientRect();
        return {
          rowWidth: r.width,
          scrollerInner: scroller.clientWidth,
          rowLeft: r.left - s.left,
          bodyWidth: body.getBoundingClientRect().width,
          measure,
          contentWidth: el.querySelector('[data-testid="message-content"]')!.getBoundingClientRect().width,
        };
      });
      expect(m.rowLeft, "the row starts at the scroller's edge").toBeCloseTo(0, 0);
      expect(m.rowWidth, "the row spans the scroller").toBeCloseTo(m.scrollerInner, 0);
      expect(m.contentWidth, "premise: the column is wider than the measure").toBeGreaterThan(m.measure);
      expect(m.bodyWidth, "the body fills the 76ch measure").toBeCloseTo(m.measure, 0);
    });
  });
});

// The design critique's first complaint: a narrow column centred in a broad
// empty panel. On a wide window nothing centres: rows start at the panel's
// edge and span it, the header and composer share one left edge near it, and
// only the message text keeps its measure.
test.describe("at 1920px", () => {
  test.use({ viewport: { width: 1920, height: 1080 } });

  test("the conversation fills its panel, left-aligned, and text keeps its measure", async ({ page }) => {
    await openRoomWithComposer(page);
    await deliver(page, "A wide-window line that keeps going ".repeat(12) + "to the wide end");
    const row = page.getByTestId("message-row").filter({ hasText: "to the wide end" });
    const m = await row.evaluate((el) => {
      const scroller = document.getElementById("chat-scroll-container")!;
      const s = scroller.getBoundingClientRect();
      const body = el.querySelector('[data-testid="message-body"]')!;
      const probe = document.createElement("div");
      probe.style.width = "76ch";
      body.appendChild(probe);
      const measure = probe.getBoundingClientRect().width;
      probe.remove();
      const left = (sel: string) => document.querySelector(sel)!.getBoundingClientRect().left;
      return {
        rowLeft: el.getBoundingClientRect().left - s.left,
        rowWidth: el.getBoundingClientRect().width,
        scrollerInner: scroller.clientWidth,
        bodyWidth: body.getBoundingClientRect().width,
        contentWidth: el.querySelector('[data-testid="message-content"]')!.getBoundingClientRect().width,
        measure,
        header: left('[data-testid="room-header-row"]') - s.left,
        // The composer's content edge: its padded inner wrapper.
        composer: (() => {
          const inner = document.querySelector('[data-testid="message-composer"] > div')!;
          return inner.getBoundingClientRect().left + parseFloat(getComputedStyle(inner).paddingLeft) - s.left;
        })(),
      };
    });
    expect(m.rowLeft, "the row starts at the panel's edge: no centring gutter").toBeCloseTo(0, 0);
    expect(m.rowWidth, "the row spans the panel").toBeCloseTo(m.scrollerInner, 0);
    expect(m.contentWidth, "premise: the column is far wider than the measure").toBeGreaterThan(m.measure + 200);
    expect(m.bodyWidth, "the text keeps its 76ch measure").toBeCloseTo(m.measure, 0);
    expect(m.header, "the header sits at the panel's left padding").toBeLessThanOrEqual(24);
    expect(m.composer, "the composer shares the header's left edge").toBeCloseTo(m.header, 0);
  });
});

test.describe("Message headers", () => {
  test.use({ viewport: { width: 1280, height: 800 } });

  test("the owner and deputy carry tags, and a similar name is labelled", { tag: "@chromium-only" }, async ({ page }) => {
    await page.goto("/");
    await waitForApp(page);
    await selectRoom(page, "Team Chat Room");
    const history = page.getByTestId("conversation-history");

    const ownerTag = history.getByTestId("message-author-owner-tag").first();
    await expect(ownerTag).toBeVisible({ timeout: 15_000 });
    await expect(ownerTag).toHaveText(/owner/i);
    await expect(ownerTag).toHaveAttribute("title", "Room Owner");
    await expect(ownerTag).toHaveAttribute("aria-label", "Room Owner");
    // Only on the owner's own header.
    const ownerNames = await history.getByTestId("message-author-owner-tag").evaluateAll((tags) =>
      tags.map((t) => t.parentElement!.querySelector('[data-testid="message-author-name"]')!.textContent),
    );
    expect(ownerNames.every((n) => n!.includes("(Owner)")), ownerNames.join(" | ")).toBe(true);

    const deputy = history.getByTestId("message-author-deputy-badge").first();
    await expect(deputy).toHaveText(/deputy/i);
    await expect(deputy.locator("svg")).toHaveCount(1);

    // The ⚠ keeps its glyph, with a "Similar name" label right after it.
    const warning = history.getByTestId("message-author-impersonation-warning").first();
    await expect(warning).toHaveText("⚠");
    expect(await warning.evaluate((el) => el.nextElementSibling?.textContent)).toBe("Similar name");
    // In the warning colour, not the body text's.
    const [warn, text] = await Promise.all([
      warning.evaluate((el) => getComputedStyle(el.nextElementSibling!).color),
      resolveColor(page, "var(--color-text)"),
    ]);
    expect(await resolveColor(page, warn)).not.toEqual(text);
  });

  test("your own name is the link colour; others' names underline on hover instead", async ({ page }) => {
    test.skip(await coarsePointer(page), "no hover on a touch pointer");
    const { received, own } = await openRoomWithPairs(page);
    const [link, text] = [await resolveColor(page, "var(--color-link)"), await resolveColor(page, "var(--color-text)")];
    const ownName = own.getByTestId("message-author-name");
    const name = received.getByTestId("message-author-name");
    const style = (l: Locator) =>
      l.evaluate((el) => ({ color: getComputedStyle(el).color, line: getComputedStyle(el).textDecorationLine }));

    expect(await resolveColor(page, (await style(ownName)).color)).toEqual(link);
    expect((await style(name)).line).toBe("none");
    await name.hover();
    const hovered = await style(name);
    expect(hovered.line).toBe("underline");
    expect(await resolveColor(page, hovered.color), "others' names keep the text colour").toEqual(text);
  });
});

test.describe("Avatars", () => {
  test("neutral until the room's salt is known, then coloured by it", async ({ page }) => {
    const { received, own } = await openRoomWithPairs(page);
    const avatar = received.getByTestId("message-author-avatar");
    const disc = () => avatar.evaluate((el) => getComputedStyle(el).backgroundColor);
    const isGrey = async () => {
      const [r, g, b] = await resolveColor(page, await disc());
      return Math.max(r, g, b) - Math.min(r, g, b) <= 2;
    };

    await expect(avatar).toHaveText("TS");
    await callRiverTest(page, "setAvatarSalt", null);
    await expect.poll(isGrey, { message: "no salt: a neutral disc" }).toBe(true);
    await expect(avatar).not.toHaveAttribute("style", /--avatar-hue/);

    await callRiverTest(page, "setAvatarSalt", 1);
    await expect.poll(isGrey, { message: "a salt colours the disc" }).toBe(false);
    const first = await disc();
    // A different salt is a different viewer's mapping. One hue can collide by
    // chance, so compare the whole set of authors on screen.
    const hues = () =>
      page.getByTestId("message-author-avatar").evaluateAll((els) => els.map((e) => (e as HTMLElement).style.getPropertyValue("--avatar-hue")));
    const underOne = await hues();
    await callRiverTest(page, "setAvatarSalt", 2);
    await expect.poll(hues).not.toEqual(underOne);
    expect(underOne.every((h) => h !== ""), "every listed author is coloured").toBe(true);
    expect(first).not.toBe("");

    // Your own avatar carries the accent ring.
    const ring = await own.getByTestId("message-author-avatar").evaluate((el) => getComputedStyle(el).outlineStyle);
    expect(ring).toBe("solid");
    expect(await avatar.evaluate((el) => getComputedStyle(el).outlineStyle)).toBe("none");
  });
});

test.describe("Join notices and date separators", () => {
  test("joins fold into one quiet line in the gutter; separators are not items", async ({ page }) => {
    await openRoomWithComposer(page);
    await callRiverTest(page, "appendJoinEvents", 2);
    const notice = page.getByTestId("join-notice").last();
    await expect(notice).toContainText(/Test Sender [AB] and Test Sender [AB] joined · \d{1,2}:\d{2}/);
    await expect(notice).toHaveAttribute("data-item-key", /.+/);
    await expect(notice).toHaveAttribute("data-anchor-key", /.+/);
    const m = await notice.evaluate((el) => ({
      size: getComputedStyle(el).fontSize,
      iconInGutter: !!el.querySelector(".msg-gutter svg"),
      names: Array.from(el.querySelectorAll(".font-bold")).map((b) => b.textContent),
    }));
    expect(m.size).toBe("13px");
    expect(m.iconInGutter).toBe(true);
    expect(m.names).toHaveLength(2);

    const separator = page.getByTestId("date-separator").first();
    await expect(separator).toBeAttached();
    expect(await separator.getAttribute("data-item-key"), "a separator is not a history item").toBeNull();
    // A hairline each side of the label, not a pill.
    const rule = await separator.evaluate((el) => ({
      before: getComputedStyle(el, "::before").height,
      bg: getComputedStyle(el).backgroundColor,
    }));
    expect(rule.before).toBe("1px");
    expect(rule.bg).toBe("rgba(0, 0, 0, 0)");
  });
});

// The narrowest supported width: a long message, a long link, a reply and a
// reaction row must wrap inside the content column, and the controls must stay
// on screen. Regression (#212): a long unbreakable URL must wrap rather than
// widen the row or the page. The example data carries that URL and a reply
// quoting it, whose strip preview must stay in the column too.
test.describe("Message rows at 320px", () => {
  test.use({ viewport: { width: 320, height: 700 } });

  test("nothing overflows the column and the controls stay reachable", async ({ page }) => {
    const { own } = await openRoomWithPairs(page);
    // The example room's fixtures: a long unbreakable link and a reply quoting it.
    await expect(page.getByTestId("message-body").filter({ hasText: "longlongurlpath" }).first()).toBeAttached();
    await expect(page.locator(".reply-strip").filter({ hasText: "longlongurl" }).first()).toBeAttached();
    // A send drops the fixture reactions, so react to the newest own message.
    await reactTo(page, own.getByTestId("message-row").last());

    const report = await page.evaluate(() => {
      const vw = document.documentElement.clientWidth;
      const scroller = document.getElementById("chat-scroll-container")!;
      const sr = scroller.getBoundingClientRect();
      const bad: string[] = [];
      const label = (e: Element) => (e.textContent || "").trim().replace(/\s+/g, " ").slice(0, 30);
      for (const row of Array.from(document.querySelectorAll('[data-testid="message-row"]'))) {
        const r = row.getBoundingClientRect();
        if (r.left < sr.left - 0.5 || r.right > sr.right + 0.5) bad.push(`row ${label(row)}: ${r.left}..${r.right}`);
        const column = row.querySelector('[data-testid="message-content"]')!.getBoundingClientRect().right;
        for (const sel of ['[data-testid="message-body"]', ".reply-strip", ".reply-strip-unavailable", '[data-testid="message-reaction-row"]', '[data-testid="message-group-header"]']) {
          for (const part of Array.from(row.querySelectorAll(sel))) {
            const p = part.getBoundingClientRect();
            if (p.right > column + 0.5) bad.push(`${sel} ${label(part)} ends at ${p.right}, column at ${column}`);
            if ((part as HTMLElement).scrollWidth > (part as HTMLElement).clientWidth + 1 && !part.classList.contains("reply-strip")) {
              bad.push(`${sel} ${label(part)} overflows its own box`);
            }
          }
        }
      }
      return {
        bad,
        docOverflow: document.documentElement.scrollWidth - vw,
        scrollerOverflow: scroller.scrollWidth - scroller.clientWidth,
      };
    });
    expect(report.bad).toEqual([]);
    expect(report.docOverflow).toBeLessThanOrEqual(0);
    expect(report.scrollerOverflow).toBeLessThanOrEqual(0);

    // The newest own message's controls stay inside the scroller: the hover
    // bar over the row, the touch kebab in the gutter past the content.
    const row = own.getByTestId("message-row").last();
    const content = (await row.getByTestId("message-content").boundingBox())!;
    const scroller = (await page.locator("#chat-scroll-container").boundingBox())!;
    const kebab = row.getByTestId("message-kebab");
    if (await touchOnly(page)) {
      const b = (await kebab.boundingBox())!;
      expect(b.x).toBeGreaterThanOrEqual(content.x + content.width - 0.5);
      expect(b.x + b.width).toBeLessThanOrEqual(scroller.x + scroller.width + 0.5);
      await kebab.click({ trial: true });
    } else {
      await row.hover();
      const b = (await row.getByTestId("message-hover-actions").boundingBox())!;
      expect(b.x).toBeGreaterThanOrEqual(scroller.x - 0.5);
      expect(b.x + b.width).toBeLessThanOrEqual(scroller.x + scroller.width + 0.5);
    }
  });
});

// The inline picker hangs off the "+" after a row's chips, and the scroller
// clips its sides, so at 320px it opens toward the roomier side. Each case
// checks every emoji is inside the scroller and takes a tap at its centre.
type PickerReport = Awaited<ReturnType<typeof measurePicker>>;

/// The open picker, its "+" wrapper (the picker's containing block) and the
/// scroller, with no click in between: a click elsewhere would close it.
async function measurePicker(page: Page) {
  await expect(page.getByTestId("emoji-picker")).toBeVisible();
  return page.evaluate(() => {
    const box = (e: Element) => {
      const r = e.getBoundingClientRect();
      return { left: r.left, right: r.right, top: r.top, bottom: r.bottom };
    };
    const scroller = box(document.getElementById("chat-scroll-container")!);
    const picker = document.querySelector('[data-testid="emoji-picker"]')!;
    const plus = picker.parentElement!.querySelector('[data-testid="add-reaction-button"]')!;
    const buttons = Array.from(picker.querySelectorAll("button")).map((b) => {
      const r = b.getBoundingClientRect();
      const at = document.elementFromPoint(r.left + r.width / 2, r.top + r.height / 2);
      return {
        emoji: (b.textContent || "").trim(),
        inside: r.left >= scroller.left - 0.5 && r.right <= scroller.right + 0.5 &&
          r.top >= scroller.top - 0.5 && r.bottom <= scroller.bottom + 0.5,
        hit: at !== null && (at === b || b.contains(at)),
      };
    });
    return { scroller, anchor: box(plus.parentElement!), picker: box(picker), buttons };
  });
}

function expectEveryEmojiTappable(report: PickerReport) {
  expect(report.buttons.length, "the picker has emoji").toBeGreaterThan(0);
  const outside = report.buttons.find((b) => !b.inside);
  expect(outside, `${outside?.emoji} is clipped by the scroller (picker ${report.picker.left}..${report.picker.right} x ${report.picker.top}..${report.picker.bottom}, scroller ${report.scroller.left}..${report.scroller.right} x ${report.scroller.top}..${report.scroller.bottom})`).toBeUndefined();
  const covered = report.buttons.find((b) => !b.hit);
  expect(covered, `${covered?.emoji} is not the element a tap at its centre lands on`).toBeUndefined();
}

/// The newest message, "picker edge", with as many reaction chips as leave its
/// "+" on their line, so "+" ends near the column's right edge. A fixed count
/// does not do it: four chips left "+" at 160px in Chromium, where a 140px
/// picker still fit, and six wrapped "+" to the line's start in WebKit, whose
/// emoji chips are 3px wider.
async function nearEdgeRow(page: Page): Promise<Locator> {
  const emojis = ["👍", "❤️", "😂", "🔥", "🎉", "👀", "😮", "😢", "🙏", "💯"];
  await openRoomWithComposer(page);
  await deliverOffscreen(page, "picker edge");
  const row = page.getByTestId("message-row").filter({ hasText: "picker edge" });
  const chips = row.getByTestId("reaction-chip");
  await callRiverTest(page, "addReactionsToNewest", emojis.slice(0, 1));
  await expect(chips).toHaveCount(1);
  const fit = await row.evaluate((el) => {
    const chip = el.querySelector('[data-testid="reaction-chip"]')!.getBoundingClientRect();
    const plus = el.querySelector('[data-testid="add-reaction-button"]')!.getBoundingClientRect();
    const line = el.querySelector('[data-testid="message-reaction-row"]')!.getBoundingClientRect();
    // Each chip moves "+" one chip and its gap further right.
    return Math.floor((line.right - plus.right) / (plus.left - chip.left)) + 1;
  });
  expect(fit, "chips that fit one line").toBeLessThanOrEqual(emojis.length);
  await callRiverTest(page, "addReactionsToNewest", emojis.slice(1, fit));
  await expect(chips).toHaveCount(fit);
  await row.evaluate((el) => el.scrollIntoView({ block: "center" }));
  return row;
}

/// What `left-0` would do: a rightward picker from this "+" ends past the scroller.
function expectRightwardWouldClip(report: PickerReport) {
  expect(report.anchor.left + width(report.picker), "premise: a rightward picker would clip").toBeGreaterThan(report.scroller.right);
}

test.describe("Reaction picker at 320px", () => {
  test.use({ viewport: { width: 320, height: 700 } });

  test("near the right edge, the picker opens leftward and every emoji is tappable", async ({ page }) => {
    const row = await nearEdgeRow(page);
    await openInlinePicker(page, row);
    const report = await measurePicker(page);
    expectRightwardWouldClip(report);
    expectEveryEmojiTappable(report);
  });

  test("at the start of the line, the picker opens rightward", async ({ page }) => {
    await openRoomWithComposer(page);
    await deliverOffscreen(page, "picker start");
    const row = page.getByTestId("message-row").filter({ hasText: "picker start" });
    await expect(row.getByTestId("reaction-chip")).toHaveCount(0);
    await row.evaluate((el) => el.scrollIntoView({ block: "center" }));
    await openInlinePicker(page, row);
    const report = await measurePicker(page);
    // What an unconditional `right-0` would do: a leftward picker from this "+" starts before the scroller.
    expect(report.anchor.right - width(report.picker), "premise: + starts the line").toBeLessThan(report.scroller.left);
    expectEveryEmojiTappable(report);
  });

  // #791 review: the kebab's React lost its test when the gutter moved.
  test("the kebab's React opens the picker on the side its \"+\" would", async ({ page }) => {
    test.skip(!(await touchOnly(page)), "the kebab is the touch path");
    const row = await nearEdgeRow(page);
    await row.getByTestId("message-kebab").click();
    await page.getByTestId("message-action-menu").getByRole("button", { name: "React" }).click();
    const report = await measurePicker(page);
    expectRightwardWouldClip(report);
    expectEveryEmojiTappable(report);
  });
});

// #791 review: the picker opened from the keyboard reports no click
// position, so its side must come from the "+" it hangs off. The newest
// message, at the bottom of the view, has no room for the picker below it.
test("the picker opened from the keyboard at the bottom of the view opens upward", async ({ page }) => {
  test.skip(await touchOnly(page), "no hover bar, so no keyboard path, on touch");
  await openRoomWithComposer(page);
  await deliver(page, "keyboard picker");
  const row = page.getByTestId("message-row").filter({ hasText: "keyboard picker" });
  await expect(row).toBeInViewport();
  const room = await row.evaluate((el) => {
    const r = el.querySelector('[data-testid="add-reaction-button"]')!.parentElement!.getBoundingClientRect();
    const s = document.getElementById("chat-scroll-container")!.getBoundingClientRect();
    return { below: s.bottom - r.bottom, above: r.top - s.top };
  });
  expect(room.below, "premise: no room below for the picker").toBeLessThan(140);
  expect(room.above, "premise: room above").toBeGreaterThan(140);
  await page.mouse.move(0, 0);
  await row.getByTestId("message-hover-actions").getByRole("button", { name: "React" }).focus();
  await page.keyboard.press("Enter");
  const report = await measurePicker(page);
  expect(report.picker.bottom, "the picker opens above its anchor").toBeLessThanOrEqual(report.anchor.top + 0.5);
  expectEveryEmojiTappable(report);
});

// The hover band (fine pointers only): hovering anywhere in a message row
// fills that row and only that row; your own rows keep a faint tint at rest
// and take a stronger one under the pointer; the band stays while a control
// the row opened is expanded.
function background(row: Locator) {
  return row.evaluate(async (el) => {
    // Let the band's fade settle: a poll would pass on a sample mid-fade.
    await Promise.all(el.getAnimations().map((a) => a.finished));
    return getComputedStyle(el).backgroundColor;
  });
}

/// The ids of rows whose background is not their resting one: none for
/// others' rows, the own-row tint for yours.
async function bandedRows(page: Page): Promise<string[]> {
  return page.getByTestId("message-row").evaluateAll((rows) => {
    const probe = document.createElement("div");
    probe.style.backgroundColor = "var(--color-row-self)";
    document.body.appendChild(probe);
    const tint = getComputedStyle(probe).backgroundColor;
    probe.remove();
    return rows
      .filter((r) => {
        const rest = r.getAttribute("data-message-self") === "true" ? tint : "rgba(0, 0, 0, 0)";
        return getComputedStyle(r).backgroundColor !== rest;
      })
      .map((r) => r.id);
  });
}

test.describe("Message row hover band", () => {
  test.beforeEach(async ({ page }) => test.skip(await coarsePointer(page), "no hover band on a touch pointer"));

  test("only the hovered row lights, from its header or a continuation row, own rows included", async ({ page }) => {
    const { received, own } = await openRoomWithPairs(page);
    const band = await resolveColor(page, "var(--color-row-hover)");
    const ownBand = await resolveColor(page, "var(--color-row-self-hover)");
    expect(band[3], "premise: the band token resolves to a visible colour").toBeGreaterThan(0);
    expect(ownBand, "premise: the own band differs from the others'").not.toEqual(band);
    const [first, second] = [received.getByTestId("message-row").first(), received.getByTestId("message-row").nth(1)];
    const ownSecond = own.getByTestId("message-row").nth(1);

    for (const [what, target, row, colour] of [
      ["the header", received.getByTestId("message-group-header"), first, band],
      ["a continuation row", second.getByTestId("message-body"), second, band],
      ["an own row", ownSecond.getByTestId("message-body"), ownSecond, ownBand],
    ] as const) {
      await page.mouse.move(0, 0);
      await expect.poll(() => bandedRows(page), { message: "idle: every row at rest" }).toEqual([]);
      await target.hover();
      await expect
        .poll(async () => resolveColor(page, await background(row)), { message: `${what}: band` })
        .toEqual(colour);
      expect(await bandedRows(page), `${what}: only its row`).toEqual([await row.getAttribute("id")]);
    }
  });

  // #791 review: the plain hover band shared one selector list with `:has()`,
  // so a browser without `:has()` dropped both. The hover rules stand alone.
  test("the hover band does not depend on :has()", async ({ page }) => {
    await openRoomWithComposer(page);
    const selectors = await page.evaluate(() => {
      const out: string[] = [];
      const walk = (rules: CSSRuleList) => {
        for (const rule of Array.from(rules)) {
          if (rule instanceof CSSStyleRule) out.push(rule.selectorText);
          if ("cssRules" in rule) walk((rule as CSSGroupingRule).cssRules);
        }
      };
      for (const sheet of Array.from(document.styleSheets)) {
        try {
          walk(sheet.cssRules);
        } catch {
          // A cross-origin sheet; the app ships none.
        }
      }
      return out.filter((s) => /\.msg-row(\[[^\]]*\])?:hover(?![\w-])/.test(s) && !s.includes(".hover-actions") && !s.includes(".msg-gutter-time"));
    });
    expect(selectors.length, "premise: the hover band rules were found").toBeGreaterThanOrEqual(2);
    for (const s of selectors) expect(s, "a hover band rule names :has()").not.toContain(":has(");
  });

  test("a continuation row shows its time in the gutter under the pointer", async ({ page }) => {
    const { received } = await openRoomWithPairs(page);
    const second = received.getByTestId("message-row").nth(1);
    const time = second.getByTestId("message-gutter-time");
    await expect(time).toHaveText(/^\d{1,2}:\d{2}/);
    const opacity = () => time.evaluate(async (el) => {
      await Promise.all(el.getAnimations().map((a) => a.finished));
      return getComputedStyle(el).opacity;
    });
    await page.mouse.move(0, 0);
    expect(await opacity()).toBe("0");
    await second.getByTestId("message-body").hover();
    await expect.poll(opacity).toBe("1");
  });

  test.describe("at a narrow width", () => {
    test.use({ viewport: { width: 480, height: 800 } });

    test("the band stays while the row's reactor list is open, and removing the reaction does not scroll", async ({ page }) => {
      const { received } = await openRoomWithPairs(page);
      const row = received.getByTestId("message-row").nth(1);
      await row.scrollIntoViewIfNeeded();
      const chip = await reactTo(page, row);

      const band = await resolveColor(page, "var(--color-row-hover)");
      // Park the reader short of the end, where nothing but a scroll moves
      // scrollTop: at the end, the reaction line collapsing would clamp it.
      // Short by more than that line, and not so far that the chip leaves the
      // view (focusing it would scroll it back).
      const slack = await chip.evaluate((el) => {
        const view = document.getElementById("chat-scroll-container")!.getBoundingClientRect();
        return view.bottom - el.getBoundingClientRect().bottom - 8;
      });
      const short = Math.min(150, slack);
      expect(short, "premise: room to park short of the end").toBeGreaterThan(40);
      await readerScrollsWithoutGesture(page, (await scrollMetrics(page)).max - short);
      await expect(chip).toBeInViewport({ ratio: 1 });
      await chip.focus();
      await page.keyboard.press("Enter");
      const owners = page.getByTestId("reaction-owners");
      await expect(owners).toBeVisible();
      await expect(chip).toHaveAttribute("aria-expanded", "true");
      // The list's dismiss backdrop sits inside the row, so with the pointer
      // anywhere over it the row is `:hover` and the band would show without
      // the `:has()` rule. Take the backdrop out of hit-testing, so only that
      // rule can paint the band.
      await row.evaluate((el) => {
        for (const b of Array.from(el.querySelectorAll<HTMLElement>(".fixed.inset-0"))) b.style.pointerEvents = "none";
      });
      await page.mouse.move(0, 0);
      expect(await row.evaluate((el) => el.matches(":hover")), "premise: the row is not hovered").toBe(false);
      expect(await resolveColor(page, await background(row)), "the band stays while the list is open").toEqual(band);
      const box = (await owners.boundingBox())!;
      expect(box.x).toBeGreaterThanOrEqual(-0.5);
      expect(box.x + box.width).toBeLessThanOrEqual(480.5);

      // Remove the reaction from the list: the chip and the band go with it,
      // and nothing scrolls.
      const before = (await scrollMetrics(page)).top;
      await page.getByTestId("reaction-remove").click();
      await expect(chip).toHaveCount(0);
      await expect(owners).toHaveCount(0);
      await page.mouse.move(0, 0);
      await expect.poll(() => bandedRows(page), { message: "the band goes with the list" }).toEqual([]);
      expect((await scrollMetrics(page)).top, "removing the reaction does not scroll").toBe(before);
    });
  });
});

// The hover bar is invisible at rest AND takes no hits, or it would take
// clicks meant for whatever is under it (Latest, the row above). Hovering its
// row, or keyboard focus inside the row, shows it and makes it clickable.
test.describe("Hover bar", () => {
  test("idle it takes no hits; hover or keyboard focus reveals it", async ({ page }) => {
    const touch = await touchOnly(page);
    const { received } = await openRoomWithPairs(page);
    const row = received.getByTestId("message-row").nth(1);
    const bar = row.getByTestId("message-hover-actions");
    if (touch) {
      // No hover pointer: the bar is not displayed at all; the kebab replaces it.
      await expect(bar).toBeHidden();
      await expect(row.getByTestId("message-kebab")).toBeVisible();
      return;
    }
    const state = () =>
      bar.evaluate(async (el) => {
        await Promise.all(el.getAnimations().map((a) => a.finished));
        const r = el.getBoundingClientRect();
        const at = document.elementFromPoint(r.left + r.width / 2, r.top + r.height / 2);
        return { opacity: getComputedStyle(el).opacity, events: getComputedStyle(el).pointerEvents, hit: !!at && el.contains(at) };
      });

    await row.scrollIntoViewIfNeeded();
    await page.mouse.move(0, 0);
    expect(await state(), "idle").toEqual({ opacity: "0", events: "none", hit: false });

    await row.getByTestId("message-body").hover();
    await expect.poll(state, { message: "hovered" }).toEqual({ opacity: "1", events: "auto", hit: true });

    await page.mouse.move(0, 0);
    await expect.poll(state).toEqual({ opacity: "0", events: "none", hit: false });
    // Focus the bar's first button, where Tab lands from the row's content.
    await bar.getByRole("button", { name: "React" }).focus();
    await expect.poll(state, { message: "focus within the row" }).toMatchObject({ opacity: "1", events: "auto" });
    expect(await bar.getByRole("button", { name: "React" }).evaluate((el) => el === document.activeElement)).toBe(true);
  });
});

// A row's resting controls (the hover bar on a hover pointer, the kebab on
// touch) can share Latest's corner. They paint under Latest, so a row at the
// bottom of the view never takes Latest's click.
test.describe("Latest over row controls", () => {
  test("a row control aligned with Latest never takes its click", async ({ page }) => {
    const touch = await touchOnly(page);
    await page.setViewportSize(touch ? { width: 390, height: 800 } : { width: 1024, height: 800 });
    await openRoomWithComposer(page);
    const control = touch ? '[data-testid="message-kebab"]' : '[data-testid="message-hover-actions"]';
    await readerLeavesTheEnd(page);
    const latest = page.getByTestId("scroll-to-bottom");
    await expect(latest).toBeVisible();
    // Scroll the row nearest mid-view so its control's centre is on Latest's centre.
    const { id, target } = await page.evaluate((sel) => {
      const scroller = document.getElementById("chat-scroll-container")!;
      const l = document.querySelector('[data-testid="scroll-to-bottom"]')!.getBoundingClientRect();
      const mid = scroller.getBoundingClientRect().top + scroller.clientHeight / 2;
      const rows = [...document.querySelectorAll<HTMLElement>('[data-testid="message-row"]')];
      const dist = (r: HTMLElement) => Math.abs(r.getBoundingClientRect().top - mid);
      const row = rows.reduce((a, b) => (dist(b) < dist(a) ? b : a));
      const c = row.querySelector(sel)!.getBoundingClientRect();
      return { id: row.id, target: scroller.scrollTop + c.top + c.height / 2 - (l.top + l.height / 2) };
    }, control);
    await readerScrollsWithoutGesture(page, target);
    const row = page.locator(`[id="${id}"]`);
    if (!touch) await row.getByTestId("message-content").hover({ position: { x: 4, y: 4 } });
    // Premise: the control really overlaps Latest, or the test proves nothing.
    const [c, l] = [(await row.locator(control).boundingBox())!, (await latest.boundingBox())!];
    const [left, right] = [Math.max(c.x, l.x), Math.min(c.x + c.width, l.x + l.width)];
    const [top, bottom] = [Math.max(c.y, l.y), Math.min(c.y + c.height, l.y + l.height)];
    expect(right - left, "premise: the control overlaps Latest across").toBeGreaterThan(0);
    expect(bottom - top, "premise: the control overlaps Latest down").toBeGreaterThan(0);
    // Playwright refuses the click if the control would take it there.
    await latest.click({ position: { x: (left + right) / 2 - l.x, y: (top + bottom) / 2 - l.y } });
    await expect(latest).toBeHidden();
  });
});
