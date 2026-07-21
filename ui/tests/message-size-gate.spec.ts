import { test, expect, Page } from "@playwright/test";

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

async function waitForApp(page: Page) {
  await page.waitForSelector(".app-root", { timeout: 30_000 });
  await expect(page.locator("aside, .app-root button")).not.toHaveCount(0);
}

async function selectRoom(page: Page, roomName: string) {
  const roomBtn = page.getByRole("button", { name: roomName });
  if (!(await roomBtn.isVisible({ timeout: 500 }).catch(() => false))) {
    // Narrow-window case: temporarily expand to click the room.
    const vp = page.viewportSize();
    if (vp && vp.width < 768) {
      await page.setViewportSize({ width: 1280, height: vp.height });
      await expect(roomBtn).toBeVisible({ timeout: 5_000 });
      await roomBtn.click();
      await expect(page.getByRole("heading", { name: roomName })).toBeVisible({
        timeout: 5_000,
      });
      await page.setViewportSize({ width: vp.width, height: vp.height });
      return;
    }
  }
  await roomBtn.click();
  await expect(page.getByRole("heading", { name: roomName })).toBeVisible({
    timeout: 5_000,
  });
}

async function openRoomWithInput(page: Page) {
  await page.goto("/");
  await waitForApp(page);
  // Self is the owner of "Your Private Room" in example data, so the
  // message input is available there.
  await selectRoom(page, "Your Private Room");
  await expect(page.getByTestId("message-input")).toBeVisible({
    timeout: 10_000,
  });
}

test.describe("Encoded message size gate", () => {
  test("998 raw chars (encoded 1007 > 1000) disables Send and keeps the draft on Enter", async ({
    page,
  }) => {
    await openRoomWithInput(page);
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
    await openRoomWithInput(page);
    const input = page.getByTestId("message-input");
    await input.fill("a".repeat(990)); // encoded 999

    await expect(page.getByText("999/1000")).toBeVisible();
    await expect(page.getByTestId("send-message-button")).toBeEnabled();
  });

  test("multi-byte characters count as encoded bytes, not characters", async ({
    page,
  }) => {
    await openRoomWithInput(page);
    const input = page.getByTestId("message-input");
    // 499 chars but 998 UTF-8 bytes -> encoded 1007 > 1000. Users count
    // characters; the limit is bytes. The gate must block this visibly
    // instead of losing the message after send.
    await input.fill("é".repeat(499));

    await expect(page.getByTestId("send-message-button")).toBeDisabled();
    await expect(page.getByText(/Message too long/)).toBeVisible();
  });
});
