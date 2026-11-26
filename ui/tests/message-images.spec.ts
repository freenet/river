import { test, expect } from "@playwright/test";
import { openRoomWithComposer, settle, submitMessage } from "./example-room";
import { callRiverTest } from "./river-test";

// Images are not displayed in messages: no syntax variant loads or shows one.

const HOST = "images.river-test.invalid";
const URL_OF = (name: string) => `https://${HOST}/${name}.png`;

const VARIANTS: [string, string][] = [
  ["inline", `![inline alt](${URL_OF("inline")})`],
  ["titled", `![titled alt](${URL_OF("titled")} "a title")`],
  ["reference", `![reference alt][r1]\n\n[r1]: ${URL_OF("reference")}`],
  ["collapsed reference", `![collapsed][]\n\n[collapsed]: ${URL_OF("collapsed")}`],
  ["linked image", `[![linked alt](${URL_OF("linked")})](https://example.com/page)`],
  ["data uri", `![data alt](data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR4nGP4z8DwHwAFAAH/q842iQAAAABJRU5ErkJggg==)`],
  ["raw img", `<img src="${URL_OF("rawimg")}">`],
  ["picture", `<picture><source srcset="${URL_OF("picture")}"><img src="${URL_OF("picture")}"></picture>`],
  ["svg image", `<svg><image href="${URL_OF("svg")}"/></svg>`],
  ["object", `<object data="${URL_OF("object")}"></object>`],
  ["embed", `<embed src="${URL_OF("embed")}">`],
  ["iframe", `<iframe src="${URL_OF("iframe")}"></iframe>`],
  ["css background", `<div style="background-image:url(${URL_OF("css")})">styled</div>`],
  ["autolink", `<${URL_OF("autolink")}>`],
  ["bare url", `bare ${URL_OF("bare")} link`],
];

async function noImagesFor(page: import("@playwright/test").Page, send: (text: string) => Promise<void>) {
  const requested: string[] = [];
  page.on("request", (r) => {
    if (r.url().includes(HOST)) requested.push(r.url());
  });
  // Would show an image if the page asked for one.
  await page.route(`**://${HOST}/**`, (route) =>
    route.fulfill({
      status: 200,
      contentType: "image/png",
      body: Buffer.from(
        "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR4nGP4z8DwHwAFAAH/q842iQAAAABJRU5ErkJggg==",
        "base64",
      ),
    }),
  );
  await openRoomWithComposer(page);
  for (const [i, [, text]] of VARIANTS.entries()) {
    await send(`variant-${i} ${text}`);
  }
  await settle(page);
  const history = page.getByTestId("conversation-history");
  await expect(history).toContainText("variant-0");
  await expect(history).toContainText(`variant-${VARIANTS.length - 1}`);
  await page.waitForTimeout(500);
  expect(requested, "no request is made for an image URL").toEqual([]);
  await expect(history.locator("img")).toHaveCount(0);
  await expect(history.locator("picture, object, embed, iframe, svg image, [style*='background']")).toHaveCount(0);
}

test.describe("Images in messages", () => {
  test("received messages in every image syntax make no request and show no image", async ({ page }) => {
    await noImagesFor(page, async (text) => {
      await callRiverTest(page, "appendMessage", text);
    });
  });

  test("sent messages in every image syntax make no request and show no image", async ({ page }) => {
    await noImagesFor(page, async (text) => {
      await submitMessage(page, text);
    });
  });

  test("an inline image shows its alt text and address as text", async ({ page }) => {
    await openRoomWithComposer(page);
    await callRiverTest(page, "appendMessage", `look ![a cat](${URL_OF("cat")}) here`);
    const body = page.getByTestId("message-body").filter({ hasText: "look" }).last();
    await expect(body).toContainText("a cat");
    await expect(body).toContainText(URL_OF("cat"));
    await expect(body.locator("img")).toHaveCount(0);
  });
});
