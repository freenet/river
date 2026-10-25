import { test, expect } from "@playwright/test";
import { colourOf, contrast, effectiveBackground, over, waitForApp } from "./example-room";
import { openMemberInfoForFirstNonSelf } from "./invite-picker";

// Quoted text (`> …` Markdown) in your own DM row, on the own-row tint (the
// DM thread's worst quote surface; it was the tinted outgoing bubble before
// DMs became rows). It must clear AA and stay dimmer than the row's other
// text. message-contrast.spec.ts covers quotes in room messages.

const AA_TEXT = 4.5;

for (const colorScheme of ["dark", "light"] as const) {
  test.describe(`Blockquote contrast (${colorScheme} mode)`, () => {
    // Desktop on every project: mobile hides the conversation behind view switching.
    test.use({ colorScheme, viewport: { width: 1280, height: 900 } });

    test("quoted text in your own DM row is readable and still muted", async ({ page }) => {
      // Example data ships no DMs, so send one with a quote.
      await page.goto("/");
      await waitForApp(page);
      await openMemberInfoForFirstNonSelf(page);
      await page.getByTestId("member-info-dm-button").click();
      await page.getByPlaceholder("Type a direct message...").fill("They wrote:\n\n> this is quoted text\n\nMy reply.");
      await page.keyboard.press("Enter");
      const quoteEl = page.locator("#dm-scroll-container blockquote").first();
      await expect(quoteEl).toBeAttached({ timeout: 15_000 });
      const row = quoteEl.locator("xpath=ancestor::*[@data-testid='dm-row'][1]");
      await expect(row, "premise: the quote is in your own row").toHaveAttribute("data-message-self", "true");
      const normalEl = row.locator("p:not(blockquote p)").first();

      const background = await effectiveBackground(row);
      const quote = contrast(over(await colourOf(quoteEl), background), background);
      const normal = contrast(over(await colourOf(normalEl), background), background);
      expect(quote, "quoted text clears AA").toBeGreaterThanOrEqual(AA_TEXT);
      // Without this bound, a missing light `--color-text-quote` would inherit body text and pass.
      expect(quote, "a quote stays dimmer than its row's text").toBeLessThan(normal * 0.8);
    });
  });
}
