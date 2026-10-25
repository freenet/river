import { test, expect } from "@playwright/test";
import { colourOf, contrast, effectiveBackground, openOwnMessageEdit, openRoomWithComposer, resolveColor } from "./example-room";

// The in-place edit form's Save is the dialogs' primary: raised neutral, not
// a solid accent fill (white on the dark accent is 2.15:1). Its label must
// clear AA on what is painted behind it, in both themes.
for (const scheme of ["light", "dark"] as const) {
  test(`${scheme}: the edit form's Save clears 4.5:1 and is no accent fill`, { tag: "@chromium-only" }, async ({ page }) => {
    await page.emulateMedia({ colorScheme: scheme });
    await openRoomWithComposer(page);
    await openOwnMessageEdit(page);
    const save = page.getByTestId("edit-save-button");
    await expect(save).toBeVisible();
    const behind = await effectiveBackground(save);
    const label = await colourOf(save);
    expect(contrast(label, behind), "Save's label").toBeGreaterThanOrEqual(4.5);
    const fill = await resolveColor(page, await save.evaluate((el) => getComputedStyle(el).backgroundColor));
    expect(fill, "Save is not the accent").not.toEqual(await resolveColor(page, "var(--color-accent)"));
    expect(parseFloat(await save.evaluate((el) => getComputedStyle(el).borderTopWidth))).toBeGreaterThanOrEqual(1);
    // Cancel is the quiet ghost: no fill at rest.
    const cancel = await resolveColor(page, await page.getByTestId("edit-cancel-button").evaluate((el) => getComputedStyle(el).backgroundColor));
    expect(cancel[3]).toBe(0);
  });
}
