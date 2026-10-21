import { test, expect } from "@playwright/test";
import { waitForApp, openRoomWithComposer } from "./example-room";
import { callRiverTest } from "./river-test";

// A private room whose secret this device does not hold cannot encrypt a
// message, and the room does not admit a public one. The conversation must
// say so instead of offering a composer whose send would vanish
// (freenet/river#703).
test("a private room without its secret offers no composer and says why", { tag: "@chromium-only" }, async ({
  page,
}) => {
  await page.goto("/");
  await waitForApp(page);
  // Self owns this room, which the hook needs.
  await openRoomWithComposer(page);

  await callRiverTest(page, "makeRoomPrivateWithoutSecret");
  await expect(
    page.getByTestId("room-list").getByRole("button", { name: "Your Private Room" }).getByLabel("Private room"),
    "premise: the room should now be private",
  ).toBeVisible();

  await expect(page.getByTestId("missing-room-secret-notice")).toBeVisible();
  await expect(page.getByTestId("missing-room-secret-notice")).toContainText("can't send messages");
  await expect(page.getByTestId("message-composer")).toHaveCount(0);
});
