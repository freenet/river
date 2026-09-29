import { test, expect, Page } from "@playwright/test";
import { openRoomWithComposer, waitForApp } from "./example-room";

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
