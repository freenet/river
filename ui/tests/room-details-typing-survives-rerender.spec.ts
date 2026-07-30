import { test, expect, Page } from "@playwright/test";

// Regression test for freenet/river#564: typing in the Room Details dialog was
// wiped every few seconds by unrelated room-state updates.
//
// `value` on `input`/`textarea` is a VOLATILE attribute in dioxus-html
// (`elements.rs:1488,1594`), so dioxus re-writes it to the DOM on every
// re-render even when the rendered string has not changed
// (`dioxus-core-0.7.9/src/diff/node.rs:463`), and the interpreter assigns
// `node.value = value` whenever the live DOM value differs
// (`dioxus-interpreter-js-0.7.9/src/ts/set_attribute.ts:31-33`).
//
// The Room Description textarea updated its bound signal only in `onchange`,
// which fires on blur. So while the owner typed, the signal still held the
// pre-typing text, and the next re-render reset the textarea to it. As
// reported, `RoomDescriptionField` read CURRENT_ROOM and ROOMS in its render
// body, so ANY room-state write re-rendered it, which is what supplied the
// periodic trigger. (It no longer reads them there, so the trigger is now a
// prop change; the fix is the handler either way, because it makes the field
// correct under any re-render.)
//
// The test drives a real room-state write from inside the open dialog and
// asserts the in-progress edit survives it.

const OWNED_ROOM = "Your Private Room";
const DRAFT = "A description I am still in the middle of typing";

async function waitForApp(page: Page) {
  await page.waitForSelector(".app-root", { timeout: 30_000 });
  await expect(page.locator("aside, .app-root button")).not.toHaveCount(0);
}

async function openRoomDetails(page: Page) {
  const vp = page.viewportSize();
  if (vp && vp.width < 1024) {
    await page.setViewportSize({ width: 1280, height: vp.height });
  }

  // Scope to the room list: once a room is selected the header button carries
  // the room name as its accessible name too, which would be a second match.
  const roomBtn = page.getByTestId("room-list").getByRole("button", { name: OWNED_ROOM });
  await expect(roomBtn).toBeVisible({ timeout: 10_000 });
  await roomBtn.click();
  await expect(page.getByRole("heading", { name: OWNED_ROOM })).toBeVisible({ timeout: 5_000 });

  await page.getByTitle("Room details").click();
  await expect(page.getByTestId("edit-room-modal")).toBeVisible({ timeout: 5_000 });
}

/**
 * Commit a new max-members value WITHOUT moving focus.
 *
 * This is the whole point of the setup. Clicking or tabbing to another field
 * would blur the description textarea, which fires its `onchange` and commits
 * the draft — the field would then hold the right text for the wrong reason and
 * the test would pass even against the broken build. Dispatching the event
 * programmatically leaves focus (and the uncommitted draft) exactly where the
 * user had it.
 *
 * `change` bubbles, so dioxus's delegated root listener picks it up and runs
 * the real `update_max_members` handler: sign the config, apply the delta to
 * ROOMS, mark the room for sync. A genuine room-state write, not a fake one.
 */
async function commitMaxMembersWithoutBlurring(page: Page, newValue: number) {
  await page.getByTestId("max-members-input").evaluate((el, value) => {
    const input = el as HTMLInputElement;
    input.value = String(value);
    input.dispatchEvent(new Event("change", { bubbles: true }));
  }, newValue);
}

test.describe("Room Details: in-progress typing survives room-state updates", () => {
  test.beforeEach(async ({ page }) => {
    await page.goto("/");
    await waitForApp(page);
  });

  test("a room-state write does not wipe the half-typed description", async ({ page }) => {
    await openRoomDetails(page);

    const description = page.getByTestId("room-description-input");
    await expect(description).toBeVisible();
    await expect(description).toBeEnabled();

    // Type like a user: real keystrokes, no blur, nothing committed yet.
    await description.click();
    await description.pressSequentially(DRAFT, { delay: 10 });
    await expect(description).toHaveValue(DRAFT);
    await expect(description).toBeFocused();

    // Read the current cap so the write we trigger is guaranteed to change it
    // (`update_max_members` early-returns when the value is unchanged, which
    // would leave nothing to re-render and make this test vacuous).
    // `Number("")` is 0 and finite, so assert a real positive cap rather than
    // just finiteness, or an empty read would silently change what we assert.
    const maxMembers = page.getByTestId("max-members-input");
    const currentCap = Number(await maxMembers.inputValue());
    expect(currentCap).toBeGreaterThan(0);
    const newCap = currentCap + 7;

    await commitMaxMembersWithoutBlurring(page, newCap);

    // ANTI-VACUITY GATE. The label is rendered from the room state, so it only
    // reaches the new cap once the delta has actually been signed, applied to
    // ROOMS and re-rendered through the dialog. If this fails the write never
    // landed and the assertion below would prove nothing.
    await expect(
      page.getByTestId("edit-room-modal").getByText(new RegExp(`Members \\(\\d+/${newCap}\\)`)),
    ).toBeVisible({ timeout: 15_000 });

    // The actual regression: the description must still hold the uncommitted
    // draft. Before the fix the re-render above reset it to the stored value
    // (empty, for this fixture room).
    await expect(description).toHaveValue(DRAFT);
    await expect(description).toBeFocused();
  });

  test("the max-members field also survives a room-state write", async ({ page }) => {
    // MaxMembersField re-renders on its `member_count` prop as well as on
    // `config`, so a member joining while the owner edited the cap was enough
    // to wipe it. Covered separately because test 1 drives the room-state write
    // THROUGH this field, so it can never observe this field being clobbered.
    await openRoomDetails(page);

    const maxMembers = page.getByTestId("max-members-input");
    const typedCap = String(Number(await maxMembers.inputValue()) + 42);
    await maxMembers.click();
    await maxMembers.fill("");
    await maxMembers.pressSequentially(typedCap, { delay: 10 });
    await expect(maxMembers).toHaveValue(typedCap);
    await expect(maxMembers).toBeFocused();

    // Same trick as test 1, but driven through the ROOM NAME field, so the
    // write is observable somewhere this test did not write to itself.
    // Committing the description here instead would be a vacuous gate: the
    // synthetic event sets the textarea's DOM value directly, so asserting
    // that value proves only that the test wrote it.
    const renamed = "Renamed While Editing";
    await page.getByTestId("room-name-input").evaluate((el, value) => {
      const input = el as HTMLInputElement;
      input.value = value;
      input.dispatchEvent(new Event("change", { bubbles: true }));
    }, renamed);

    // ANTI-VACUITY GATE: the room heading renders from room state, so it can
    // only show the new name once the delta was signed, applied to ROOMS and
    // re-rendered through the tree.
    await expect(page.getByRole("heading", { name: renamed })).toBeVisible({ timeout: 15_000 });

    // The cap being typed must still be there, and still uncommitted.
    await expect(maxMembers).toHaveValue(typedCap);
    await expect(maxMembers).toBeFocused();
  });

  test("the draft still commits normally on blur after a room-state write", async ({ page }) => {
    // Guards the other direction: `oninput` must not have broken the `onchange`
    // save path, so a description typed through an update still persists.
    await openRoomDetails(page);

    const description = page.getByTestId("room-description-input");
    await description.click();
    await description.pressSequentially(DRAFT, { delay: 10 });

    const maxMembers = page.getByTestId("max-members-input");
    const newCap = Number(await maxMembers.inputValue()) + 3;
    await commitMaxMembersWithoutBlurring(page, newCap);
    await expect(
      page.getByTestId("edit-room-modal").getByText(new RegExp(`Members \\(\\d+/${newCap}\\)`)),
    ).toBeVisible({ timeout: 15_000 });

    // Blur to commit (the description saves on `onchange`), then close and
    // reopen the dialog. Reopening remounts RoomDescriptionField, so it seeds
    // its signal afresh from the stored configuration: the value can only come
    // back if the save actually landed.
    await page.getByTestId("room-name-input").click();
    await expect(description).not.toBeFocused();

    // Wait for the DESCRIPTION save specifically to land before closing. It is
    // async (sign -> defer -> ROOMS write), and a reopen that won that race
    // would reseed the remounted field from the OLD config and never recover,
    // since `use_signal` seeds once: a permanent failure the retry below could
    // not rescue.
    //
    // The room header renders the description from room state, and this test
    // never writes there, so it is real proof the save round-tripped. (Gating
    // on the Members label instead would be a no-op: it already reached
    // `newCap` above and the description save cannot move it.)
    await expect(page.getByTestId("room-header-description")).toContainText(DRAFT, {
      timeout: 15_000,
    });

    await page.getByTestId("edit-room-close-button").click();
    await expect(page.getByTestId("edit-room-modal")).toBeHidden({ timeout: 5_000 });

    await page.getByTitle("Room details").click();
    await expect(page.getByTestId("edit-room-modal")).toBeVisible({ timeout: 5_000 });
    await expect(page.getByTestId("room-description-input")).toHaveValue(DRAFT, {
      timeout: 10_000,
    });
  });
});
