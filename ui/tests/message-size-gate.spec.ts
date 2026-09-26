import { test, expect } from "@playwright/test";
import { waitForApp, openRoomWithComposer, openOwnMessageEdit } from "./example-room";

// Regression tests for the "message was lost" bug (HostFat, Matrix 2026-07):
// the input gate compared raw text bytes against max_message_size, but the
// contract validates the ENCODED content (CBOR framing adds ~9 bytes for
// plain text). A message in the gap passed the UI gate, had its draft
// cleared, then was silently dropped by the encoded-size safety net —
// "WARN Message too long: 1006 encoded bytes, max 1000 bytes" with the
// counter still under 1000.
//
// The example-data rooms use Configuration::default(): max_message_size =
// 1000 bytes (encoded), public room. Public text encodes as CBOR
// {text: "..."} = raw bytes + 9 for texts in the 256..65535-byte range.

test.beforeEach(async ({ page }) => {
  await page.goto("/");
  await waitForApp(page);
  await openRoomWithComposer(page);
});

test.describe("Encoded message size gate", () => {
  test("998 raw chars (encoded 1007 > 1000) disables Send and keeps the draft on Enter", async ({
    page,
  }) => {
    const input = page.getByTestId("message-input");
    const text = "a".repeat(998); // raw 998 <= 1000, encoded 1007 > 1000
    await input.fill(text);

    // The gate must count encoded bytes: Send disabled, counter red.
    await expect(page.getByTestId("send-message-button")).toBeDisabled();
    await expect(page.getByText(/Message too long/)).toBeVisible();
    await expect(page.getByText(/1007\/1000/)).toBeVisible();

    // Enter must NOT clear the draft — before the fix the draft was cleared
    // and the message silently dropped by the encoded-size safety net.
    await input.press("Enter");
    await expect(input).toHaveValue(text);
  });

  test("990 raw chars (encoded 999 <= 1000) shows the encoded count and keeps Send enabled", async ({
    page,
  }) => {
    const input = page.getByTestId("message-input");
    await input.fill("a".repeat(990)); // encoded 999

    await expect(page.getByText("999/1000")).toBeVisible();
    await expect(page.getByTestId("send-message-button")).toBeEnabled();
  });

  test("multi-byte characters count as encoded bytes, not characters", async ({
    page,
  }) => {
    const input = page.getByTestId("message-input");
    // 499 chars but 998 UTF-8 bytes -> encoded 1007 > 1000. Users count
    // characters; the limit is bytes. The gate must block this visibly
    // instead of losing the message after send.
    await input.fill("é".repeat(499));

    await expect(page.getByTestId("send-message-button")).toBeDisabled();
    await expect(page.getByText(/Message too long/)).toBeVisible();
  });

  test("a message at exactly the encoded limit is sendable (no off-by-one)", async ({
    page,
  }) => {
    const input = page.getByTestId("message-input");
    await input.fill("a".repeat(991)); // encoded exactly 1000

    await expect(page.getByText("1000/1000")).toBeVisible();
    await expect(page.getByTestId("send-message-button")).toBeEnabled();
  });
});

// The in-place edit form previously had NO size gate: an over-limit edit
// action was signed, sent, and silently pruned by contract validation. The
// edit action (ActionContentV1: target id + payload) carries more overhead
// than plain text, so the gate must measure the encoded action.
test.describe("Encoded size gate on the edit form", () => {
  test("over-limit edit disables Save, shows the counter, and Enter keeps the form open", async ({
    page,
  }) => {
    const editArea = await openOwnMessageEdit(page);

    // 998 raw chars: within the limit as raw text, over it as an encoded
    // edit action (ActionContentV1 overhead > plain-text overhead).
    await editArea.fill("a".repeat(998));

    const saveBtn = page.getByRole("button", { name: /Save/ });
    await expect(saveBtn).toBeDisabled();
    await expect(page.getByText(/Message too long/)).toBeVisible();

    // Enter must not discard the over-limit edit — the form stays open.
    await editArea.press("Enter");
    await expect(editArea).toBeVisible();
    await expect(editArea).toHaveValue("a".repeat(998));
  });
});
