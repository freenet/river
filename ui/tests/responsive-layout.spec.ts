import { test, expect, Page } from "@playwright/test";

// Helper: wait for WASM app to fully render
async function waitForApp(page: Page) {
  await page.waitForSelector(".app-root", { timeout: 30_000 });
  // Wait for at least one interactive element to confirm WASM hydration
  await expect(page.locator("aside, .app-root button")).not.toHaveCount(0);
}

// Helper: select a room at any viewport width.
// On desktop the room list is always visible. On mobile we may need to
// navigate to the Rooms view first (via hamburger button).
async function selectRoom(page: Page, roomName: string) {
  const roomBtn = page.getByRole("button", { name: roomName });

  if (!(await roomBtn.isVisible({ timeout: 500 }).catch(() => false))) {
    // Try the hamburger in the chat header (visible when a room IS selected on mobile)
    const hamburger = page.locator(
      ".border-b.border-border.bg-panel button >> nth=0"
    );
    if (await hamburger.isVisible({ timeout: 500 }).catch(() => false)) {
      await hamburger.click();
      await expect(roomBtn).toBeVisible({ timeout: 5_000 });
    } else {
      // No room selected yet on mobile — the room list panel is hidden.
      // Temporarily resize to desktop to select the room, then resize back.
      const vp = page.viewportSize();
      if (vp && vp.width < 768) {
        await page.setViewportSize({ width: 1280, height: vp.height });
        await expect(roomBtn).toBeVisible({ timeout: 5_000 });
        await roomBtn.click();
        await expect(
          page.getByRole("heading", { name: roomName })
        ).toBeVisible({ timeout: 5_000 });
        await page.setViewportSize({ width: vp.width, height: vp.height });
        return;
      }
    }
  }

  await roomBtn.click();
  await expect(
    page.getByRole("heading", { name: roomName })
  ).toBeVisible({ timeout: 5_000 });
}

test.describe("Desktop layout (1280px)", () => {
  test.use({ viewport: { width: 1280, height: 800 } });

  test("shows 3-column layout with room selected", async ({ page }) => {
    await page.goto("/");
    await waitForApp(page);
    await selectRoom(page, "Team Chat Room");

    await expect(
      page.locator("aside").filter({ hasText: "Rooms" })
    ).toBeVisible();
    await expect(
      page.locator("aside").filter({ hasText: "Active Members" })
    ).toBeVisible();
    await expect(
      page.getByRole("heading", { name: "Team Chat Room" })
    ).toBeVisible();
  });

  test("no horizontal scrollbar", async ({ page }) => {
    await page.goto("/");
    await waitForApp(page);
    await selectRoom(page, "Team Chat Room");

    const hasHScroll = await page.evaluate(
      () =>
        document.documentElement.scrollWidth >
        document.documentElement.clientWidth
    );
    expect(hasHScroll).toBe(false);
  });
});

test.describe("Tablet layout (768px)", () => {
  test.use({ viewport: { width: 768, height: 1024 } });

  test("shows all panels with narrower sidebars", async ({ page }) => {
    await page.goto("/");
    await waitForApp(page);
    await selectRoom(page, "Team Chat Room");

    await expect(
      page.locator("aside").filter({ hasText: "Rooms" })
    ).toBeVisible();
    await expect(
      page.locator("aside").filter({ hasText: "Active Members" })
    ).toBeVisible();
  });

  test("no horizontal scrollbar", async ({ page }) => {
    await page.goto("/");
    await waitForApp(page);
    await selectRoom(page, "Team Chat Room");

    const hasHScroll = await page.evaluate(
      () =>
        document.documentElement.scrollWidth >
        document.documentElement.clientWidth
    );
    expect(hasHScroll).toBe(false);
  });
});

test.describe("Breakpoint boundary (767px)", () => {
  test.use({ viewport: { width: 767, height: 1024 } });

  test("at 767px only active panel is shown (mobile mode)", async ({
    page,
  }) => {
    await page.goto("/");
    await waitForApp(page);
    await selectRoom(page, "Team Chat Room");

    // At 767px (below md: breakpoint), only chat should be visible
    await expect(
      page.getByRole("heading", { name: "Team Chat Room" })
    ).toBeVisible();

    // Sidebars should be hidden
    await expect(
      page.locator("aside").filter({ hasText: "Rooms" })
    ).not.toBeVisible();
    await expect(
      page.locator("aside").filter({ hasText: "Active Members" })
    ).not.toBeVisible();

    // No horizontal scrollbar
    const hasHScroll = await page.evaluate(
      () =>
        document.documentElement.scrollWidth >
        document.documentElement.clientWidth
    );
    expect(hasHScroll).toBe(false);
  });
});

test.describe("Mobile layout (480px)", () => {
  test.use({ viewport: { width: 480, height: 844 } });

  test("shows only chat panel by default", async ({ page }) => {
    await page.goto("/");
    await waitForApp(page);
    await selectRoom(page, "Team Chat Room");

    await expect(
      page.getByRole("heading", { name: "Team Chat Room" })
    ).toBeVisible();
    await expect(
      page.getByPlaceholder("Type your message...")
    ).toBeVisible();
  });

  test("hamburger opens room list, room click returns to chat", async ({
    page,
  }) => {
    await page.goto("/");
    await waitForApp(page);
    await selectRoom(page, "Team Chat Room");

    // Open hamburger
    const header = page.locator(".border-b.border-border.bg-panel");
    await header.locator("button").first().click();

    // Room list should be visible
    await expect(
      page.locator("aside").filter({ hasText: "Rooms" })
    ).toBeVisible();

    // Click a different room
    await page
      .getByRole("button", { name: "Your Private Room" })
      .click();

    // Should be back in chat with new room
    await expect(
      page.getByRole("heading", { name: "Your Private Room" })
    ).toBeVisible();
    await expect(
      page.getByPlaceholder("Type your message...")
    ).toBeVisible();
  });

  test("members button opens members panel with back button", async ({
    page,
  }) => {
    await page.goto("/");
    await waitForApp(page);
    await selectRoom(page, "Team Chat Room");

    // Click members button (last button in header)
    const header = page.locator(".border-b.border-border.bg-panel");
    await header.locator("button").last().click();

    // Members panel visible
    await expect(
      page.locator("aside").filter({ hasText: "Active Members" })
    ).toBeVisible();

    // Back button returns to chat
    const memberPanel = page
      .locator("aside")
      .filter({ hasText: "Active Members" });
    await memberPanel.locator("button").first().click();

    await expect(
      page.getByPlaceholder("Type your message...")
    ).toBeVisible();
  });

  test("no horizontal scrollbar", async ({ page }) => {
    await page.goto("/");
    await waitForApp(page);
    await selectRoom(page, "Team Chat Room");

    const hasHScroll = await page.evaluate(
      () =>
        document.documentElement.scrollWidth >
        document.documentElement.clientWidth
    );
    expect(hasHScroll).toBe(false);
  });
});

test.describe("Small mobile layout (320px)", () => {
  test.use({ viewport: { width: 320, height: 568 } });

  test("content is readable, no overflow", async ({ page }) => {
    await page.goto("/");
    await waitForApp(page);
    await selectRoom(page, "Team Chat Room");

    const hasHScroll = await page.evaluate(
      () =>
        document.documentElement.scrollWidth >
        document.documentElement.clientWidth
    );
    expect(hasHScroll).toBe(false);

    await expect(
      page.getByRole("heading", { name: "Team Chat Room" })
    ).toBeVisible();
    await expect(
      page.getByPlaceholder("Type your message...")
    ).toBeVisible();
  });
});

test.describe("Desktop recovery after mobile", () => {
  test("resizing from mobile to desktop restores 3-column layout", async ({
    page,
  }) => {
    await page.setViewportSize({ width: 480, height: 844 });
    await page.goto("/");
    await waitForApp(page);
    await selectRoom(page, "Team Chat Room");

    // Switch to members view on mobile
    const header = page.locator(".border-b.border-border.bg-panel");
    await header.locator("button").last().click();
    await expect(
      page.locator("aside").filter({ hasText: "Active Members" })
    ).toBeVisible();

    // Resize to desktop
    await page.setViewportSize({ width: 1280, height: 800 });

    // All three panels should be visible
    await expect(
      page.locator("aside").filter({ hasText: "Rooms" })
    ).toBeVisible();
    await expect(
      page.locator("aside").filter({ hasText: "Active Members" })
    ).toBeVisible();
    await expect(
      page.getByPlaceholder("Type your message...")
    ).toBeVisible();
  });
});

test.describe("Sandboxed iframe embedding", () => {
  // The Freenet gateway uses: sandbox="allow-scripts allow-forms allow-popups"
  // (without allow-same-origin, giving the iframe an opaque origin).
  // For local testing, we add allow-same-origin so the WASM can load from
  // the dev server. The layout behavior we're testing (position:fixed,
  // flex containers, overflow) is the same regardless.
  // PR #142 broke because of structural CSS issues inside iframes, not
  // because of origin restrictions.
  const sandboxAttrs =
    "allow-scripts allow-forms allow-popups allow-same-origin";

  test("app renders inside a sandboxed iframe", async ({ page }) => {
    await page.setContent(`
      <!DOCTYPE html>
      <html>
      <body style="margin:0;padding:0;height:100vh;">
        <iframe
          sandbox="${sandboxAttrs}"
          src="http://localhost:8082"
          style="width:100%;height:100%;border:none;"
        ></iframe>
      </body>
      </html>
    `);

    const iframe = page.frameLocator("iframe");
    await iframe.locator(".app-root").waitFor({ timeout: 30_000 });
    await expect(
      iframe.getByRole("heading", { name: "Rooms" })
    ).toBeVisible();
  });

  test("3-column layout works inside sandboxed iframe at desktop width", async ({
    page,
  }) => {
    await page.setViewportSize({ width: 1280, height: 800 });
    await page.setContent(`
      <!DOCTYPE html>
      <html>
      <body style="margin:0;padding:0;height:100vh;">
        <iframe
          sandbox="${sandboxAttrs}"
          src="http://localhost:8082"
          style="width:100%;height:100%;border:none;"
        ></iframe>
      </body>
      </html>
    `);

    const iframe = page.frameLocator("iframe");
    await iframe.locator(".app-root").waitFor({ timeout: 30_000 });

    // Both sidebars and the main area should be visible
    await expect(
      iframe.locator("aside").filter({ hasText: "Rooms" })
    ).toBeVisible();

    // Verify no horizontal overflow inside the iframe
    const hasHScroll = await iframe.locator("html").evaluate(
      (el) => el.scrollWidth > el.clientWidth
    );
    expect(hasHScroll).toBe(false);
  });
});
