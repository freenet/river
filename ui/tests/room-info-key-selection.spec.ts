import { test, expect, Page, Locator } from "@playwright/test";

// Regression test for freenet/river#537: the Room Public Key and Contract ID
// in the room-details panel could not be selected or copied in Firefox.
//
// The fields carried Tailwind's `select-all` (`user-select: all`). Firefox
// parses that — the computed value really is `all` — but selecting inside an
// `<input>` under it produces a ZERO-length selection, so click-drag and
// double-click both did nothing and Ctrl+C copied nothing. Chromium and WebKit
// select the whole value under the same rule, which is why the bug looked
// Firefox-specific. The fix declares `user-select: text` explicitly.
//
// This spec runs on every project in playwright.config.ts, so the Firefox
// project is the actual gate; chromium/webkit assert the fix did not regress
// the browsers that already worked.

const ROOM_NAME = "Public Discussion Room";

// Playwright's WebKit build will not deliver a keyboard copy to a READONLY
// input. Isolated with a standalone page containing nothing but two inputs,
// one `readonly` and one editable — no River code involved:
//
//   engine    | readonly Ctrl+A | readonly mouse-select | readonly Ctrl+C | editable
//   firefox   | selects         | selects               | copy fires      | all work
//   chromium  | selects         | selects               | copy fires      | all work
//   webkit    | selects NOTHING | selects               | NO copy event   | all work
//
// So on WebKit only the KEYBOARD half is unobservable; mouse selection — the
// behaviour this spec exists to protect — works and is asserted on every
// project including webkit and mobile-safari. These fields were already
// `readonly` before the fix, so this is not a regression, and it is a property
// of the harness rather than of the app. Firefox is the browser from the bug
// report, so the Ctrl+C acceptance criterion is still genuinely gated.
const WEBKIT_KEYBOARD_COPY_SKIP =
  "Playwright's WebKit does not deliver a keyboard copy to a readonly input (harness limitation, verified against a bare input outside River); mouse selection is still asserted on webkit.";

async function waitForApp(page: Page) {
  await page.waitForSelector(".app-root", { timeout: 30_000 });
  await expect(page.locator("aside, .app-root button")).not.toHaveCount(0);
}

async function openRoomDetails(page: Page) {
  const vp = page.viewportSize();
  if (vp && vp.width < 1024) {
    await page.setViewportSize({ width: 1280, height: vp.height });
  }

  // Scope to the room list — once a room is selected the header button carries
  // the room name as its accessible name too, which would be a second match.
  const roomBtn = page.getByTestId("room-list").getByRole("button", { name: ROOM_NAME });
  await expect(roomBtn).toBeVisible({ timeout: 10_000 });
  await roomBtn.click();
  await expect(page.getByRole("heading", { name: ROOM_NAME })).toBeVisible({ timeout: 5_000 });

  // The (i) affordance in the room header opens the room-details modal.
  await page.getByTitle("Room details").click();
  await expect(page.getByTestId("edit-room-modal")).toBeVisible({ timeout: 5_000 });
}

/**
 * Capture what the app actually writes to the clipboard.
 *
 * `crate::util::copy_to_clipboard` goes through `document.execCommand('copy')`
 * on a throwaway off-screen textarea (so it works inside the gateway's
 * sandboxed iframe), which is not readable via the Clipboard API. Patching
 * `execCommand` lets us assert the COPIED TEXT rather than just the button's
 * label — a button wired to the wrong field would still say "Copied!".
 */
async function captureClipboardWrites(page: Page) {
  await page.evaluate(() => {
    const w = window as unknown as { __clip: string[] };
    w.__clip = [];
    const orig = document.execCommand.bind(document);
    document.execCommand = function (cmd: string, ...rest: unknown[]) {
      if (cmd === "copy") {
        const active = document.activeElement as HTMLTextAreaElement | null;
        let copied = active && "value" in active ? active.value : "";
        if (!copied) {
          // `select()` is not guaranteed to focus, so fall back to the
          // helper's signature: an off-screen fixed-position textarea.
          const ta = Array.from(document.querySelectorAll("textarea")).find(
            (t) => t.style.left === "-9999px"
          );
          copied = ta?.value ?? "";
        }
        w.__clip.push(copied);
      }
      return (orig as (c: string, ...r: unknown[]) => boolean)(cmd, ...rest);
    } as typeof document.execCommand;
  });
}

function clipboardWrites(page: Page) {
  return page.evaluate(() => (window as unknown as { __clip: string[] }).__clip);
}

/** Text the browser would put on the clipboard for `Ctrl+C` on this input. */
function selectedText(input: Locator) {
  return input.evaluate((el: HTMLInputElement) =>
    el.value.substring(el.selectionStart ?? 0, el.selectionEnd ?? 0)
  );
}

async function clearSelection(input: Locator) {
  await input.evaluate((el: HTMLInputElement) => el.setSelectionRange(0, 0));
}

async function dragAcross(page: Page, input: Locator) {
  const box = await input.boundingBox();
  if (!box) throw new Error("input has no bounding box");
  const y = box.y + box.height / 2;
  await page.mouse.move(box.x + 8, y);
  await page.mouse.down();
  await page.mouse.move(box.x + box.width - 12, y, { steps: 12 });
  await page.mouse.up();
}

for (const field of [
  { testid: "room-public-key-input", label: "room public key" },
  { testid: "contract-id-input", label: "contract ID" },
]) {
  test.describe(`room-details ${field.label} is selectable`, () => {
    test.use({ viewport: { width: 1280, height: 800 } });

    test(`click-drag selects the ${field.label}`, async ({ page }) => {
      await page.goto("/");
      await waitForApp(page);
      await openRoomDetails(page);

      const input = page.getByTestId(field.testid);
      await expect(input).toBeVisible();
      const value = await input.inputValue();
      expect(value.length).toBeGreaterThan(0);

      await dragAcross(page, input);

      // Before the fix this was "" on Firefox.
      const selected = await selectedText(input);
      expect(selected.length).toBeGreaterThan(0);
      // The drag starts/ends a few px inside the field, so on a value wider
      // than the box it is a partial selection; either way it must be a real,
      // contiguous run of the value.
      expect(value).toContain(selected);
    });

    test(`double-click selects the ${field.label}`, async ({ page }) => {
      await page.goto("/");
      await waitForApp(page);
      await openRoomDetails(page);

      const input = page.getByTestId(field.testid);
      const value = await input.inputValue();

      await clearSelection(input);
      await input.dblclick();

      const selected = await selectedText(input);
      expect(selected.length).toBeGreaterThan(0);
      expect(value).toContain(selected);
    });

    test(`Ctrl+C copies a mouse selection of the ${field.label}`, async ({ page, browserName }) => {
      test.skip(browserName === "webkit", WEBKIT_KEYBOARD_COPY_SKIP);
      await page.goto("/");
      await waitForApp(page);
      await openRoomDetails(page);

      const input = page.getByTestId(field.testid);
      const value = await input.inputValue();

      // Record what the browser actually hands to the clipboard. Reading the
      // system clipboard needs per-browser permissions that Firefox does not
      // grant in Playwright, so observe the copy event instead: its default
      // payload is the field's current selection.
      await input.evaluate((el: HTMLInputElement) => {
        (window as unknown as { __copied?: string }).__copied = undefined;
        el.addEventListener("copy", () => {
          (window as unknown as { __copied?: string }).__copied = el.value.substring(
            el.selectionStart ?? 0,
            el.selectionEnd ?? 0
          );
        });
      });

      // Drive the selection with the MOUSE, which is what the bug broke.
      // Ctrl+A is deliberately not used here: keyboard select-all still works
      // under `user-select: all`, so a Ctrl+A-driven copy passes even on the
      // broken build and would make this test non-discriminating.
      await clearSelection(input);
      await input.dblclick();
      const selected = await selectedText(input);
      expect(selected.length).toBeGreaterThan(0);

      await page.keyboard.press("ControlOrMeta+c");
      await expect
        .poll(() => page.evaluate(() => (window as unknown as { __copied?: string }).__copied), {
          timeout: 2_000,
        })
        .toBe(selected);
      expect(value).toContain(selected);
    });

    test(`Ctrl+A then Ctrl+C copies the whole ${field.label}`, async ({ page, browserName }) => {
      test.skip(browserName === "webkit", WEBKIT_KEYBOARD_COPY_SKIP);
      await page.goto("/");
      await waitForApp(page);
      await openRoomDetails(page);

      const input = page.getByTestId(field.testid);
      const value = await input.inputValue();

      await input.evaluate((el: HTMLInputElement) => {
        (window as unknown as { __copied?: string }).__copied = undefined;
        el.addEventListener("copy", () => {
          (window as unknown as { __copied?: string }).__copied = el.value.substring(
            el.selectionStart ?? 0,
            el.selectionEnd ?? 0
          );
        });
      });

      await input.click();
      await page.keyboard.press("ControlOrMeta+a");
      expect(await selectedText(input)).toBe(value);

      await page.keyboard.press("ControlOrMeta+c");
      await expect
        .poll(() => page.evaluate(() => (window as unknown as { __copied?: string }).__copied), {
          timeout: 2_000,
        })
        .toBe(value);
    });

    test(`selecting the ${field.label} does not close the modal`, async ({ page }) => {
      await page.goto("/");
      await waitForApp(page);
      await openRoomDetails(page);

      const input = page.getByTestId(field.testid);
      await dragAcross(page, input);
      await input.dblclick();

      // Acceptance criterion: no surrounding click/modal/drag behaviour regresses.
      await expect(page.getByTestId("edit-room-modal")).toBeVisible();
    });
  });
}

test.describe("room-details copy buttons", () => {
  test.use({ viewport: { width: 1280, height: 800 } });

  for (const field of [
    {
      input: "room-public-key-input",
      button: "room-public-key-copy-button",
      other: "contract-id-input",
    },
    {
      input: "contract-id-input",
      button: "contract-id-copy-button",
      other: "room-public-key-input",
    },
  ]) {
    test(`${field.button} confirms the copy`, async ({ page }) => {
      await page.goto("/");
      await waitForApp(page);
      await openRoomDetails(page);

      const value = await page.getByTestId(field.input).inputValue();
      expect(value.length).toBeGreaterThan(0);

      const button = page.getByTestId(field.button);
      await expect(button).toBeVisible();
      await expect(button).toHaveText(/Copy/);

      await button.click();
      await expect(button).toHaveText(/Copied!/, { timeout: 2_000 });
    });

    test(`${field.button} copies THAT field's value, not another`, async ({ page }) => {
      await page.goto("/");
      await waitForApp(page);
      await openRoomDetails(page);

      const mine = await page.getByTestId(field.input).inputValue();
      const other = await page.getByTestId(field.other).inputValue();
      expect(mine.length).toBeGreaterThan(0);
      expect(mine).not.toBe(other);

      await captureClipboardWrites(page);
      await page.getByTestId(field.button).click();

      // Asserting the button says "Copied!" is NOT enough: it would say that
      // just the same if the two buttons' values were swapped.
      await expect.poll(() => clipboardWrites(page), { timeout: 2_000 }).toEqual([mine]);
    });
  }

  test("the copy feedback resets when the panel is closed and reopened", async ({ page }) => {
    await page.goto("/");
    await waitForApp(page);
    await openRoomDetails(page);

    const button = page.getByTestId("room-public-key-copy-button");
    await button.click();
    await expect(button).toHaveText(/Copied!/, { timeout: 2_000 });

    // Same contract the Export Identity copy button holds
    // (copy-clipboard-feedback.spec.ts): reopening must not show a stale
    // "Copied!" from a previous visit.
    await page.getByTestId("edit-room-close-button").click();
    await expect(page.getByTestId("edit-room-modal")).toHaveCount(0);

    await page.getByTitle("Room details").click();
    await expect(page.getByTestId("edit-room-modal")).toBeVisible({ timeout: 5_000 });
    await expect(page.getByTestId("room-public-key-copy-button")).toHaveText(/^Copy$/);
  });

  test("the room-details panel does not overflow horizontally", async ({ page }) => {
    await page.goto("/");
    await waitForApp(page);
    await openRoomDetails(page);

    // The copy buttons put the inputs in a flex row; `flex-1 min-w-0` must keep
    // a long base58 value from widening the modal.
    const modal = page.getByTestId("edit-room-modal");
    const overflow = await modal.evaluate(
      (el) => el.scrollWidth - el.clientWidth
    );
    expect(overflow).toBeLessThanOrEqual(1);
  });

  test("the panel still fits, with both copy buttons, at 320px", async ({ page }) => {
    await page.goto("/");
    await waitForApp(page);
    await openRoomDetails(page);

    // The narrow viewport is the whole point of this check: adding a button
    // beside each value is exactly what could overflow a small screen, and
    // `openRoomDetails` widens to desktop, so the other tests never see it.
    // 320px is the smallest width responsive-layout.spec.ts covers.
    await page.setViewportSize({ width: 320, height: 800 });
    const modal = page.getByTestId("edit-room-modal");
    await expect(modal).toBeVisible();

    for (const testid of ["room-public-key-copy-button", "contract-id-copy-button"]) {
      await expect(page.getByTestId(testid)).toBeVisible();
    }

    const overflow = await modal.evaluate((el) => el.scrollWidth - el.clientWidth);
    expect(overflow).toBeLessThanOrEqual(1);

    // The modal itself must not be pushed outside the viewport either.
    const docOverflow = await page.evaluate(
      () => document.documentElement.scrollWidth - document.documentElement.clientWidth
    );
    expect(docOverflow).toBeLessThanOrEqual(1);
  });
});
