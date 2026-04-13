import { test, expect, Page } from "@playwright/test";

// Regression tests for freenet/river#205, #206, #207:
//   #205 edit box wider than view
//   #206 reply messages require more width than others
//   #207 hovering a reply changes the width of the message
//
// Assumes the example-data build is served on `baseURL`, which includes a
// reply message added in ui/src/example_data.rs specifically so these tests
// can exercise the reply bubble layout.

async function waitForApp(page: Page) {
  await page.waitForSelector(".app-root", { timeout: 30_000 });
  await expect(page.locator("aside, .app-root button")).not.toHaveCount(0);
}

async function selectRoom(page: Page, roomName: string) {
  const roomBtn = page.getByRole("button", { name: roomName });
  if (!(await roomBtn.isVisible({ timeout: 500 }).catch(() => false))) {
    // Narrow-window case: temporarily expand to click the room.
    const vp = page.viewportSize();
    if (vp && vp.width < 768) {
      await page.setViewportSize({ width: 1280, height: vp.height });
      await expect(roomBtn).toBeVisible({ timeout: 5_000 });
      await roomBtn.click();
      await expect(
        page.getByRole("heading", { name: roomName })
      ).toBeVisible({ timeout: 5_000 });
      await page.setViewportSize({ width: vp.width, height: vp.height });
      return;
    }
  }
  await roomBtn.click();
  await expect(
    page.getByRole("heading", { name: roomName })
  ).toBeVisible({ timeout: 5_000 });
}

// #205: on a narrow viewport, clicking edit on an own message must not produce
// an edit container wider than the available chat area.
test.describe("Edit box width (#205)", () => {
  test.use({ viewport: { width: 500, height: 900 } });

  test("edit container fits within viewport at narrow widths", async ({
    page,
  }) => {
    await page.goto("/");
    await waitForApp(page);
    await selectRoom(page, "Your Private Room");

    // Hover each message bubble until we find one that exposes an Edit
    // button (own messages in the private room, where the owner IS self).
    // Bubbles are divs with `max-w-prose` in the class list.
    const bubbles = page.locator(".max-w-prose");
    const count = await bubbles.count();
    expect(count).toBeGreaterThan(0);

    let clicked = false;
    for (let i = 0; i < count; i++) {
      const bubble = bubbles.nth(i);
      await bubble.scrollIntoViewIfNeeded();
      await bubble.hover();
      // Scope the edit button lookup to the hovered bubble's ancestor
      // (the outer message container with the hover action bar), so a
      // stray previously-visible edit button on another message doesn't
      // mask the current hover target.
      const msgContainer = bubble.locator(
        "xpath=ancestor::*[starts-with(@id,'msg-')][1]"
      );
      const editBtn = msgContainer.getByRole("button", { name: /edit/i });
      if (await editBtn.isVisible({ timeout: 500 }).catch(() => false)) {
        await editBtn.click();
        clicked = true;
        break;
      }
    }
    expect(clicked, "found an own-message edit button").toBe(true);

    const textarea = page.locator("textarea").first();
    await expect(textarea).toBeVisible({ timeout: 5_000 });

    // The edit container (parent of the textarea) must not exceed the
    // viewport width. Before the fix it had an inline `width: 550px` and
    // spilled out of a 500px viewport.
    const editBox = await textarea.evaluate((el) => {
      const container = el.parentElement as HTMLElement;
      const rect = container.getBoundingClientRect();
      return { width: rect.width, right: rect.right };
    });

    expect(editBox.width).toBeLessThanOrEqual(500);
    expect(editBox.right).toBeLessThanOrEqual(500 + 1); // +1 for sub-pixel rounding
  });
});

// #206 and #207: at a narrow viewport, a reply message bubble should not be
// wider than a sibling non-reply bubble, and hovering the reply strip should
// not change the bubble's width.
test.describe("Reply bubble layout (#206, #207)", () => {
  test.use({ viewport: { width: 480, height: 900 } });

  test("reply bubbles are not wider than non-reply bubbles", async ({
    page,
  }) => {
    await page.goto("/");
    await waitForApp(page);
    await selectRoom(page, "Your Private Room");

    const replyStrip = page.locator(".reply-strip").first();
    await expect(replyStrip).toBeVisible({ timeout: 10_000 });

    // Find the bubble containing the reply strip and a sibling non-reply bubble.
    const replyBubble = replyStrip.locator(
      "xpath=ancestor::*[contains(@class,'max-w-prose')][1]"
    );
    const allBubbles = page.locator(".max-w-prose");
    const bubbleCount = await allBubbles.count();
    let maxNonReplyWidth = 0;
    for (let i = 0; i < bubbleCount; i++) {
      const bubble = allBubbles.nth(i);
      const hasReplyStrip = await bubble
        .locator(".reply-strip")
        .count();
      if (hasReplyStrip === 0) {
        const w = await bubble.evaluate(
          (el) => el.getBoundingClientRect().width
        );
        if (w > maxNonReplyWidth) maxNonReplyWidth = w;
      }
    }

    const replyWidth = await replyBubble.evaluate(
      (el) => el.getBoundingClientRect().width
    );

    // Reply bubble width should be determined by its body content (same as
    // non-reply siblings), not by the strip's nowrap text. Allow a small
    // margin because different content lengths mean widths won't be exactly
    // equal — the regression was a visible 100-200px gap.
    //
    // Before the fix: reply bubbles were dramatically wider because the
    // reply-strip's nowrap text forced the shrink-to-fit width up to
    // max-w-prose. After the fix: bubble is sized by the body, strip
    // ellipsizes within it.
    expect(replyWidth).toBeLessThanOrEqual(maxNonReplyWidth + 40);
  });

  test("hovering the reply strip does not change the bubble width", async ({
    page,
  }, testInfo) => {
    await page.goto("/");
    await waitForApp(page);
    await selectRoom(page, "Your Private Room");

    const replyStrip = page.locator(".reply-strip").first();
    await expect(replyStrip).toBeVisible({ timeout: 10_000 });

    // The hover-expand CSS is gated behind
    // `@media (hover: hover) and (pointer: fine)`, which evaluates false
    // on touch-emulated Playwright projects AND on some headless desktop
    // Firefox configurations. On those browsers the :hover rule never
    // applies, so hovering cannot cause a reflow at all — the test would
    // pass for the wrong reason. Skip when the media query is false, so
    // the test only runs (and only matters) when it actually exercises
    // the hover reflow pathway.
    const hoverCapable = await page.evaluate(() =>
      window.matchMedia("(hover: hover) and (pointer: fine)").matches
    );
    test.skip(
      !hoverCapable,
      `(hover: hover) and (pointer: fine) is false in this browser (project: ${testInfo.project.name}); the hover-expand CSS is suppressed and there is nothing to exercise`
    );

    const replyBubble = replyStrip.locator(
      "xpath=ancestor::*[contains(@class,'max-w-prose')][1]"
    );

    const widthBefore = await replyBubble.evaluate(
      (el) => el.getBoundingClientRect().width
    );

    // Move mouse to the origin first to ensure no prior hover state
    // affects the measurement, then hover the reply strip.
    await page.mouse.move(0, 0);
    await replyStrip.hover();
    // Poll until the computed `white-space` flips to `normal`, which
    // proves the hover CSS actually engaged.
    await expect
      .poll(async () =>
        replyStrip.evaluate((el) => getComputedStyle(el).whiteSpace)
      )
      .toMatch(/normal/);

    const widthAfter = await replyBubble.evaluate(
      (el) => el.getBoundingClientRect().width
    );

    // Width must not change when the reply strip expands on hover.
    expect(Math.abs(widthAfter - widthBefore)).toBeLessThanOrEqual(0.5);
  });
});

// #210: the reply strip has onclick and cursor-pointer but was previously a
// plain div with no tabindex / role / key handler, and the hover-expand CSS
// had no :focus-visible equivalent, so keyboard users couldn't reach or
// activate it.
test.describe("Reply strip keyboard accessibility (#210)", () => {
  test.use({ viewport: { width: 1280, height: 800 } });

  test("reply strip is keyboard-focusable and announces as a button", async ({
    page,
  }) => {
    await page.goto("/");
    await waitForApp(page);
    await selectRoom(page, "Your Private Room");

    const replyStrip = page.locator(".reply-strip").first();
    await expect(replyStrip).toBeVisible({ timeout: 10_000 });

    // ARIA contract
    await expect(replyStrip).toHaveAttribute("role", "button");
    await expect(replyStrip).toHaveAttribute("tabindex", "0");
    await expect(replyStrip).toHaveAttribute("aria-label", /reply/i);

    // Focusable via .focus() — this also verifies the element accepts focus
    // at the DOM level (tabindex >= 0).
    await replyStrip.evaluate((el) => (el as HTMLElement).focus());
    const isFocused = await replyStrip.evaluate(
      (el) => document.activeElement === el
    );
    expect(isFocused).toBe(true);
  });

  test("a :focus-visible CSS rule exists for the reply strip", async ({
    page,
  }) => {
    // Playwright's programmatic `.focus()` does not reliably trigger
    // `:focus-visible` in headless Chromium (the spec defines it via a
    // heuristic that considers the input modality, and scripted focus
    // is treated as mouse-like). Instead of trying to simulate keyboard
    // focus, verify the stylesheet actually contains the rule — that's
    // what the a11y contract requires, and it's what would regress if
    // someone deleted the CSS.
    await page.goto("/");
    await waitForApp(page);
    await selectRoom(page, "Your Private Room");

    const hasFocusVisibleRule = await page.evaluate(() => {
      for (const sheet of Array.from(document.styleSheets)) {
        let rules: CSSRuleList | null = null;
        try {
          rules = sheet.cssRules;
        } catch {
          continue;
        }
        if (!rules) continue;
        for (const rule of Array.from(rules)) {
          if (
            rule instanceof CSSStyleRule &&
            rule.selectorText &&
            rule.selectorText.includes(".reply-strip") &&
            rule.selectorText.includes(":focus-visible")
          ) {
            return true;
          }
        }
      }
      return false;
    });
    expect(
      hasFocusVisibleRule,
      ".reply-strip:focus-visible CSS rule must exist so keyboard users see full preview (#210)"
    ).toBe(true);
  });

  test("pressing Enter or Space on the focused reply strip scrolls to the original", async ({
    page,
  }) => {
    await page.goto("/");
    await waitForApp(page);
    await selectRoom(page, "Your Private Room");

    const replyStrip = page.locator(".reply-strip").first();
    await expect(replyStrip).toBeVisible({ timeout: 10_000 });

    // The onclick handler adds the `reply-highlight` class to the target
    // message after scrolling; pressing Enter/Space on the focused strip
    // must do the same (Space needs preventDefault to stop the page from
    // scrolling).
    await replyStrip.focus();
    await page.keyboard.press("Enter");

    // Wait for the highlight class to appear on any `[id^='msg-']` element.
    await expect
      .poll(async () =>
        page.locator("[id^='msg-'].reply-highlight").count()
      )
      .toBeGreaterThan(0);
  });
});
