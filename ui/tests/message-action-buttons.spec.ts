import { test, expect, Page } from "@playwright/test";
import { openRoomWithComposer, resolveColor, waitForApp } from "./example-room";

// The reply, edit and delete buttons under each bubble
// (ui/src/components/conversation.rs, `message-action-cluster`):
//   [chips][+] ...... [reply][edit][delete] 12:04
// Others' rows offer reply only; your own rows offer all three. The time stays
// the row's last item, outside the cluster.

test.beforeEach(async ({ page }) => {
  await page.goto("/");
  await waitForApp(page);
  await openRoomWithComposer(page);
});

const own = (page: Page) => page.locator('[data-self="true"] [id^="msg-"]').first();
const received = (page: Page) => page.locator('[data-self="false"] [id^="msg-"]').first();
const isCoarse = (page: Page) =>
  page.evaluate(() => matchMedia("(hover: none), (any-pointer: coarse)").matches);

test(
  "own rows offer reply, edit and delete; received rows only reply",
  { tag: "@chromium-only" },
  async ({ page }) => {
    const ownRow = own(page);
    const receivedRow = received(page);
    await expect(ownRow).toBeAttached();
    await expect(receivedRow).toBeAttached();

    await expect(ownRow.getByTestId("message-reply-button")).toHaveAttribute("aria-label", "Reply");
    await expect(ownRow.getByTestId("message-edit-button")).toHaveAttribute("aria-label", "Edit message");
    await expect(ownRow.getByTestId("message-delete-button")).toHaveAttribute("aria-label", "Delete message");

    await expect(receivedRow.getByTestId("message-reply-button")).toHaveAttribute("aria-label", "Reply");
    await expect(receivedRow.getByTestId("message-edit-button")).toHaveCount(0);
    await expect(receivedRow.getByTestId("message-delete-button")).toHaveCount(0);

    for (const row of [ownRow, receivedRow]) {
      const cluster = row.getByTestId("message-action-cluster");
      await expect(cluster).toHaveCount(1);
      await expect(cluster.getByTestId("message-reply-button")).toHaveCount(1);
      // The time is the row's own, to the right of the cluster, and never inside it.
      await expect(cluster.getByTestId("message-time")).toHaveCount(0);
      const time = row.getByTestId("message-time");
      await expect(time).toHaveCount(1);
      const c = (await cluster.boundingBox())!;
      const t = (await time.boundingBox())!;
      expect(t.x).toBeGreaterThanOrEqual(c.x + c.width - 1);
    }
  }
);

test("delete asks for confirmation", { tag: "@chromium-only" }, async ({ page }) => {
  const row = own(page);
  await row.scrollIntoViewIfNeeded();
  const id = await row.getAttribute("id");
  await row.getByTestId("message-delete-button").click();
  await expect(page.getByRole("heading", { name: "Delete Message?" })).toBeVisible();
  await page.getByRole("button", { name: "Cancel" }).click();
  await expect(page.getByRole("heading", { name: "Delete Message?" })).toBeHidden();
  // `own` is `.first()`, so it would match the next own row after a deletion: pin this row by id.
  await expect(page.locator(`[id="${id}"]`)).toHaveCount(1);
});

test("reply opens the composer's reply preview", { tag: "@chromium-only" }, async ({ page }) => {
  const row = received(page);
  await row.scrollIntoViewIfNeeded();
  await expect(page.getByTitle("Cancel reply")).toHaveCount(0); // premise: no reply pending yet
  await row.getByTestId("message-reply-button").click();
  // The cancel button's text is "\u00d7", so its title is the only "Cancel reply" there is.
  await expect(page.getByTitle("Cancel reply")).toBeVisible();
});

test("the buttons stay hidden until their row is hovered or holds keyboard focus", async ({
  page,
  browserName,
}) => {
  test.skip(await isCoarse(page), "always shown on touch");
  const row = own(page);
  await row.scrollIntoViewIfNeeded();
  const op = (id: string) =>
    row.getByTestId(id).evaluate((el) => +getComputedStyle(el).opacity);

  await page.mouse.move(0, 0);
  await expect.poll(() => op("message-reply-button")).toBe(0);

  await row.getByTestId("message-bubble").hover();
  await expect.poll(() => op("message-reply-button")).toBeGreaterThan(0);

  // Delete turns red on hover. An unlayered `color` rule for `.msg-action-btn` would beat
  // Tailwind's `hover:text-red-500` and keep it grey (the old branch's B2).
  expect(
    await page.evaluate(() => getComputedStyle(document.documentElement).getPropertyValue("--color-red-500").trim()),
    "premise: the red token is emitted"
  ).not.toBe("");
  const del = row.getByTestId("message-delete-button");
  await del.hover();
  const red = await resolveColor(page, "var(--color-red-500)");
  await expect
    .poll(async () => resolveColor(page, await del.evaluate((el) => getComputedStyle(el).color)))
    .toEqual(red);

  // WebKit keeps buttons out of the plain Tab order (they need Option+Tab).
  if (browserName === "webkit") return;

  // Keyboard focus reveals the whole row, not just the focused button.
  await page.mouse.move(0, 0);
  await page.getByTestId("message-input").focus();
  let focused = "";
  for (let i = 0; i < 300 && !/^message-(reply|edit|delete)-button$/.test(focused); i++) {
    await page.keyboard.press("Shift+Tab");
    focused = await page.evaluate(
      () => (document.activeElement as HTMLElement | null)?.dataset.testid ?? ""
    );
  }
  expect(focused, "Shift+Tab from the composer reaches a message button").toMatch(
    /^message-(reply|edit|delete)-button$/
  );
  // The first one reached is the last row's last button, so its reply is not the focused one.
  const focusedRow = page.locator('[id^="msg-"]:has(:focus)');
  await expect
    .poll(() =>
      focusedRow.getByTestId("message-reply-button").evaluate((el) => +getComputedStyle(el).opacity)
    )
    .toBeGreaterThanOrEqual(0.4);
});

test("on touch, a tap just beside an icon lands on its button", async ({ page }) => {
  test.skip(!(await isCoarse(page)), "touch only");
  const row = own(page);
  await row.scrollIntoViewIfNeeded();
  for (const id of ["message-reply-button", "message-edit-button", "message-delete-button"]) {
    const hits = await row.getByTestId(id).evaluate((btn) => {
      const r = btn.getBoundingClientRect();
      const cx = r.left + r.width / 2;
      const cy = r.top + r.height / 2;
      const lands = (x: number, y: number) =>
        document.elementFromPoint(x, y)?.closest('[data-testid="' + (btn as HTMLElement).dataset.testid + '"]') === btn;
      return {
        left: lands(r.left - 4, cy),
        right: lands(r.right + 4, cy),
        above: lands(cx, r.top - 3),
        below: lands(cx, r.bottom + 3),
      };
    });
    expect(hits, id).toEqual({ left: true, right: true, above: true, below: true });
  }
});
