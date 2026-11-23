import { test, expect, Locator, Page } from "@playwright/test";
import { chips, clickFooter, committedRow, openInlinePicker, openOwnMessageEdit, openRoomWithComposer, openRoomWithTarget, press, reactionChoices, reactTo, selectListedRoom, sendCommitted, settle, submitMessage } from "./example-room";
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

/// A send marks no room wait and lights no wait dots (they are for edits,
/// deletes and joins).
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
  await sendCommitted(page, "right after");
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

test("a reply started in one room does not show in another", { tag: "@chromium-only" }, async ({ page }) => {
  const target = await openRoomWithTarget(page);
  await clickFooter(page, target, "message-reply-button");
  await expect(replyPreview(page)).toContainText("react target");

  await selectListedRoom(page, "Team Chat Room");
  await expect(replyPreview(page), "another room has no reply target").toHaveCount(0);

  await selectListedRoom(page, "Your Private Room");
  await expect(replyPreview(page), "it is still held in its own room").toContainText("react target");
});

test("a failed reply is not carried into another room", { tag: "@chromium-only" }, async ({ page }) => {
  const target = await openRoomWithTarget(page);
  await clickFooter(page, target, "message-reply-button");
  await expect(replyPreview(page)).toContainText("react target");
  await callRiverTest(page, "failNextMessageSign");
  await submitMessage(page, "private reply");
  await expect(sendError(page)).toContainText("signing failed");
  await expect(replyPreview(page)).toContainText("react target");

  await selectListedRoom(page, "Team Chat Room");
  await expect(replyPreview(page), "the failed reply's quote stays in its room").toHaveCount(0);
  await input(page).fill("sent elsewhere");
  await input(page).press("Enter");
  const row = committedRow(page, "sent elsewhere");
  await expect(row).toHaveCount(1);
  await expect(row.getByTestId("reply-strip"), "and is not sent with this room's message").toHaveCount(0);
  await expect(row, "nor is the failed text").not.toContainText("private reply");

  await selectListedRoom(page, "Your Private Room");
  await expect(replyPreview(page), "the reply is still with its draft").toContainText("react target");
  await expect(input(page)).toHaveValue("private reply");
});

test("a failed send's text stays in its room, and a draft in a room not shown holds a reload", { tag: "@chromium-only" }, async ({ page }) => {
  await openRoomWithComposer(page);
  await callRiverTest(page, "failNextMessageSign");
  await submitMessage(page, "fails here");
  await expect(sendError(page)).toContainText("signing failed");
  await expect(input(page)).toHaveValue("fails here");

  await selectListedRoom(page, "Team Chat Room");
  await expect(input(page), "another room's composer is empty").toHaveValue("");
  await expect(sendError(page)).toHaveCount(0);
  expect(await callRiverTest(page, "userIsBusy"), "the draft left in the other room holds a reload").toBe(true);
  await input(page).fill("typed elsewhere");

  await selectListedRoom(page, "Your Private Room");
  await expect(input(page), "the failed send is where it was written").toHaveValue("fails here");
  await expect(sendError(page)).toContainText("signing failed");

  await selectListedRoom(page, "Team Chat Room");
  await expect(input(page), "and this room's draft is its own").toHaveValue("typed elsewhere");
});

test("a draft in the open composer holds a reload with the box unfocused", { tag: "@chromium-only" }, async ({ page }) => {
  await openRoomWithComposer(page);
  await expect.poll(() => callRiverTest(page, "userIsBusy"), { message: "premise: idle" }).toBe(false);
  await input(page).fill("typed here");
  await input(page).evaluate((el: HTMLElement) => el.blur());
  await expect(input(page), "premise: not focused").not.toBeFocused();
  expect(await callRiverTest(page, "userIsBusy"), "the open room's draft holds a reload").toBe(true);
  await input(page).fill("");
  await input(page).evaluate((el: HTMLElement) => el.blur());
  await expect.poll(() => callRiverTest(page, "userIsBusy"), { message: "an emptied draft holds nothing" }).toBe(false);
});

test("a draft outlives a room that shows a notice instead of the composer", { tag: "@chromium-only" }, async ({ page }) => {
  // Self owns this room, which the hook needs. Without its key, the room
  // shows a notice and no composer.
  await openRoomWithComposer(page);
  await callRiverTest(page, "makeRoomPrivateWithoutSecret");
  await expect(page.getByTestId("missing-room-secret-notice")).toBeVisible();
  await expect.poll(() => callRiverTest(page, "userIsBusy"), { message: "premise: idle" }).toBe(false);

  await selectListedRoom(page, "Team Chat Room");
  await input(page).fill("typed in team chat");

  await selectListedRoom(page, "Your Private Room");
  await expect(page.getByTestId("missing-room-secret-notice")).toBeVisible();
  await expect(page.getByTestId("message-composer"), "no composer is mounted").toHaveCount(0);
  expect(await callRiverTest(page, "userIsBusy"), "the draft in the room not shown holds a reload").toBe(true);

  await selectListedRoom(page, "Team Chat Room");
  await expect(input(page), "the draft is still there").toHaveValue("typed in team chat");
});

// A send that fails while the room shows a notice instead of the composer
// (its key missing, self banned or removed, the identity gone, a sync) has no
// composer to come back to. The notice shows it until the user discards it or
// the composer returns and takes it back.

const failedNotice = (page: Page) => page.getByTestId("failed-send-notice");

/// Holds signing, sends `text` from the composer and waits until the composer
/// shows it locked: the send is in the outbox, being signed.
async function sendHeld(page: Page, text: string) {
  await callRiverTest(page, "holdMessageSigning");
  await submitMessage(page, text);
  await expectLocked(page, text);
}

/// Fails the held signing request and waits for its result to land.
async function failHeldSend(page: Page) {
  const before = await callRiverTest(page, "messageSignsRecorded");
  await callRiverTest(page, "failNextMessageSign");
  await callRiverTest(page, "releaseMessageSigning");
  await expect.poll(() => callRiverTest(page, "messageSignsRecorded"), { message: "the signing result landed" }).toBeGreaterThan(before);
}

/// The notice area shows the failed send: its text, its reply target, why it
/// was not sent, and Discard.
async function expectFailedNotice(page: Page, text: string, reply: string | null) {
  const notice = failedNotice(page);
  await expect(notice, "the failed send is shown beside the notice").toBeVisible();
  await expect(notice.getByTestId("failed-send-text")).toHaveText(text);
  if (reply === null) {
    await expect(notice.getByTestId("failed-send-reply")).toHaveCount(0);
  } else {
    await expect(notice.getByTestId("failed-send-reply")).toContainText(reply);
  }
  await expect(notice.getByTestId("failed-send-reason")).toContainText("signing failed");
  await expect(notice.getByTestId("failed-send-discard-button")).toBeEnabled();
}

test("a send failing behind the missing-key notice is shown there, and Discard lets a reload go", { tag: "@chromium-only" }, async ({ page }) => {
  const target = await openRoomWithTarget(page);
  await clickFooter(page, target, "message-reply-button");
  await expect(replyPreview(page)).toContainText("react target");
  await sendHeld(page, "fails behind the notice");
  await callRiverTest(page, "makeRoomPrivateWithoutSecret");
  await expect(page.getByTestId("missing-room-secret-notice")).toBeVisible();
  await expect(page.getByTestId("message-composer"), "premise: no composer").toHaveCount(0);

  await failHeldSend(page);
  await expect(page.getByTestId("missing-room-secret-notice"), "it failed while the notice showed").toBeVisible();
  await expectFailedNotice(page, "fails behind the notice", "react target");
  expect(await callRiverTest(page, "userIsBusy"), "the failed send holds a reload").toBe(true);

  await failedNotice(page).getByTestId("failed-send-discard-button").click();
  await expect(failedNotice(page)).toHaveCount(0);
  await expect.poll(() => callRiverTest(page, "userIsBusy"), { message: "discarded, nothing holds a reload" }).toBe(false);
  await expect(page.getByRole("heading", { name: "Your Private Room" }), "in the same room").toBeVisible();
  await expect(page.getByTestId("missing-room-secret-notice")).toBeVisible();

  // Another room's draft still holds one.
  await selectListedRoom(page, "Team Chat Room");
  await input(page).fill("a real draft");
  await selectListedRoom(page, "Your Private Room");
  expect(await callRiverTest(page, "userIsBusy"), "another room's draft holds a reload").toBe(true);
});

test("a send failing behind the missing-key notice comes back as one draft with its reply when the key arrives, and sends once", { tag: "@chromium-only" }, async ({ page }) => {
  const target = await openRoomWithTarget(page);
  await clickFooter(page, target, "message-reply-button");
  await expect(replyPreview(page)).toContainText("react target");
  await sendHeld(page, "returns with the key");
  await callRiverTest(page, "makeRoomPrivateWithoutSecret");
  await expect(page.getByTestId("missing-room-secret-notice")).toBeVisible();
  await failHeldSend(page);
  await expectFailedNotice(page, "returns with the key", "react target");

  await callRiverTest(page, "makeRoomPrivate", 1);
  await expect(page.getByTestId("message-composer")).toBeVisible();
  await expect(failedNotice(page)).toHaveCount(0);
  await expect(input(page), "one draft, the failed text once").toHaveValue("returns with the key");
  await expect(input(page)).not.toHaveAttribute("readonly", /.*/);
  await expect(replyPreview(page), "with its reply").toContainText("react target");
  await expect(sendError(page)).toContainText("signing failed");

  await sendButton(page).click();
  const row = committedRow(page, "returns with the key");
  await expect(row).toHaveCount(1);
  // Committed as a reply. The quoted message is public, which the room, now
  // private, does not show, so the strip cannot resolve it.
  await expect(row.locator('[data-testid="reply-strip"], [data-testid="reply-strip-unavailable"]')).toHaveCount(1);
  await expect(input(page)).toHaveValue("");
  await settle(page);
  await expect(row, "sent once").toHaveCount(1);
});

test("a send failing after a ban shows in the banned notice, and the composer takes it back when the ban is lifted", { tag: "@chromium-only" }, async ({ page }) => {
  await openRoomWithComposer(page);
  await callRiverTest(page, "makeSelfPlainMember");
  await settle(page);
  await expect(page.getByTestId("message-composer"), "premise: a plain member has a composer").toBeVisible();
  await sendHeld(page, "sent before the ban");
  await callRiverTest(page, "setSelfStanding", "banned");
  await expect(page.getByTestId("banned-notice")).toBeVisible();

  await failHeldSend(page);
  await expect(page.getByTestId("banned-notice")).toBeVisible();
  await expectFailedNotice(page, "sent before the ban", null);

  await callRiverTest(page, "setSelfStanding", "member");
  await expect(input(page)).toHaveValue("sent before the ban");
  await expect(sendError(page)).toContainText("signing failed");
  await expect(failedNotice(page)).toHaveCount(0);
  await sendButton(page).click();
  await expect(committedRow(page, "sent before the ban")).toHaveCount(1);
  await expect(input(page)).toHaveValue("");
});

test("a send failing after self is removed shows in the not-a-member notice, and Discard drops it for good", { tag: "@chromium-only" }, async ({ page }) => {
  await openRoomWithComposer(page);
  await callRiverTest(page, "makeSelfPlainMember");
  await settle(page);
  await sendHeld(page, "sent before removal");
  await callRiverTest(page, "setSelfStanding", "not-member");
  await expect(page.getByTestId("not-member-notice")).toBeVisible();

  await failHeldSend(page);
  await expect(page.getByTestId("not-member-notice")).toBeVisible();
  await expectFailedNotice(page, "sent before removal", null);
  expect(await callRiverTest(page, "userIsBusy"), "the failed send holds a reload").toBe(true);
  await failedNotice(page).getByTestId("failed-send-discard-button").click();
  await expect(failedNotice(page)).toHaveCount(0);
  await expect.poll(() => callRiverTest(page, "userIsBusy"), { message: "discarded, nothing holds a reload" }).toBe(false);

  await callRiverTest(page, "setSelfStanding", "member");
  await expect(page.getByTestId("message-composer")).toBeVisible();
  await settle(page);
  await expect(input(page), "nothing comes back").toHaveValue("");
  await expect(sendError(page)).toHaveCount(0);
});

test("a send failing while this device lacks the room's identity says how to recover it, shows none of it, and holds no reload", { tag: "@chromium-only" }, async ({ page }) => {
  const target = await openRoomWithTarget(page);
  await clickFooter(page, target, "message-reply-button");
  await expect(replyPreview(page)).toContainText("react target");
  await sendHeld(page, "written before the key left");
  await callRiverTest(page, "setRoomIdentityAvailable", false);
  await expect(page.getByTestId("identity-unavailable-notice")).toBeVisible();

  await failHeldSend(page);
  await expect(page.getByTestId("identity-unavailable-notice")).toBeVisible();
  const notice = failedNotice(page);
  await expect(notice, "the notice says a send is waiting").toBeVisible();
  await expect(notice).toContainText("original identity");
  await expect(notice.getByTestId("failed-send-text")).toHaveCount(0);
  await expect(notice.getByTestId("failed-send-reply")).toHaveCount(0);
  await expect(notice.getByTestId("failed-send-discard-button")).toHaveCount(0);
  await expect(notice).not.toContainText("react target");
  await expect(page.getByText("written before the key left"), "its text is shown nowhere").toHaveCount(0);
  await expect.poll(() => callRiverTest(page, "userIsBusy"), { message: "a row nobody can see holds no reload" }).toBe(false);

  await callRiverTest(page, "setRoomIdentityAvailable", true);
  await expect(page.getByTestId("message-composer")).toBeVisible();
  await expect(failedNotice(page)).toHaveCount(0);
  await expect(input(page), "restoring the identity gives it back").toHaveValue("written before the key left");
  await expect(replyPreview(page)).toContainText("react target");
  await expect(sendError(page)).toContainText("signing failed");
  await sendButton(page).click();
  const row = committedRow(page, "written before the key left");
  await expect(row).toHaveCount(1);
  await expect(row.getByTestId("reply-strip")).toContainText("react target");
});

test("a failed send from a replaced identity shows in no notice, and the toast names none of it", { tag: "@chromium-only" }, async ({ page }) => {
  await openRoomWithComposer(page);
  await sendHeld(page, "failed as the old identity");
  await callRiverTest(page, "replaceRoomIdentity");
  // The new identity is not a member of this room.
  await expect(page.getByTestId("not-member-notice")).toBeVisible();
  await failHeldSend(page);
  await expect(page.getByTestId("toast-message")).toContainText("previous identity");
  await expect(page.getByTestId("toast-message")).not.toContainText("failed as the old identity");
  await expect(failedNotice(page)).toHaveCount(0);
  await expect(page.getByText("failed as the old identity")).toHaveCount(0);
  await expect.poll(() => callRiverTest(page, "userIsBusy")).toBe(false);
});

test("a send failing while the room syncs is shown beside the sync notice", { tag: "@chromium-only" }, async ({ page }) => {
  await openRoomWithComposer(page);
  await sendHeld(page, "sent before the resync");
  await callRiverTest(page, "showRoomInitialSync");
  await expect(page.getByTestId("room-sync-banner-dots")).toBeVisible();
  await expect(page.getByTestId("message-composer")).toHaveCount(0);

  await failHeldSend(page);
  await expectFailedNotice(page, "sent before the resync", null);
  await failedNotice(page).getByTestId("failed-send-discard-button").click();
  await expect(failedNotice(page)).toHaveCount(0);
  await expect.poll(() => callRiverTest(page, "userIsBusy")).toBe(false);
});

test("a send after a failure waits for the key it was first sealed under", { tag: "@chromium-only" }, async ({ page }) => {
  const keyNotHere = "does not have the room's newest key yet";
  await openRoomWithComposer(page);
  await callRiverTest(page, "makeRoomPrivate", 3);
  await callRiverTest(page, "holdMessageSigning");
  await submitMessage(page, "sealed at three");
  await expectLocked(page, "sealed at three");

  // The room's secret pointer moves back while the message is being signed.
  await callRiverTest(page, "setSecretVersion", 2);
  await callRiverTest(page, "releaseMessageSigning");
  await expect(sendError(page)).toContainText(keyNotHere);
  await expect(input(page)).toHaveValue("sealed at three");
  const signed = await callRiverTest(page, "messageSignsRecorded");

  // Send again: not sealed under the older key, so nothing is signed or sent.
  await sendButton(page).click();
  await settle(page);
  await expect(sendError(page)).toContainText(keyNotHere);
  await expect(committedRow(page, "sealed at three")).toHaveCount(0);
  await expect(input(page)).toHaveValue("sealed at three");
  expect(await callRiverTest(page, "messageSignsRecorded"), "nothing was signed").toBe(signed);

  // The key is back: the same Send goes.
  await callRiverTest(page, "setSecretVersion", 3);
  await settle(page);
  await sendButton(page).click();
  await expect(committedRow(page, "sealed at three")).toHaveCount(1);
  await expect(sendError(page)).toHaveCount(0);
  await expect(input(page)).toHaveValue("");
});

test("a send from a replaced identity is never committed",{ tag: "@chromium-only" }, async ({ page }) => {
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
  const row = await sendCommitted(page, "act on me");

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
  const row = await sendCommitted(page, "react twice");

  await callRiverTest(page, "holdMessageSigning");
  const chip = await reactTo(page, row);
  // A click on its own chip takes the reaction back.
  await chip.click();
  await expect(row.getByTestId("reaction-chip"), "the pending reaction is undone").toHaveCount(0);

  await callRiverTest(page, "releaseMessageSigning");
  await settle(page);
  await expect(row.getByTestId("reaction-chip"), "and stays undone once both are signed").toHaveCount(0);
});

// A private room's key pointer can move back below a version this device has
// already sealed under there. A composer send is refused then (above); so is
// an edit, a delete and a reaction, each of which is sealed too, and an edit
// carries new private text. Nothing is signed, shown or marked until the key
// is back.
const KEY_NOT_HERE = "does not have the room's newest key yet";
const signsRecorded = (page: Page) => callRiverTest(page, "messageSignsRecorded");

/// An own message committed at key version 3 in a fresh private room, with an
/// own reaction on it when `seeded`, and the room's key pointer then moved
/// back to 2. Returns the row and the signatures recorded so far.
async function openRoomBelowItsFloor(page: Page, seeded: boolean): Promise<{ row: Locator; signed: number }> {
  await openRoomWithComposer(page);
  await callRiverTest(page, "makeRoomPrivate", 3);
  const before = await signsRecorded(page);
  const row = await sendCommitted(page, "floor target");
  if (seeded) await reactTo(page, row);
  const signed = before + (seeded ? 2 : 1);
  await expect.poll(() => signsRecorded(page), { message: "premise: the setup is signed at version 3" }).toBe(signed);
  await settle(page);
  expect(await callRiverTest(page, "userWaitRooms"), "premise: no wait mark").toBe(0);
  await callRiverTest(page, "setSecretVersion", 2);
  await settle(page);
  return { row, signed };
}

/// A refused action signed nothing and marked nothing.
async function expectNothingSigned(page: Page, signed: number, why: string) {
  await settle(page);
  expect(await signsRecorded(page), `${why}: nothing new is signed`).toBe(signed);
  expect(await callRiverTest(page, "userWaitRooms"), `${why}: no wait mark`).toBe(0);
}

async function expectRefusalToast(page: Page) {
  await expect(page.getByTestId("toast-message"), "the reason is shown").toContainText(KEY_NOT_HERE);
}

async function confirmDelete(page: Page, row: Locator) {
  await clickFooter(page, row, "message-delete-button");
  await page.getByRole("button", { name: "Delete", exact: true }).click();
  await expect(page.getByRole("heading", { name: "Delete Message?" })).toHaveCount(0);
}

/// Pick the `index`-th emoji of `row`'s reaction choices.
async function pickReaction(page: Page, row: Locator, index: number): Promise<string> {
  await openInlinePicker(page, row);
  const choice = (await reactionChoices(page)).nth(index);
  const emoji = (await choice.textContent())!.trim();
  await press(choice);
  return emoji;
}

test("an edit below the room's sealed key version is refused and kept, then saved once the key is back", { tag: "@chromium-only" }, async ({ page }) => {
  const { row, signed } = await openRoomBelowItsFloor(page, false);
  const edited = committedRow(page, "floor target, edited");
  const editError = page.getByTestId("edit-save-error");

  // Enter, then Save: both keep the form and what was typed.
  const editArea = await openOwnMessageEdit(page, row);
  await editArea.fill("floor target, edited");
  await editArea.press("Enter");
  await expectNothingSigned(page, signed, "an edit by Enter at version 2");
  await expect(editError).toContainText(KEY_NOT_HERE);
  await expect(editArea, "the proposed text is kept for another try").toHaveValue("floor target, edited");
  await page.getByTestId("edit-save-button").click();
  await expectNothingSigned(page, signed, "an edit by Save at version 2");
  await expect(editError).toContainText(KEY_NOT_HERE);
  await expect(editArea).toHaveValue("floor target, edited");
  await expect(edited, "the message is not edited").toHaveCount(0);

  // The key is back: the same Save goes, once.
  await callRiverTest(page, "setSecretVersion", 3);
  await settle(page);
  await page.getByTestId("edit-save-button").click();
  await expect(edited).toHaveCount(1);
  await expect(page.locator('textarea[id^="edit-msg-"]'), "an accepted edit closes the form").toHaveCount(0);
  await expect.poll(() => signsRecorded(page), { message: "one signature for the edit" }).toBe(signed + 1);
  await settle(page);
  expect(await signsRecorded(page), "and no more").toBe(signed + 1);
});

test("a confirmed delete below the room's sealed key version is refused, then goes once the key is back", { tag: "@chromium-only" }, async ({ page }) => {
  const { row, signed } = await openRoomBelowItsFloor(page, false);

  await confirmDelete(page, row);
  await expectNothingSigned(page, signed, "a delete at version 2");
  await expect(row, "the message is still there").toHaveCount(1);
  await expect(row).toContainText("floor target");
  await expectRefusalToast(page);

  await callRiverTest(page, "setSecretVersion", 3);
  await settle(page);
  await confirmDelete(page, row);
  await expect(row).toHaveCount(0);
  await expect.poll(() => signsRecorded(page), { message: "one signature for the delete" }).toBe(signed + 1);
  await settle(page);
  expect(await signsRecorded(page), "and no more").toBe(signed + 1);
});

for (const kind of ["add", "remove", "swap"] as const) {
  test(`a reaction ${kind} below the room's sealed key version is refused, then goes once the key is back`, { tag: "@chromium-only" }, async ({ page }) => {
    const seeded = kind !== "add";
    const { row, signed } = await openRoomBelowItsFloor(page, seeded);
    const chip = chips(row);
    const original = seeded ? (await chip.textContent())!.trim() : "";
    // The reaction the accepted action leaves, and the signatures it takes
    // (a swap removes the old one and adds the new one).
    const act = async (): Promise<string | null> => {
      if (kind === "add") return pickReaction(page, row, 0);
      if (kind === "swap") return pickReaction(page, row, 1);
      await chip.click();
      return null;
    };
    const signs = kind === "swap" ? 2 : 1;

    await act();
    await expectNothingSigned(page, signed, `a reaction ${kind} at version 2`);
    await expect(chip, "the chips are as they were").toHaveCount(seeded ? 1 : 0);
    if (seeded) await expect(chip, "the own reaction survives").toHaveText(original);
    await expectRefusalToast(page);

    await callRiverTest(page, "setSecretVersion", 3);
    await settle(page);
    const emoji = await act();
    if (emoji === null) {
      await expect(chip).toHaveCount(0);
    } else {
      await expect(chip).toHaveCount(1);
      await expect(chip).toContainText(emoji);
      if (kind === "swap") expect(original, "premise: another emoji").not.toContain(emoji);
    }
    await expect.poll(() => signsRecorded(page), { message: `${signs} signature(s) for the ${kind}` }).toBe(signed + signs);
    await settle(page);
    expect(await signsRecorded(page), "and no more").toBe(signed + signs);
  });
}
