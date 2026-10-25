import { test, expect } from "@playwright/test";
import { openRoomWithComposer } from "./example-room";

// Lato is bundled (fonts.rs): both weights load from the webapp itself, so
// every OS draws the same face. A missing or blocked file would leave the
// family resolving to the system fallback with nothing else failing.
test("Lato 400 and 700 load from the bundle", async ({ page }) => {
  await openRoomWithComposer(page);
  const fonts = await page.evaluate(async () => {
    await document.fonts.ready;
    // Force both weights to load, as rendering would.
    await Promise.all([document.fonts.load("400 15px Lato"), document.fonts.load("700 15px Lato")]);
    return {
      regular: document.fonts.check("400 15px Lato"),
      bold: document.fonts.check("700 15px Lato"),
      faces: Array.from(document.fonts)
        .filter((f) => /lato/i.test(f.family))
        .map((f) => ({ weight: f.weight, status: f.status })),
    };
  });
  expect(fonts.faces.length, "premise: the Lato faces are declared").toBeGreaterThanOrEqual(2);
  expect(fonts.faces.filter((f) => f.status === "error"), "no Lato face failed to load").toEqual([]);
  for (const weight of ["400", "700"]) {
    expect(fonts.faces.some((f) => f.weight === weight && f.status === "loaded"), `Lato ${weight} loaded`).toBe(true);
  }
  expect(fonts.regular, "document.fonts.check 400").toBe(true);
  expect(fonts.bold, "document.fonts.check 700").toBe(true);
});
