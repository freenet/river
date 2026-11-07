import { test, expect, Page } from "@playwright/test";
import { committedRow, FOOTER_CONTROLS, openInlinePicker, openOwnMessageEdit, openRoomWithComposer, reactTo, settle, submitMessage } from "./example-room";
import { callRiverTest } from "./river-test";

// An own message is signed by the chat delegate, which can take seconds. It
// shows at once as a pending row and turns into the committed message when
// the signature arrives (freenet/river#512). With `no-sync` the delegate
// fails instantly and the local key signs, so these tests hold signing open
// through a test hook: without the hold they would only ever see the fast path.

const pending = (page: Page) => page.getByTestId("pending-message");

/// Submit `text` and wait for the composer to clear, so the next send starts
/// from an empty draft.
async function send(page: Page, text: string) {
  await submitMessage(page, text);
  await expect(page.getByTestId("message-input"), "the draft is cleared on Enter").toHaveValue("");
}

// Footer actions (React, Reply, Edit, Delete) act on a signed message. A
// row still being signed keeps "Sending…", a failed one Retry and Discard,
// and neither has a footer; once signed, the one message has its footer.
const FOOTER = ['[data-testid="message-footer"]', ...FOOTER_CONTROLS.map((id) => `[data-testid="${id}"]`)].join(", ");

test("a sent message shows at once, pending, and commits once when signed", async ({ page }) => {
  await openRoomWithComposer(page);
  await callRiverTest(page, "holdMessageSigning");
  await send(page, "pending until signed");

  const row = pending(page).filter({ hasText: "pending until signed" });
  await expect(row).toBeVisible();
  await expect(row).toHaveAttribute("data-pending-state", "sending");
  await expect(row.getByTestId("pending-message-status")).toHaveText("Sending…");
  await expect(row, "the pending row is in view").toBeInViewport();
  await expect(row.locator(FOOTER), "no footer while unsigned").toHaveCount(0);

  // Held: nothing reaches the room however long it takes.
  await settle(page);
  await expect(committedRow(page, "pending until signed")).toHaveCount(0);
  await expect(row).toBeVisible();

  await callRiverTest(page, "releaseMessageSigning");
  await expect(committedRow(page, "pending until signed")).toHaveCount(1);
  await expect(pending(page), "the pending row is replaced, not duplicated").toHaveCount(0);
  const signed = committedRow(page, "pending until signed");
  await expect(signed).toBeInViewport();
  await expect(signed.getByTestId("message-footer"), "once signed, its own footer").toHaveCount(1);
  for (const id of FOOTER_CONTROLS) await expect(signed.getByTestId(id), id).toHaveCount(1);
});

test("messages commit in the order they were sent, whatever order they are signed in", { tag: "@chromium-only" }, async ({
  page,
}) => {
  await openRoomWithComposer(page);
  await callRiverTest(page, "holdMessageSigning");
  for (const text of ["order one", "order two", "order three"]) await send(page, text);
  await expect(pending(page)).toHaveText([/order one/, /order two/, /order three/]);

  // The second and third are signed first: they must wait for the first.
  const recorded = await callRiverTest(page, "messageSignsRecorded");
  await expect.poll(() => callRiverTest(page, "releaseMessageSign", 2)).toBe(3);
  await callRiverTest(page, "releaseMessageSign", 1);
  await expect
    .poll(() => callRiverTest(page, "messageSignsRecorded"), {
      message: "premise: the second and third signatures reach the outbox",
    })
    .toBe(recorded + 2);
  await settle(page);
  for (const text of ["order one", "order two", "order three"]) {
    await expect(committedRow(page, text), `"${text}" committed before "order one" was signed`).toHaveCount(0);
  }
  await expect(pending(page)).toHaveCount(3);

  await callRiverTest(page, "releaseMessageSign", 0);
  await expect(pending(page)).toHaveCount(0);
  const rows = page.getByTestId("conversation-history").getByTestId("message-row");
  const texts = (await rows.allInnerTexts()).filter((t) => t.includes("order "));
  expect(texts.map((t) => t.match(/order (one|two|three)/)?.[0])).toEqual([
    "order one",
    "order two",
    "order three",
  ]);
  await callRiverTest(page, "releaseMessageSigning");
});

test("a message that cannot be signed stays, failed, with Retry and Discard and no footer, and Retry sends it", { tag: "@chromium-only" }, async ({ page }) => {
  await openRoomWithComposer(page);
  await callRiverTest(page, "failNextMessageSign");
  await send(page, "signing fails once");

  const row = pending(page).filter({ hasText: "signing fails once" });
  await expect(row).toHaveAttribute("data-pending-state", "failed");
  await expect(row.getByTestId("pending-message-status")).toHaveText("Not sent");
  await expect(row.getByTestId("pending-message-retry")).toBeVisible();
  await expect(row.getByTestId("pending-message-discard")).toBeVisible();
  await expect(row.locator(FOOTER), "no footer while failed").toHaveCount(0);
  await expect(committedRow(page, "signing fails once")).toHaveCount(0);

  await row.getByTestId("pending-message-retry").click();
  await expect(committedRow(page, "signing fails once")).toHaveCount(1);
  await expect(pending(page)).toHaveCount(0);
});

test("a failed message can be discarded", { tag: "@chromium-only" }, async ({ page }) => {
  await openRoomWithComposer(page);
  await callRiverTest(page, "failNextMessageSign");
  await send(page, "discard me");
  const row = pending(page).filter({ hasText: "discard me" });
  await expect(row).toHaveAttribute("data-pending-state", "failed");
  await row.getByTestId("pending-message-discard").click();
  await expect(pending(page)).toHaveCount(0);
  await expect(committedRow(page, "discard me")).toHaveCount(0);
});

test("an own reaction, a switch to another emoji, and an edit show while their signatures are held", async ({ page }) => {
  await openRoomWithComposer(page);
  await send(page, "act on me");
  const row = committedRow(page, "act on me");
  await expect(row).toHaveCount(1);

  await callRiverTest(page, "holdMessageSigning");
  const chip = await reactTo(page, row);
  await expect(chip, "the reaction shows before it is signed").toHaveCount(1);
  const first = (await chip.textContent())!.trim();

  // The reaction, still being signed, switches to another emoji from the footer.
  await openInlinePicker(page, row);
  const second = page.getByTestId("emoji-picker").getByRole("button").nth(1);
  const secondEmoji = (await second.textContent())!.trim();
  expect(secondEmoji, "premise: another emoji").not.toBe(first.replace(/\d+$/, ""));
  await second.click();
  await expect(chip).toHaveCount(1);
  await expect(chip, "the switch shows before it is signed").toContainText(secondEmoji);

  const editArea = await openOwnMessageEdit(page, row);
  await editArea.fill("acted on, edited");
  await page.getByTestId("edit-save-button").click();
  const edited = committedRow(page, "acted on, edited");
  await expect(edited, "the edit shows before it is signed").toHaveCount(1);

  await callRiverTest(page, "releaseMessageSigning");
  await settle(page);
  await expect(edited, "the committed edit replaces the pending one in place").toHaveCount(1);
  await expect(edited.getByTestId("reaction-chip")).toHaveCount(1);
  await expect(edited.getByTestId("reaction-chip"), "the switch holds once signed").toContainText(secondEmoji);
});

test("a second click on a reaction still being signed undoes it", { tag: "@chromium-only" }, async ({ page }) => {
  await openRoomWithComposer(page);
  await send(page, "react twice");
  const row = committedRow(page, "react twice");
  await expect(row).toHaveCount(1);

  await callRiverTest(page, "holdMessageSigning");
  const chip = await reactTo(page, row);
  // A fine pointer removes its own reaction on click.
  await chip.click();
  await expect(row.getByTestId("reaction-chip"), "the pending reaction is undone").toHaveCount(0);

  await callRiverTest(page, "releaseMessageSigning");
  await settle(page);
  await expect(row.getByTestId("reaction-chip"), "and stays undone once both are signed").toHaveCount(0);
});
