import { test, expect, Page } from "@playwright/test";
import { committedRow, openRoomWithComposer, settle, submitMessage } from "./example-room";
import { callRiverTest } from "./river-test";

// A message posted while its room is still subscribing is committed to the
// room but not sent: the sync pass holds it until the subscription is
// confirmed. The conversation must say so, not show it as posted, and drop the
// notice when the change goes out. The hold itself is exercised by the native
// tests in sync_info.rs; here a hook puts the room in that state.

const notice = (page: Page) => page.getByTestId("held-message-notice");

test("a message held for an unconfirmed subscription is shown as not sent, then sent", async ({ page }) => {
  await openRoomWithComposer(page);
  await expect(notice(page), "nothing held yet").toHaveCount(0);

  await callRiverTest(page, "holdRoomUpdates");
  await submitMessage(page, "posted before the subscription");
  await expect(committedRow(page, "posted before the subscription")).toBeVisible();

  await expect(notice(page), "the unsent message is marked pending").toBeVisible();
  await expect(notice(page)).toHaveAttribute("data-pending-state", "held");
  await expect(notice(page)).toHaveAttribute("data-held-count", "1");
  await expect(page.getByTestId("held-message-status")).toContainText("not been sent");

  await settle(page);
  await expect(notice(page), "it stays pending while it stays unsent").toBeVisible();

  await callRiverTest(page, "releaseRoomUpdates");
  await expect(notice(page), "once sent, the notice is gone").toHaveCount(0);
  await expect(committedRow(page, "posted before the subscription")).toBeVisible();
});

test("a second held message is counted, and nothing shows for a room with nothing held", async ({ page }) => {
  await openRoomWithComposer(page);
  await callRiverTest(page, "holdRoomUpdates");
  await submitMessage(page, "first held");
  await expect(notice(page)).toHaveAttribute("data-held-count", "1");
  await submitMessage(page, "second held");
  await expect(notice(page)).toHaveAttribute("data-held-count", "2");
  await callRiverTest(page, "releaseRoomUpdates");
  await expect(notice(page)).toHaveCount(0);
});
