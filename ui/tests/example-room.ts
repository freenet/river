import { CDPSession, expect, Locator, Page, test } from "@playwright/test";
import { callRiverTest } from "./river-test";

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

/// `openRoomWithComposer`, then a received message with no reactions at the
/// end of the history. Returns its row.
export async function openRoomWithTarget(page: Page, text = "react target"): Promise<Locator> {
  await openRoomWithComposer(page);
  await callRiverTest(page, "appendMessage", text);
  const row = committedRow(page, text);
  await expect(row).toBeVisible({ timeout: 5_000 });
  return row;
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

/// Whether a row with no reactions keeps no footer line: a desktop-width
/// window with a mouse puts its controls at the end of its last line instead
/// (main.css, "Controls at the end of the last line").
export async function inlineFooter(page: Page): Promise<boolean> {
  const wide = await page.evaluate(() => window.matchMedia("(min-width: 48rem)").matches);
  return wide && !(await coarsePointer(page));
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
    await expect(sheet(page)).toBeHidden();
    return;
  }
  await revealFooter(page, row);
  await row.getByTestId(testid).click();
}

/// Open `row`'s reaction choices through its footer React, the only way in:
/// the anchored picker at 48rem and wider; below it, the reaction popup and
/// its row of choices.
export async function openInlinePicker(page: Page, row: Locator) {
  if (await narrowLayout(page)) {
    await openSheetFromReact(page, row);
    await expect(palette(page)).toBeVisible();
    return;
  }
  await clickFooter(page, row, "message-react-button");
}

/// The emoji buttons of whichever reaction choices `openInlinePicker` opened.
export async function reactionChoices(page: Page): Promise<Locator> {
  return (await narrowLayout(page))
    ? palette(page).getByRole("button")
    : page.getByTestId("emoji-picker").getByRole("button");
}

/// React to `row` with the picker's first emoji. Expects the row to end with
/// exactly one reaction chip, so pass a row that has none.
export async function reactTo(page: Page, row: Locator): Promise<Locator> {
  await openInlinePicker(page, row);
  await press(page, (await reactionChoices(page)).first());
  const chip = chips(row);
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
export function hasTouch(): boolean {
  return test.info().project.use.hasTouch === true;
}

/// Press `target` as the reader would: a tap where touch is available, else
/// a click.
export async function press(page: Page, target: Locator, options?: { position?: { x: number; y: number } }) {
  if (hasTouch()) await target.tap(options);
  else await target.click(options);
}

/// Press the page at a point as the reader would: a tap where touch is
/// available, else a click.
export async function pressAt(page: Page, { x, y }: { x: number; y: number }) {
  if (hasTouch()) await page.touchscreen.tap(x, y);
  else await page.mouse.click(x, y);
}

/// A pointer down on `chip`, held past the app's hold time (CHIP_HOLD_MS,
/// 500). Returns the lift, which also sends the click a browser sends when
/// the pointer lifts. Dispatched events: outside Chromium's input protocol
/// Playwright has no touch hold, and the app decides a hold on the
/// `pointerdown`'s type and a timer alone.
export async function holdChip(page: Page, chip: Locator): Promise<() => Promise<void>> {
  const fire = (type: string) =>
    chip.evaluate((el, type) => {
      const r = el.getBoundingClientRect();
      el.dispatchEvent(
        new PointerEvent(type, {
          bubbles: true,
          cancelable: true,
          composed: true,
          pointerId: 7,
          pointerType: "touch",
          isPrimary: true,
          clientX: r.left + r.width / 2,
          clientY: r.top + r.height / 2,
        }),
      );
    }, type);
  await chip.scrollIntoViewIfNeeded();
  await fire("pointerdown");
  await page.waitForTimeout(700);
  return async () => {
    await fire("pointerup");
    await chip.evaluate((el) => (el as HTMLElement).click());
  };
}

/// The mobile message sheet's id and test id: the one dialog a row tap, a
/// row's + or its keyboard entry opens below 48rem.
export const SHEET = "mobile-message-sheet";

export function sheet(page: Page): Locator {
  return page.getByTestId(SHEET);
}

/// The sheet's row of emoji choices.
export function palette(page: Page): Locator {
  return page.getByTestId("mobile-reaction-palette");
}

/// `row`'s reaction chips.
export function chips(row: Locator): Locator {
  return row.getByTestId("reaction-chip");
}

/// The message key a row's id carries (`msg-<key>`), as the sheet's
/// `data-target-row` names it.
export async function rowKey(row: Locator): Promise<string> {
  return (await row.getAttribute("id"))!.replace(/^msg-/, "");
}

/// The middle of `target`'s box, in whole px: where a finger or the mouse
/// goes down on it.
export async function centreOf(target: Locator): Promise<[number, number]> {
  const box = (await target.boundingBox())!;
  return [Math.round(box.x + box.width / 2), Math.round(box.y + box.height / 2)];
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
    // Hover can reveal inline controls between measuring and pressing.
    const controlBoxes = Array.from(el.querySelectorAll(interactive)).map((e) => e.getBoundingClientRect());
    const candidates: [number, number][] = [];
    // The body's lines, then the empty end of the content column, then its edges.
    for (const fy of [0.5, 0.25, 0.75, 0.1, 0.9]) {
      for (const fx of [0.98, 0.9, 0.75, 0.5, 0.25, 0.05]) {
        candidates.push([body.left + body.width * fx, body.top + body.height * fy]);
      }
    }
    for (const fy of [0.05, 0.5, 0.95]) candidates.push([content.right - 3, content.top + content.height * fy]);
    for (const [x, y] of candidates) {
      if (controlBoxes.some((r) => x >= r.left && x < r.right && y >= r.top && y < r.bottom)) continue;
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
  await pressAt(page, await plainPointOf(row));
}

/// The sheet is open for `row` and its opening slide has ended: until then
/// the panel is partly below the screen, so a point measured in it is not
/// where it will be, or is on nothing.
export async function expectSheetOpenFor(page: Page, row: Locator) {
  await expect(sheet(page)).toBeVisible();
  await expect(sheet(page)).toHaveAttribute("data-target-row", await rowKey(row));
  await panelAtRest(page);
}

/// Below 48rem: open `row`'s sheet with a tap on the row, and wait until it
/// is open for that row and at rest.
export async function openMessageActions(page: Page, row: Locator) {
  await tapRow(page, row);
  await expectSheetOpenFor(page, row);
}

/// Below 48rem: open `row`'s sheet with its +, and wait until it is open
/// for that row and at rest.
export async function openSheetFromReact(page: Page, row: Locator) {
  await press(page, row.getByTestId("message-react-button"));
  await expectSheetOpenFor(page, row);
}

/// A point over the history's top, below the header.
export async function historyPoint(page: Page): Promise<{ x: number; y: number }> {
  const view = (await page.locator("#chat-scroll-container").boundingBox())!;
  return { x: view.x + 40, y: view.y + 30 };
}

const PANEL = `#${SHEET} .mobile-sheet-panel`;

/// How far the open sheet's panel sits below its rest position now, in px
/// (its computed `transform`; the opening slide's `translate` is not
/// counted).
export function panelOffset(page: Page): Promise<number> {
  return page.evaluate((panel) => new DOMMatrix(getComputedStyle(document.querySelector(panel)!).transform).m42, PANEL);
}

/// How far the open sheet's panel sits above the bottom of the viewport, in
/// px (0 at rest; the opening slide counts).
export function panelBottomGap(page: Page): Promise<number> {
  return page.evaluate((panel) => window.innerHeight - document.querySelector(panel)!.getBoundingClientRect().bottom, PANEL);
}

/// Wait until the open sheet's panel has no slide or transition running.
export async function panelAtRest(page: Page) {
  await page.waitForFunction(
    (panel) => document.querySelector(panel)?.getAnimations().every((a) => a.playState !== "running") ?? false,
    PANEL,
  );
}

/// A point inside the open sheet's panel, on no control.
async function panelPoint(page: Page): Promise<{ x: number; y: number }> {
  const box = (await page.locator(PANEL).boundingBox())!;
  return { x: box.x + box.width / 2, y: box.y + 6 };
}

/// Whether a one- (or two-) finger `touchmove` at the point is cancelled,
/// dispatched on whatever is there, as the browser would.
export function touchmovePrevented(page: Page, x: number, y: number, fingers = 1): Promise<boolean> {
  return page.evaluate(
    ({ x, y, fingers }) => {
      const target = document.elementFromPoint(x, y)!;
      const e = new Event("touchmove", { bubbles: true, cancelable: true });
      Object.defineProperty(e, "touches", { value: { length: fingers } });
      target.dispatchEvent(e);
      return e.defaultPrevented;
    },
    { x, y, fingers },
  );
}

/// Whether a synthetic, cancelable wheel dispatched where (x, y) lands was
/// cancelled: the hold's handler contract, not the browser's scroll or zoom.
export function wheelCancelled(page: Page, x: number, y: number, ctrlKey: boolean): Promise<boolean> {
  return page.evaluate(
    ([x, y, ctrlKey]) => {
      const at = document.elementFromPoint(x, y)!;
      const wheel = new WheelEvent("wheel", { deltaY: -100, clientX: x, clientY: y, ctrlKey, bubbles: true, cancelable: true });
      return !at.dispatchEvent(wheel);
    },
    [x, y, ctrlKey] as const,
  );
}

/// For each `[how, close]`: open the sheet with `open`, check that a
/// one-finger touch over the history is held while a pinch and a touch on
/// its panel are not, close it with `close`, and check the hold let go.
export async function expectTouchHeldUntilClosed(
  page: Page,
  open: () => Promise<void>,
  closes: [string, () => Promise<void>][],
) {
  for (const [how, close] of closes) {
    await open();
    const h = await historyPoint(page);
    expect(await touchmovePrevented(page, h.x, h.y), `${how}: a finger over the history is held`).toBe(true);
    expect(await touchmovePrevented(page, h.x, h.y, 2), `${how}: a pinch is not`).toBe(false);
    const inside = await panelPoint(page);
    expect(await touchmovePrevented(page, inside.x, inside.y), `${how}: the panel scrolls`).toBe(false);
    expect(await touchmovePrevented(page, inside.x, inside.y, 2), `${how}: and a pinch on it zooms`).toBe(false);
    await close();
    await expect(sheet(page)).toBeHidden();
    await settle(page);
    const after = await historyPoint(page);
    expect(await page.evaluate(({ x, y }) => !!document.elementFromPoint(x, y)?.closest("#chat-scroll-container"), after), "premise: the history is under the point").toBe(true);
    expect(await touchmovePrevented(page, after.x, after.y), `closed by ${how}: let go`).toBe(false);
  }
}

/// How far a test finger drags by default.
export const DRAG_PX = 80;

type Finger = { x: number; y: number };

/// Fingers on the screen, through Chromium's input protocol: the browser
/// hit-tests them, fires pointer and touch events, captures and scrolls as it
/// does for a real finger. Spelled out as touchStart, touchMove a frame apart,
/// touchEnd: `Input.synthesizeScrollGesture` with a touch source fires no
/// touchmove and scrolls nothing in Linux headless Chromium (CI), picker open
/// or not.
export class Fingers {
  private down = new Map<number, Finger>();
  constructor(readonly cdp: CDPSession) {}

  private send(type: "touchStart" | "touchMove" | "touchEnd" | "touchCancel") {
    const touchPoints = type === "touchEnd" || type === "touchCancel" ? [] : [...this.down].map(([id, p]) => ({ id, x: p.x, y: p.y }));
    return this.cdp.send("Input.dispatchTouchEvent", { type, touchPoints });
  }

  get anyDown(): boolean {
    return this.down.size > 0;
  }

  /// Put fingers down, together.
  async press(...fingers: [number, number, number][]) {
    for (const [id, x, y] of fingers) this.down.set(id, { x, y });
    await this.send("touchStart");
  }

  /// Move each finger by its own distance, vertical alone or `[dx, dy]`, in
  /// steps a frame apart.
  async move(moves: Record<number, number | [number, number]>, steps = 10) {
    const from = new Map([...this.down].map(([id, p]) => [id, { ...p }]));
    for (let i = 1; i <= steps; i++) {
      for (const [id, by] of Object.entries(moves)) {
        const [dx, dy] = typeof by === "number" ? [0, by] : by;
        const [f, at] = [from.get(Number(id))!, this.down.get(Number(id))!];
        at.x = f.x + (dx * i) / steps;
        at.y = f.y + (dy * i) / steps;
      }
      await this.send("touchMove");
      await new Promise((r) => setTimeout(r, 16));
    }
  }

  /// Every finger lifts.
  async lift() {
    this.down.clear();
    await this.send("touchEnd");
  }

  /// The browser cancels every finger.
  async cancel() {
    this.down.clear();
    await this.send("touchCancel");
  }

  /// One finger, down at (x, y), dragged `dy` and lifted.
  async drag(x: number, y: number, dy = DRAG_PX) {
    await this.press([1, x, y]);
    await this.move({ 1: dy });
    await this.lift();
  }
}

/// Run `body` with fingers on `page`, every one lifted and the session
/// detached afterwards, whatever happens.
export async function withFingers(page: Page, body: (fingers: Fingers) => Promise<void>) {
  const cdp = await page.context().newCDPSession(page);
  const fingers = new Fingers(cdp);
  try {
    await body(fingers);
  } finally {
    if (fingers.anyDown) await fingers.cancel().catch(() => {});
    await cdp.detach();
  }
}
