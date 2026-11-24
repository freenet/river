// Drive the River UI headless on a rehearsal node.
//
//   node ui.js seed <page-url> <out-prefix> <room-name> [invitation-code ...]
//     create one room in the UI, join every invitation, wait for the rooms to
//     show and the delegate writes to settle.
//   node ui.js send <page-url> <out-prefix> <room-owner-b58> [text ...]
//     open the room, post each text through the composer, wait for it to show
//     as a message bubble, and record every bubble the UI shows afterwards
//     (result.bubbles). With no text it only reads the room.
//   node ui.js load <page-url> <out-prefix> <max-seconds>
//     load the app, record the console until the crate walk reports, then
//     list the rooms the UI shows.
// Writes <out-prefix>.log (console) and <out-prefix>.json (result).
//
// The app runs in a sandboxed iframe: page.textContent reads the SHELL, so
// everything below goes through the app frame.
const fs = require('fs');
const { chromium } = require(process.env.PLAYWRIGHT_MODULE || __dirname + '/pw/node_modules/playwright');

const [mode, url, prefix, ...rest] = process.argv.slice(2);
const lines = [];
const t0 = Date.now();
const stamp = () => ((Date.now() - t0) / 1000).toFixed(1);
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

async function appFrame(page) {
  for (let i = 0; i < 120; i++) {
    for (const f of page.frames()) {
      if (f === page.mainFrame()) continue;
      try {
        if (await f.$('[data-testid="rooms-rail"]')) return f;
      } catch (_) {}
    }
    await sleep(500);
  }
  throw new Error('app frame never showed the rooms rail');
}

async function roomKeys(frame) {
  return frame.$$eval('[data-testid^="room-item-"]', (els) =>
    els.map((e) => ({ key: e.getAttribute('data-testid').slice('room-item-'.length), text: e.innerText.trim().split('\n')[0] })));
}

async function waitRooms(frame, n, maxMs) {
  const end = Date.now() + maxMs;
  let rooms = [];
  while (Date.now() < end) {
    rooms = await roomKeys(frame);
    if (rooms.length >= n) return rooms;
    await sleep(1000);
  }
  return rooms;
}

(async () => {
  const browser = await chromium.launch({ headless: true });
  const page = await browser.newPage();
  page.on('console', (m) => lines.push(`[${stamp()}s] [${m.type()}] ${m.text().replace(/%c/g, '').replace(/ color:.*$/, '')}`));
  page.on('pageerror', (e) => lines.push(`[${stamp()}s] [pageerror] ${e.message}`));
  const result = { mode };
  let code = 0;
  try {
    await page.goto(url);
    const frame = await appFrame(page);
    lines.push(`[${stamp()}s] [harness] app frame ready`);
    if (mode === 'seed') {
      const [roomName, ...codes] = rest;
      // Let the initial load settle so the create is not raced by it.
      await sleep(8000);
      await frame.click('[data-testid="create-room-button"]');
      await frame.fill('[data-testid="create-room-name-input"]', roomName);
      await frame.fill('[data-testid="create-room-nickname-input"]', 'uiuser');
      await frame.click('[data-testid="create-room-submit-button"]');
      let want = 1;
      result.after_create = await waitRooms(frame, want, 60000);
      for (const c of codes) {
        await frame.click('[data-testid="join-with-code-button"]');
        await frame.fill('[data-testid="join-with-code-input"]', c);
        await frame.click('[data-testid="join-with-code-submit-button"]');
        await frame.waitForSelector('[data-testid="receive-invitation-nickname-input"]', { timeout: 60000 });
        await frame.fill('[data-testid="receive-invitation-nickname-input"]', 'uiuser');
        await frame.click('[data-testid="receive-invitation-accept-button"]');
        want += 1;
        result[`after_join_${want}`] = await waitRooms(frame, want, 90000);
      }
      await sleep(15000); // delegate CAS writes and room PUTs settle
      result.rooms = await roomKeys(frame);
      if (result.rooms.length < want) code = 3;
    } else if (mode === 'send') {
      const [owner, ...texts] = rest;
      await sleep(8000);
      const rooms = await waitRooms(frame, 1, 90000);
      result.rooms = rooms;
      const sel = `[data-testid="room-item-${owner}"]`;
      await frame.waitForSelector(sel, { timeout: 60000 });
      await frame.click(sel);
      await frame.waitForSelector('[data-testid="message-input"]', { timeout: 60000 });
      const bubbles = () => frame.$$eval('[data-testid="message-bubble"]', (els) => els.map((e) => e.innerText.replace(/\s+/g, ' ').trim()));
      result.bubbles_before = await bubbles();
      result.sent = [];
      for (const t of texts) {
        await frame.fill('[data-testid="message-input"]', t);
        await frame.click('[data-testid="send-message-button"]');
        const end = Date.now() + 60000;
        let shown = false;
        while (Date.now() < end) { if ((await bubbles()).some((b) => b.includes(t))) { shown = true; break; } await sleep(1000); }
        result.sent.push({ text: t, shown });
        if (!shown) code = 5;
      }
      await sleep(texts.length ? 10000 : 5000); // let the update reach the node
      result.bubbles = await bubbles();
    } else if (mode === 'load') {
      const max = Number(rest[0] || 180) * 1000;
      let walkedAt = null;
      while (Date.now() - t0 < max) {
        if (walkedAt === null && lines.some((l) => /crate-migration: walk (complete|incomplete)/.test(l))) walkedAt = Date.now();
        if (walkedAt !== null && Date.now() - walkedAt > 20000) break;
        await sleep(500);
      }
      result.walk_reported = walkedAt !== null;
      result.walk_lines = lines.filter((l) => /crate-migration/.test(l));
      result.rooms = await roomKeys(frame);
      if (walkedAt === null) code = 2;
    }
  } catch (e) {
    lines.push(`[${stamp()}s] [harness-error] ${e.stack || e}`);
    result.error = String(e);
    code = 4;
  }
  fs.writeFileSync(prefix + '.log', lines.join('\n') + '\n');
  fs.writeFileSync(prefix + '.json', JSON.stringify(result, null, 1) + '\n');
  await browser.close();
  process.exit(code);
})();
