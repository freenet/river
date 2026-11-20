#!/usr/bin/env node
// River against a real, isolated Freenet node (scripts/real-node/node.sh),
// through one node restart. Checks what a user sees:
//
//   - River stays on screen through the outage (rooms, chat, composer);
//   - no document reload of the gateway page or River's frame;
//   - an unsent draft survives in the same input;
//   - River reconnects by itself, and the draft then sends exactly once;
//   - the connection pill never slides its label over an empty dot slot.
//
// Usage (from the River repo root, node running in local mode with River
// published: see node.sh):
//
//   node scripts/real-node/check.mjs restart [--down 3]     stop, wait, start
//   node scripts/real-node/check.mjs rejoin  [--joining 5]  restart into a peer
//        that has not rejoined the network (contract pages answer 503), then
//        into a ready node
//   options: --width 1280 (390 for a phone), --headed
//
// Every wait has a short deadline and fails with what it saw. Outages are kept
// short on purpose: River's own reconnect backoff starts at 3 s and doubles to
// 60 s, so a long outage measures that backoff, not recovery.
import { createRequire } from 'node:module';
import { execFileSync } from 'node:child_process';
import { fileURLToPath } from 'node:url';
import { dirname, join } from 'node:path';

const here = dirname(fileURLToPath(import.meta.url));
const repo = join(here, '..', '..');
const require = createRequire(join(repo, 'ui', 'tests', 'package.json'));
const { chromium } = require('@playwright/test');

const argv = process.argv.slice(2);
const scenario = argv[0];
const opt = (name, dflt) => {
  const i = argv.indexOf(`--${name}`);
  return i > 0 ? Number(argv[i + 1]) : dflt;
};
if (!['restart', 'rejoin'].includes(scenario)) {
  console.error('usage: check.mjs restart|rejoin [--down S] [--joining S] [--width PX] [--headed]');
  process.exit(2);
}
const width = opt('width', 1280);
const DEADLINE = { connect: 20_000, reconnect: 30_000, send: 15_000 };

const node = (...args) => execFileSync(join(here, 'node.sh'), args, { cwd: repo, encoding: 'utf8' }).trim();
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const T0 = Date.now();
const t = () => `${((Date.now() - T0) / 1000).toFixed(1)}s`;
const log = (...a) => console.log(`[${t()}]`, ...a);

if (!node('status').startsWith('running')) {
  console.error('node.sh: node not running. Start it: scripts/real-node/node.sh start local');
  process.exit(2);
}
const url = node('url');

const browser = await chromium.launch({ headless: !argv.includes('--headed') });
const page = await browser.newPage({ viewport: { width, height: 844 } });

// Every document load after the baseline is a failure.
const navigations = [];
page.on('request', (r) => {
  if (r.isNavigationRequest()) navigations.push({ at: Date.now(), main: r.frame() === page.mainFrame(), url: r.url() });
});
// The pill check runs every frame in River's document.
const pillGaps = [];
const wsEvents = [];
page.on('console', (m) => {
  if (m.text().startsWith('PILL-GAP ')) pillGaps.push(m.text().slice(9));
  if (m.text().startsWith('WS ')) wsEvents.push({ at: Date.now(), what: m.text().slice(3) });
});
await page.addInitScript(() => {
  if (!location.search.includes('__sandbox=1')) return;
  // What River's sockets are told, as the gateway page relays it.
  window.addEventListener('message', (e) => {
    const m = e.data;
    if (m && m.__freenet_ws__ && (m.type === 'open' || m.type === 'close' || m.type === 'error')) {
      console.log(`WS ${m.type}${m.type === 'close' ? ` ${m.code}` : ''}`);
    }
  });
  const tick = () => {
    for (const p of document.querySelectorAll('[data-testid="connection-status-indicator"]')) {
      if (!p.getClientRects().length) continue;
      const slot = p.querySelector('[data-testid="connection-activity-dots"]');
      const dot = p.querySelector('[data-testid="connection-status-dot"]');
      const label = p.querySelector('[data-testid="connection-status-label"]');
      if (!slot || !dot || !label) continue;
      const clip = slot.querySelector('.connection-activity-clip')?.getBoundingClientRect();
      const shown = clip
        ? [...slot.querySelectorAll('.small-dots > span')].filter((s) => {
            const r = s.getBoundingClientRect();
            return r.left >= clip.left - 0.5 && r.right <= clip.right + 0.5;
          }).length
        : 0;
      const push = label.getBoundingClientRect().left - dot.getBoundingClientRect().right - 8;
      if (push >= 8 && shown === 0 && Number(getComputedStyle(slot).opacity) > 0.2) {
        const last = slot.querySelector('.small-dots > span:last-child')?.getBoundingClientRect();
        console.log(
          `PILL-GAP label pushed ${push.toFixed(1)}px, no dot shown (${label.textContent}; ` +
            `clip ${clip ? `${clip.left.toFixed(1)}-${clip.right.toFixed(1)}` : 'none'}, ` +
            `last dot ${last ? `${last.left.toFixed(1)}-${last.right.toFixed(1)}` : 'none'})`,
        );
      }
    }
    requestAnimationFrame(tick);
  };
  requestAnimationFrame(tick);
});

// Command sockets, as the gateway page opens them for River.
const sockets = [];
page.on('websocket', (ws) => {
  const u = new URL(ws.url());
  if (!u.pathname.endsWith('/contract/command')) return;
  const s = { opened: Date.now(), closed: null };
  sockets.push(s);
  ws.on('close', () => (s.closed = Date.now()));
});
const app = () => page.frames().find((f) => f.url().includes('__sandbox=1'));
async function until(what, ms, fn) {
  const end = Date.now() + ms;
  let last;
  while (Date.now() < end) {
    try {
      last = await fn();
      if (last) return last;
    } catch (e) {
      last = String(e).split('\n')[0];
    }
    await sleep(200);
  }
  throw new Error(`${what}: not within ${ms / 1000}s (last: ${JSON.stringify(last)})`);
}
const screen = async () =>
  app().evaluate(() => {
    const shown = (sel) => [...document.querySelectorAll(sel)].filter((e) => e.getClientRects().length).length;
    // The pill is hidden on a phone while a room is open; its state is the same.
    const labels = [...document.querySelectorAll('[data-testid="connection-status-label"]')];
    const label = labels.find((e) => e.getClientRects().length) ?? labels[0];
    return {
      rooms: shown('[data-testid^="room-item-"]'),
      messages: shown('[data-testid="message-body"]'),
      composer: shown('[data-testid="message-input"]'),
      status: label?.textContent ?? null,
    };
  });
const connected = async () => (await screen()).status === 'Connected';

// --- Baseline: River connected, a room open, a draft typed.
await page.goto(url);
await until('River connects', DEADLINE.connect, connected);
const f = app();
const rail = f.getByTestId('hamburger-rooms-button').filter({ visible: true });
const room = f.locator('[data-testid^="room-item-"]');
if ((await f.locator('[data-testid^="room-item-"]').count()) === 0) {
  if (await rail.count()) await rail.first().click();
  await f.getByTestId('create-room-button').first().click();
  await f.getByTestId('create-room-name-input').fill('real-node check');
  await f.getByTestId('create-room-nickname-input').fill('tester');
  await f.getByTestId('create-room-submit-button').click();
  await room.first().waitFor({ timeout: DEADLINE.connect });
  log('created a room; giving the delegate a moment to store it');
  await sleep(3000);
} else {
  if (await rail.count()) await rail.first().click();
  await room.first().click();
}
const input = f.getByTestId('message-input');
await input.waitFor({ timeout: DEADLINE.connect });
const draft = `draft ${scenario} ${Date.now()}`;
await input.fill(draft);
const marker = `m${Math.random().toString(36).slice(2)}`;
await page.evaluate((m) => (window.__check = m), marker);
await f.evaluate((m) => (window.__check = m), marker);
const inputHandle = await input.elementHandle();
const navBase = navigations.length;
const before = await screen();
log('baseline', JSON.stringify(before));

// --- The outage. River's screen is sampled throughout.
const samples = [];
let sampling = true;
const sampler = (async () => {
  while (sampling) {
    samples.push(await screen().catch((e) => ({ error: String(e).split('\n')[0] })));
    await sleep(250);
  }
})();
if (scenario === 'restart') {
  node('stop');
  log(`node stopped for ${opt('down', 3)}s`);
  await sleep(opt('down', 3) * 1000);
  log(node('start', 'local'));
} else {
  log(node('start', 'joining'));
  const r = await fetch(url, { headers: { 'Sec-Fetch-Dest': 'document' } });
  log(`contract page while joining: HTTP ${r.status}`);
  await sleep(opt('joining', 5) * 1000);
  log(node('start', 'local'));
}
const upAt = Date.now();
let reconnectedAfter = null;
try {
  await until('River reconnects', DEADLINE.reconnect, connected);
  reconnectedAfter = (Date.now() - upAt) / 1000;
} catch (e) {
  log(String(e.message));
}
await sleep(1000); // past River's own 500 ms auth-reload timer
sampling = false;
await sampler;

// --- Results.
const rel = (ms) => `${((ms - upAt) / 1000).toFixed(1)}s`;
log('River socket events (relative to node ready): ' + wsEvents.filter((e) => e.at >= upAt - 60_000).map((e) => `${rel(e.at)} ${e.what}`).join(', '));
log(
  'command sockets since the outage (relative to node ready): ' +
    sockets.filter((s) => s.opened >= upAt - 60_000).map((s) => `${rel(s.opened)}→${s.closed ? rel(s.closed) : 'open'}`).join(', '),
);
const frameNow = app();
const results = [];
const check = (name, ok, detail = '') => results.push({ name, ok, detail });
const loads = navigations.slice(navBase);
check('no document reload', loads.length === 0, loads.map((n) => `${n.main ? 'page' : 'frame'} ${n.url.replace(/authToken=[^&]+/, '')}`).join(', '));
check('gateway page kept', (await page.evaluate(() => window.__check).catch(() => null)) === marker);
check('River document kept', !!frameNow && (await frameNow.evaluate(() => window.__check).catch(() => null)) === marker);
const stayed = samples.filter((s) => !s.error && s.composer > 0 && (s.messages > 0 || s.rooms > 0)).length;
check('River on screen throughout', stayed === samples.length, `${stayed}/${samples.length} samples`);
const draftKept = await inputHandle.evaluate((el) => el.isConnected && el.value).catch(() => false);
check('draft kept in the same input', draftKept === draft);
check('reconnected', reconnectedAfter !== null, reconnectedAfter !== null ? `${reconnectedAfter.toFixed(1)}s after the node was ready` : '');
if (reconnectedAfter !== null && frameNow) {
  if (draftKept !== draft) await frameNow.getByTestId('message-input').fill(draft);
  await frameNow.getByTestId('send-message-button').click();
  const sent = await until('the draft is delivered', DEADLINE.send, async () => {
    const c = await frameNow.evaluate((d) => ({
      shown: [...document.querySelectorAll('[data-testid="message-body"]')].filter((e) => e.textContent.includes(d)).length,
      pending: document.querySelectorAll('[data-testid="pending-message"]').length,
    }), draft);
    return c.shown >= 1 && c.pending === 0 ? c : null;
  }).catch((e) => ({ error: e.message }));
  await sleep(2000);
  const finalCount = await frameNow.evaluate((d) => [...document.querySelectorAll('[data-testid="message-body"]')].filter((e) => e.textContent.includes(d)).length, draft).catch(() => -1);
  check('draft sent exactly once', !sent.error && finalCount === 1, sent.error ?? `${finalCount} shown`);
}
check('pill never slides over an empty slot', pillGaps.length === 0, [...new Set(pillGaps)].slice(0, 3).join('; '));
await browser.close();

console.log(`\n${scenario} (width ${width}), node: ${node('status')}`);
for (const r of results) console.log(`  ${r.ok ? 'PASS' : 'FAIL'}  ${r.name}${r.detail ? `  (${r.detail})` : ''}`);
process.exit(results.every((r) => r.ok) ? 0 : 1);
