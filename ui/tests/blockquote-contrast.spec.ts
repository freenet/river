import { test, expect } from "@playwright/test";
import { colourOf, contrast, effectiveBackground, over, waitForApp } from "./example-room";
import { openMemberInfoForFirstNonSelf } from "./invite-picker";

// Quoted text (`> …` Markdown) on the tinted outgoing DM bubble, the app's
// worst quote surface (the app-wide muted token was 3.96:1 there in light
// mode). It must clear AA and stay dimmer than the bubble's other text.
// message-contrast.spec.ts covers quotes in room messages.

const AA_TEXT = 4.5;

for (const colorScheme of ["dark", "light"] as const) {
  test.describe(`Blockquote contrast (${colorScheme} mode)`, () => {
    // Desktop on every project: mobile hides the conversation behind view switching.
    test.use({ colorScheme, viewport: { width: 1280, height: 900 } });

    test("quoted text in an outgoing DM bubble is readable and still muted", async ({ page }) => {
      // Example data ships no DMs, so send one with a quote.
      await page.goto("/");
      await waitForApp(page);
      await openMemberInfoForFirstNonSelf(page);
      await page.getByTestId("member-info-dm-button").click();
      await page.getByPlaceholder("Type a direct message...").fill("They wrote:\n\n> this is quoted text\n\nMy reply.");
      await page.keyboard.press("Enter");
      const quoteEl = page.locator("#dm-scroll-container blockquote").first();
      await expect(quoteEl).toBeAttached({ timeout: 15_000 });
      const bubble = quoteEl.locator("xpath=ancestor::*[contains(@class, 'bg-accent')][1]");
      const normalEl = bubble.locator("p:not(blockquote p)").first();

      const background = await effectiveBackground(bubble);
      const quote = contrast(over(await colourOf(quoteEl), background), background);
      const normal = contrast(over(await colourOf(normalEl), background), background);
      expect(quote, "quoted text clears AA").toBeGreaterThanOrEqual(AA_TEXT);
      // Without this bound, a missing light `--color-text-quote` would inherit body text and pass.
      expect(quote, "a quote stays dimmer than its bubble's text").toBeLessThan(normal * 0.8);
    });
  });
}
