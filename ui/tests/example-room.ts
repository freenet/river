import { expect, Locator, Page, test } from "@playwright/test";

export async function waitForApp(page: Page) {
  await page.waitForSelector(".app-root", { timeout: 30_000 });
  await expect(page.locator("aside, .app-root button")).not.toHaveCount(0);
  // App injects its stylesheets on first render; until they load every panel is visible and nothing is where it will be.
  await page.waitForFunction(
    () => {
      const links = Array.from(document.querySelectorAll<HTMLLinkElement>('link[rel="stylesheet"]'));
      return links.length > 0 && links.every((l) => l.sheet !== null);
    },
    undefined,
    { timeout: 30_000 }
  );
}

/// Two animation frames, so a patch's effects and the layout they cause land.
export function nextFrames(page: Page): Promise<void> {
  return page.evaluate(
    () => new Promise<void>((r) => requestAnimationFrame(() => requestAnimationFrame(() => r()))),
  );
}

/// Let a patch, its effects and any deferred mark land, so that a geometry or
/// unread count read afterwards is not just early.
export async function settle(page: Page) {
  await nextFrames(page);
  await page.waitForTimeout(300);
}

// Force the tab's visibility state: override the `document.hidden` and
// `document.visibilityState` getters and dispatch `visibilitychange`, the way
// Chromium, WebKit and Firefox do when the tab goes to the background or back.
export async function setTabVisibility(page: Page, state: "hidden" | "visible") {
  await page.evaluate((state) => {
    Object.defineProperty(document, "hidden", {
      configurable: true,
      get: () => state === "hidden",
    });
    Object.defineProperty(document, "visibilityState", {
      configurable: true,
      get: () => state,
    });
    document.dispatchEvent(new Event("visibilitychange"));
  }, state);
}

/// The (N) of the title while the tab is hidden: unread across every room and
/// DM thread, the open one included. Hides the tab if it is not hidden
/// already, and lets the hide's mark land first.
export async function hiddenTitleCount(page: Page): Promise<number> {
  if (await page.evaluate(() => document.visibilityState !== "hidden")) {
    await setTabVisibility(page, "hidden");
  }
  await settle(page);
  const counted = /^\((\d+)\) /.exec(await page.title());
  return counted ? Number(counted[1]) : 0;
}

// Scoped to the room list: once a room is open the header title has the same name.
export async function selectListedRoom(page: Page, roomName: string) {
  const roomBtn = page.getByTestId("room-list").getByRole("button", { name: roomName });
  await expect(roomBtn).toBeVisible({ timeout: 5_000 });
  await roomBtn.click();
  await expect(page.getByRole("heading", { name: roomName })).toBeVisible({ timeout: 5_000 });
}

// Any viewport: the list if it is on screen, else the hamburger, else widen for the click.
export async function selectRoom(page: Page, roomName: string) {
  const listed = page.getByTestId("room-list").getByRole("button", { name: roomName });
  if (await listed.isVisible()) return selectListedRoom(page, roomName);

  const hamburger = page.getByTestId("hamburger-rooms-button").filter({ visible: true });
  if ((await hamburger.count()) > 0) {
    await hamburger.click();
    return selectListedRoom(page, roomName);
  }

  const vp = page.viewportSize();
  if (!vp) throw new Error("selectRoom needs a fixed viewport");
  await page.setViewportSize({ width: 1280, height: vp.height });
  await selectListedRoom(page, roomName);
  await page.setViewportSize(vp);
  // Let layout settle after the resize round trip (mobile-safari measured too early).
  await nextFrames(page);
}

/// A room's unread badge in the room list. Absent for the current room while
/// the chat shows, so callers open another room first or hide the chat.
export function roomUnreadBadge(page: Page, roomName: string): Locator {
  return page.getByTestId("room-list").getByRole("button", { name: roomName }).getByTestId("room-unread-badge");
}

/// The unread badge on whichever hamburger is showing (room header or no-room panel).
export function hamburgerBadge(page: Page): Locator {
  return page.getByTestId("hamburger-rooms-button").filter({ visible: true }).getByTestId("hamburger-unread-badge");
}

export function memberRows(page: Page): Locator {
  return page.getByTestId("member-list").locator('[data-testid^="member-item-"] button');
}

// Opens the app at "/" and the room self owns, "Your Private Room" (in
// "Public Discussion Room" self is an observer with no composer).
export async function openRoomWithComposer(page: Page) {
  await page.goto("/");
  await waitForApp(page);
  await selectRoom(page, "Your Private Room");
  await expect(page.getByTestId("message-composer")).toBeVisible({ timeout: 5_000 });
}

/// The signed message in the room history holding `text` (a pending send is
/// a `pending-message`, not one of these).
export function committedRow(page: Page, text: string): Locator {
  return page.getByTestId("conversation-history").getByTestId("message-row").filter({ hasText: text });
}

/// Type `text` into the composer and press Enter. Returns the row it commits
/// to; the caller waits for whichever state it is testing (pending, failed or
/// committed).
export async function submitMessage(page: Page, text: string): Promise<Locator> {
  const input = page.getByTestId("message-input");
  await input.fill(text);
  await input.press("Enter");
  return committedRow(page, text);
}

/// Focus `target` as a keyboard user would, so it is `:focus-visible`. A key
/// press first, so a scripted focus counts as keyboard focus (WebKit's Tab
/// skips buttons).
export async function keyboardFocus(page: Page, target: Locator) {
  await page.keyboard.press("Escape");
  await target.focus();
  if (!(await target.evaluate((el) => el.matches(":focus-visible")))) {
    await page.keyboard.press("Shift+Tab");
    await page.keyboard.press("Tab");
  }
  await expect
    .poll(() => target.evaluate((el) => el === document.activeElement && el.matches(":focus-visible")), {
      message: "premise: keyboard focus",
    })
    .toBe(true);
}

/// A signed message's footer controls, in footer order. Edit and Delete are
/// on your own messages only.
export const FOOTER_CONTROLS = ["message-react-button", "message-reply-button", "message-edit-button", "message-delete-button"];

// The edit form on an own message, through its footer Edit. Returns the edit
// textarea. Without a `message`, the first own message is scrolled into view
// first; a given `message` is used from wherever it already is.
export async function openOwnMessageEdit(page: Page, message?: Locator): Promise<Locator> {
  const ownRow = message ?? page.locator('[id^="msg-"][data-message-self="true"]').first();
  await expect(ownRow).toBeVisible();
  if (!message) await ownRow.scrollIntoViewIfNeeded();
  await clickFooter(page, ownRow, "message-edit-button");
  const editArea = page.locator('textarea[id^="edit-msg-"]');
  await expect(editArea).toBeVisible({ timeout: 5_000 });
  return editArea;
}

// Any CSS colour (tokens, color-mix, oklab) as sRGB [r, g, b, a], so engines compare equal.
export function resolveColor(page: Page, css: string): Promise<number[]> {
  return page.evaluate((value) => {
    const probe = document.createElement("div");
    probe.style.color = value;
    document.body.appendChild(probe);
    const computed = getComputedStyle(probe).color;
    probe.remove();
    const ctx = Object.assign(document.createElement("canvas"), { width: 1, height: 1 })
      .getContext("2d", { willReadFrequently: true })!;
    ctx.fillStyle = computed;
    ctx.fillRect(0, 0, 1, 1);
    const [r, g, b, a] = ctx.getImageData(0, 0, 1, 1).data;
    return [r, g, b, a / 255];
  }, css);
}

/// WCAG 2.x relative luminance of an 8-bit sRGB colour (alpha ignored).
function luminance(c: number[]): number {
  const [r, g, b] = c.slice(0, 3).map((v) => {
    const s = v / 255;
    return s <= 0.04045 ? s / 12.92 : ((s + 0.055) / 1.055) ** 2.4;
  });
  return 0.2126 * r + 0.7152 * g + 0.0722 * b;
}

/// WCAG contrast ratio of two 8-bit sRGB colours, in either order.
export function contrast(a: number[], b: number[]): number {
  const [hi, lo] = [luminance(a), luminance(b)].sort((x, y) => y - x);
  return (hi + 0.05) / (lo + 0.05);
}

/// `fg` (an [r, g, b, a] from `resolveColor`) composited over an opaque `bg`.
export function over(fg: number[], bg: number[]): number[] {
  const a = fg[3] ?? 1;
  return [0, 1, 2].map((i) => fg[i] * a + bg[i] * (1 - a)).concat(1);
}

/// The opaque colour painted behind `el`: every ancestor's background (and its
/// own), composited from the page down, since some surfaces are translucent.
export async function effectiveBackground(el: Locator): Promise<number[]> {
  const layers = await el.evaluate((e) => {
    const out: string[] = [];
    for (let n: Element | null = e; n; n = n.parentElement) out.push(getComputedStyle(n).backgroundColor);
    return out.reverse();
  });
  let base = [255, 255, 255, 1];
  for (const css of layers) base = over(await resolveColor(el.page(), css), base);
  return base;
}

/// The element's computed text colour (or, given "backgroundColor", its own
/// background), as `resolveColor` returns it.
export async function colourOf(locator: Locator, property: "color" | "backgroundColor" = "color"): Promise<number[]> {
  return resolveColor(locator.page(), await locator.evaluate((el, p) => getComputedStyle(el)[p], property));
}

/// Whether a finger can be the pointer: the same query as main.css. Message
/// footer controls then show at rest; otherwise only on the row's hover or
/// keyboard focus.
export function coarsePointer(page: Page): Promise<boolean> {
  return page.evaluate(() => window.matchMedia("(hover: none), (any-pointer: coarse)").matches);
}

/// Show `row`'s footer controls (React, Reply, Edit, Delete): a fine pointer
/// hovers the row, as a reader does; a touch or hybrid pointer already shows
/// them.
export async function revealFooter(page: Page, row: Locator) {
  if (!(await coarsePointer(page))) await row.hover({ position: { x: 4, y: 4 } });
}

/// Click one of `row`'s footer controls by its test id, revealing it first.
/// Below 48rem Reply, Edit and Delete are not in the footer: a tap on the
/// row opens them in the actions sheet, and this goes that way.
export async function clickFooter(page: Page, row: Locator, testid: string) {
  const sheetAction = SHEET_ACTIONS[testid];
  if (sheetAction && (await narrowLayout(page))) {
    await openMessageActions(page, row);
    await press(page, page.getByTestId(sheetAction));
    await expect(page.getByTestId("mobile-message-actions-sheet")).toBeHidden();
    return;
  }
  await revealFooter(page, row);
  await row.getByTestId(testid).click();
}

/// Open `row`'s reaction choices through its footer React, the only way in:
/// the anchored picker at 48rem and wider; below it, the reaction popup with
/// its palette shown.
export async function openInlinePicker(page: Page, row: Locator) {
  if (await narrowLayout(page)) {
    await press(page, row.getByTestId("message-react-button"));
    await expect(page.getByTestId("mobile-reaction-popup")).toBeVisible();
    await press(page, page.getByTestId("mobile-reaction-palette-toggle"));
    await expect(page.getByTestId("mobile-reaction-palette")).toBeVisible();
    return;
  }
  await clickFooter(page, row, "message-react-button");
}

/// The emoji buttons of whichever reaction choices `openInlinePicker` opened.
export async function reactionChoices(page: Page): Promise<Locator> {
  return (await narrowLayout(page))
    ? page.getByTestId("mobile-reaction-palette").getByRole("button")
    : page.getByTestId("emoji-picker").getByRole("button");
}

/// React to `row` with the picker's first emoji. Expects the row to end with
/// exactly one reaction chip, so pass a row that has none.
export async function reactTo(page: Page, row: Locator): Promise<Locator> {
  await openInlinePicker(page, row);
  await press(page, (await reactionChoices(page)).first());
  const chip = row.getByTestId("reaction-chip");
  await expect(chip).toHaveCount(1);
  return chip;
}

/// Below Tailwind's `md` (48rem), the query `main.css` and the app use: a
/// row's Reply, Edit and Delete are in its actions sheet, its + opens the
/// reaction popup.
export function narrowLayout(page: Page): Promise<boolean> {
  return page.evaluate(() => window.matchMedia("not all and (min-width: 48rem)").matches);
}

/// A phone project's test of the desktop footer or the anchored reaction
/// picker runs at a touch tablet's width: below 48rem a row's actions and
/// reactions are the mobile sheet and popup instead. Keeps the height, and
/// the device's touch input, so the wide coarse-pointer case stays covered.
export async function asTouchTablet(page: Page) {
  const vp = page.viewportSize();
  if (vp && vp.width < 768) await page.setViewportSize({ width: 834, height: vp.height });
}

/// Whether this test's context has touch input, so `tap()` works: the
/// project's device, whatever width a test sets.
export async function hasTouch(_page: Page): Promise<boolean> {
  return test.info().project.use.hasTouch === true;
}

/// Press `target` as the reader would: a tap where touch is available, else
/// a click.
export async function press(page: Page, target: Locator, options?: { position?: { x: number; y: number } }) {
  if (await hasTouch(page)) await target.tap(options);
  else await target.click(options);
}

/// Footer test ids and the sheet action that replaces each below 48rem.
const SHEET_ACTIONS: Record<string, string> = {
  "message-reply-button": "mobile-message-action-reply",
  "message-edit-button": "mobile-message-action-edit",
  "message-delete-button": "mobile-message-action-delete",
};

/// A point on `row` where a tap lands on the row itself: inside it, on
/// nothing with a behaviour of its own (links, buttons, chips, the avatar).
export async function plainPointOf(row: Locator): Promise<{ x: number; y: number }> {
  await row.evaluate((el) => {
    const r = el.getBoundingClientRect();
    const view = document.getElementById("chat-scroll-container")!.getBoundingClientRect();
    if (r.top < view.top || r.bottom > view.bottom) el.scrollIntoView({ block: "center" });
  });
  const find = () => row.evaluate((el) => {
    const interactive =
      'a, button, input, textarea, [role="button"], [role="link"], [tabindex]:not([tabindex="-1"]), [data-row-actions="exclude"], [data-member-id]';
    const content = el.querySelector('[data-testid="message-content"]')!.getBoundingClientRect();
    const body = el.querySelector('[data-testid="message-body"]')?.getBoundingClientRect() ?? content;
    const candidates: [number, number][] = [];
    // The body's lines, then the empty end of the content column, then its edges.
    for (const fy of [0.5, 0.25, 0.75, 0.1, 0.9]) {
      for (const fx of [0.98, 0.9, 0.75, 0.5, 0.25, 0.05]) {
        candidates.push([body.left + body.width * fx, body.top + body.height * fy]);
      }
    }
    for (const fy of [0.05, 0.5, 0.95]) candidates.push([content.right - 3, content.top + content.height * fy]);
    for (const [x, y] of candidates) {
      const at = document.elementFromPoint(x, y);
      if (!at || !el.contains(at)) continue;
      const control = at.closest(interactive);
      if (control && el.contains(control)) continue;
      return { x, y };
    }
    return null;
  });
  // Still: a row that just committed, or a history still following, can
  // move between measuring the point and pressing it.
  await expect
    .poll(
      () =>
        row.evaluate(
          (el) =>
            new Promise<boolean>((resolve) => {
              const a = el.getBoundingClientRect().top;
              requestAnimationFrame(() => requestAnimationFrame(() => resolve(el.getBoundingClientRect().top === a)));
            }),
        ),
      { message: "the row holds still", timeout: 5_000 },
    )
    .toBe(true);
  // Polled: a dialog closing a moment ago may still cover it.
  let point: { x: number; y: number } | null = null;
  await expect.poll(async () => (point = await find()) !== null, { message: "a plain point on the row to tap", timeout: 5_000 }).toBe(true);
  return point!;
}

/// Tap (or click) `row` on a plain point, as a reader opening its actions.
export async function tapRow(page: Page, row: Locator) {
  const { x, y } = await plainPointOf(row);
  if (await hasTouch(page)) await page.touchscreen.tap(x, y);
  else await page.mouse.click(x, y);
}

/// Below 48rem: open `row`'s actions sheet with a tap on the row, and check
/// it is for that row.
export async function openMessageActions(page: Page, row: Locator) {
  await tapRow(page, row);
  const sheet = page.getByTestId("mobile-message-actions-sheet");
  await expect(sheet).toBeVisible();
  const id = (await row.getAttribute("id"))!.replace(/^msg-/, "");
  await expect(sheet).toHaveAttribute("data-target-row", id);
}
