import { test, expect, Page } from "@playwright/test";
import { committedRow, openRoomWithComposer, settle, submitMessage } from "./example-room";
import { callRiverTest } from "./river-test";

// A message posted while its room is still subscribing is committed to the
// room but not sent: the sync pass holds it until the subscription is
// confirmed. The message itself must read as pending, not posted, and switch
// to sent when the change goes out. The hook sets the room up as the invite
// path leaves it (registered with no baseline; its PUT sent with the room's
// state); the pass and the timers are covered by native tests in sync_info.rs.

const row = (page: Page, text: string) => committedRow(page, text);
const status = (page: Page, text: string) => row(page, text).getByTestId("held-message-status");

test("a message held for an unconfirmed subscription is marked pending, then sent", async ({ page }) => {
  await openRoomWithComposer(page);
  await callRiverTest(page, "holdRoomUpdates", false);
  await submitMessage(page, "posted before the subscription");

  const message = row(page, "posted before the subscription");
  await expect(message).toBeVisible();
  await expect(message, "the message itself is marked").toHaveAttribute("data-pending-state", "held");
  await expect(status(page, "posted before the subscription")).toHaveText("Sending");

  await settle(page);
  await expect(message, "it stays pending while it stays unsent").toHaveAttribute("data-pending-state", "held");

  await callRiverTest(page, "releaseRoomUpdates");
  await expect(status(page, "posted before the subscription"), "once sent, the mark is gone").toHaveCount(0);
  await expect(message).not.toHaveAttribute("data-pending-state", /.+/);
});

test("only the held messages are marked, and a room in error says not sent", async ({ page }) => {
  await openRoomWithComposer(page);
  await callRiverTest(page, "holdRoomUpdates", true);
  await submitMessage(page, "will not go out");

  await expect(row(page, "will not go out")).toHaveAttribute("data-pending-state", "failed");
  await expect(status(page, "will not go out")).toHaveText("Not sent");
  await expect(page.locator('[data-testid="message-row"][data-pending-state]'), "older messages are not marked").toHaveCount(1);
});
