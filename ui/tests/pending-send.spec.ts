import { test, expect, Page } from "@playwright/test";
import { clickFooter, committedRow, openInlinePicker, openOwnMessageEdit, openRoomWithComposer, openRoomWithTarget, press, reactionChoices, reactTo, selectListedRoom, settle, submitMessage } from "./example-room";
import { callRiverTest } from "./river-test";

// An own message is signed by the chat delegate, which can take seconds. The
// composer keeps the text and reply target, read-only, until the room admits
// the signed message; then the message shows once in the history and the
// draft clears. A failure gives the draft back with the reason. With
// `no-sync` the delegate fails instantly and the local key signs, so these
// tests hold signing open through a test hook: without the hold they would
// only ever see the fast path.

const input = (page: Page) => page.getByTestId("message-input");
const composerBox = (page: Page) => page.getByTestId("message-composer-box");
const sendButton = (page: Page) => page.getByTestId("send-message-button");
const replyPreview = (page: Page) => page.getByTestId("composer-reply-preview");
const sendError = (page: Page) => page.getByTestId("composer-send-error");

/// Submit `text` and wait for the room to admit it, so the next send starts
/// from an empty draft.
async function send(page: Page, text: string) {
  await submitMessage(page, text);
  await expect(committedRow(page, text)).toHaveCount(1);
  await expect(input(page), "the draft clears once the room admits it").toHaveValue("");
}

/// A send marks no room wait and lights no wait dots (they are for edits,
/// deletes, reactions and joins).
async function expectNoWait(page: Page, why: string) {
  expect(await callRiverTest(page, "userWaitRooms"), `${why}: no room wait`).toBe(0);
  await expect(page.locator('[data-testid="user-wait-dots"][data-active="true"]'), `${why}: no dots`).toHaveCount(0);
  await expect(page.getByTestId("user-wait-spacer"), `${why}: no spacer`).toHaveAttribute("data-active", "false");
}

/// Nothing of a send is in the history, and it lights no wait dots.
async function expectNoInterimRowOrWait(page: Page, text: string) {
  await expect(committedRow(page, text)).toHaveCount(0);
  await expect(page.getByTestId("pending-message"), "no interim history row").toHaveCount(0);
  await expectNoWait(page, "a send being signed");
}

async function expectLocked(page: Page, text: string) {
  await expect(input(page)).toHaveValue(text);
  await expect(input(page)).toHaveAttribute("readonly", /.*/);
  await expect(composerBox(page)).toHaveAttribute("data-sending", "true");
  await expect(sendButton(page)).toBeDisabled();
}

/// The signing requests held so far: polls until one arrives, releasing it.
async function releaseOnlyRequest(page: Page) {
  await expect
    .poll(() => callRiverTest(page, "releaseMessageSign", 0), { message: "exactly one signing request was made" })
    .toBe(1);
}

test("Enter keeps a held reply in the composer, read-only and sent once, then it commits once and clears", async ({ page }) => {
  const target = await openRoomWithTarget(page);
  await callRiverTest(page, "setSyncStatus", "connected");
  await clickFooter(page, target, "message-reply-button");
  await expect(replyPreview(page)).toContainText("react target");
  await callRiverTest(page, "holdMessageSigning");
  await input(page).fill("held reply");

  // Three activations before the next render: one send.
  await page.evaluate(() => {
    const el = document.getElementById("message-input")!;
    for (let i = 0; i < 3; i++) {
      el.dispatchEvent(new KeyboardEvent("keydown", { key: "Enter", bubbles: true, cancelable: true }));
    }
  });
  await expectLocked(page, "held reply");
  await expect(replyPreview(page), "the reply target stays with the send").toContainText("react target");
  await expect(page.getByTestId("composer-reply-cancel"), "and cannot be dropped while it is sent").toHaveCount(0);
  await input(page).press("Enter");
  await settle(page);
  await expectNoInterimRowOrWait(page, "held reply");

  await releaseOnlyRequest(page);
  const row = committedRow(page, "held reply");
  await expect(row).toHaveCount(1);
  await expect(row.getByTestId("reply-strip"), "committed as a reply").toContainText("react target");
  await expect(row, "an own send goes to the end once admitted").toBeInViewport();
  await expect(input(page)).toHaveValue("");
  await expect(input(page)).not.toHaveAttribute("readonly", /.*/);
  await expect(replyPreview(page)).toHaveCount(0);
  await expect(composerBox(page)).toHaveAttribute("data-sending", "false");
  await expectNoWait(page, "admitted");
  await callRiverTest(page, "releaseMessageSigning");

  // The next message goes at once, and lights nothing either.
  await send(page, "right after");
  await expect(committedRow(page, "held reply"), "no duplicate").toHaveCount(1);
  await settle(page);
  await expectNoWait(page, "an ordinary send, connected");
});

test("the composer is one line again once a multi-line send is admitted", async ({ page }) => {
  await openRoomWithComposer(page);
  const height = () => input(page).evaluate((el) => el.getBoundingClientRect().height);
  const empty = await height();
  await callRiverTest(page, "holdMessageSigning");
  await input(page).fill("line one\nline two\nline three");
  await expect.poll(height, { message: "premise: the draft grows the box" }).toBeGreaterThan(empty + 20);
  await input(page).press("Enter");
  await expectLocked(page, "line one\nline two\nline three");
  expect(await height(), "the send keeps its size while it is signed").toBeGreaterThan(empty + 20);
  await callRiverTest(page, "releaseMessageSigning");
  await expect(committedRow(page, "line three")).toHaveCount(1);
  await expect.poll(height, { message: "back to one line" }).toBeLessThan(empty + 2);
});

test("rapid Send clicks send once", { tag: "@chromium-only" }, async ({ page }) => {
  await openRoomWithComposer(page);
  await callRiverTest(page, "holdMessageSigning");
  await input(page).fill("clicked thrice");
  await page.evaluate(() => {
    const button = document.querySelector<HTMLButtonElement>('[data-testid="send-message-button"]')!;
    for (let i = 0; i < 3; i++) button.click();
  });
  await expectLocked(page, "clicked thrice");
  await expectNoInterimRowOrWait(page, "clicked thrice");
  await releaseOnlyRequest(page);
  await expect(committedRow(page, "clicked thrice")).toHaveCount(1);
  await expect(input(page)).toHaveValue("");
  await callRiverTest(page, "releaseMessageSigning");
});

test("a send that cannot be signed comes back as an editable draft with its reply, and sending it again commits once", { tag: "@chromium-only" }, async ({ page }) => {
  const target = await openRoomWithTarget(page);
  await clickFooter(page, target, "message-reply-button");
  await expect(replyPreview(page)).toContainText("react target");
  await callRiverTest(page, "failNextMessageSign");
  await submitMessage(page, "signing fails once");

  await expect(sendError(page)).toContainText("signing failed");
  await expect(input(page), "the draft is intact").toHaveValue("signing fails once");
  await expect(input(page)).not.toHaveAttribute("readonly", /.*/);
  await expect(replyPreview(page), "and its reply target").toContainText("react target");
  await expect(page.getByTestId("composer-reply-cancel")).toHaveCount(1);
  await expect(sendButton(page)).toBeEnabled();
  await expect(committedRow(page, "signing fails once")).toHaveCount(0);
  await expect(page.getByTestId("pending-message"), "no retry control in the history").toHaveCount(0);

  // Edited, then sent again: one message, the edited one, still a reply.
  await input(page).fill("signing fails once, edited");
  await expect(sendError(page), "typing clears the reason").toHaveCount(0);
  await sendButton(page).click();
  const row = committedRow(page, "signing fails once, edited");
  await expect(row).toHaveCount(1);
  await expect(row.getByTestId("reply-strip")).toContainText("react target");
  await expect(input(page)).toHaveValue("");
  await expect(page.getByTestId("conversation-history").getByText("signing fails once", { exact: false })).toHaveCount(1);
});

test("a send the room does not admit comes back with the reason, and sending it again commits once", { tag: "@chromium-only" }, async ({ page }) => {
  await openRoomWithComposer(page);
  await callRiverTest(page, "forgeNextSendSignature");
  await submitMessage(page, "refused once");
  await expect(sendError(page)).toContainText("the room did not admit the message");
  await expect(input(page)).toHaveValue("refused once");
  await expect(committedRow(page, "refused once")).toHaveCount(0);

  await input(page).press("Enter");
  await expect(committedRow(page, "refused once")).toHaveCount(1);
  await expect(sendError(page)).toHaveCount(0);
  await expect(input(page)).toHaveValue("");
});

test("a send stays with its room: another room's composer is free, and a late commit clears nothing there", { tag: "@chromium-only" }, async ({ page }) => {
  await openRoomWithComposer(page);
  await callRiverTest(page, "holdMessageSigning");
  await submitMessage(page, "sent before switching");
  await expectLocked(page, "sent before switching");
  expect(await callRiverTest(page, "userIsBusy"), "a send being signed holds a reload").toBe(true);

  await selectListedRoom(page, "Team Chat Room");
  await expect(composerBox(page), "this room's composer is not locked").toHaveAttribute("data-sending", "false");
  await expect(input(page)).not.toHaveValue("sent before switching");
  await input(page).fill("draft in another room");
  await callRiverTest(page, "releaseMessageSigning");
  await expect.poll(() => callRiverTest(page, "messageSignsRecorded")).toBeGreaterThan(0);
  await settle(page);
  await expect(input(page), "the late commit did not clear this draft").toHaveValue("draft in another room");
  await expect(committedRow(page, "sent before switching"), "nor land here").toHaveCount(0);

  await selectListedRoom(page, "Your Private Room");
  await expect(committedRow(page, "sent before switching")).toHaveCount(1);
  await expect(composerBox(page)).toHaveAttribute("data-sending", "false");
});

test("a send failing while its room is not shown is given back when the room is", { tag: "@chromium-only" }, async ({ page }) => {
  await openRoomWithComposer(page);
  await callRiverTest(page, "holdMessageSigning");
  await callRiverTest(page, "failNextMessageSign");
  await submitMessage(page, "fails while away");
  await selectListedRoom(page, "Team Chat Room");
  await callRiverTest(page, "releaseMessageSigning");
  await expect.poll(() => callRiverTest(page, "messageSignsRecorded")).toBeGreaterThan(0);
  await expect(sendError(page), "another room shows no failure").toHaveCount(0);
  expect(await callRiverTest(page, "userIsBusy"), "the failed send still holds a reload").toBe(true);

  await selectListedRoom(page, "Your Private Room");
  await expect(sendError(page)).toContainText("signing failed");
  await expect(input(page)).toHaveValue("fails while away");
});

test("a send from a replaced identity is never committed", { tag: "@chromium-only" }, async ({ page }) => {
  await openRoomWithComposer(page);
  await callRiverTest(page, "holdMessageSigning");
  await submitMessage(page, "from the old identity");
  await expectLocked(page, "from the old identity");
  await callRiverTest(page, "replaceRoomIdentity");
  await callRiverTest(page, "releaseMessageSigning");
  await expect(page.getByTestId("toast-message")).toContainText("previous identity");
  await expect(committedRow(page, "from the old identity")).toHaveCount(0);
  await expect(page.getByTestId("conversation-history").getByText("from the old identity")).toHaveCount(0);
  await expect(page.locator('[data-testid="message-composer-box"][data-sending="true"]'), "nothing is left locked").toHaveCount(0);
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
  const second = (await reactionChoices(page)).nth(1);
  const secondEmoji = (await second.textContent())!.trim();
  expect(secondEmoji, "premise: another emoji").not.toBe(first.replace(/\d+$/, ""));
  await press(second);
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
  // A click on its own chip takes the reaction back.
  await chip.click();
  await expect(row.getByTestId("reaction-chip"), "the pending reaction is undone").toHaveCount(0);

  await callRiverTest(page, "releaseMessageSigning");
  await settle(page);
  await expect(row.getByTestId("reaction-chip"), "and stays undone once both are signed").toHaveCount(0);
});
