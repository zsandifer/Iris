// Capture app screens and composite them into phone frames for the README.
// Run through scripts/readme-images.sh, which seeds fake demo data and
// starts a server:  node shoot.mjs <base url> <pairing code> <work dir> <out dir>
import { createRequire } from 'node:module';
import { mkdirSync, readFileSync } from 'node:fs';

const require = createRequire((process.env.NODE_PATH || process.cwd()) + '/');
const { chromium, devices } = require('playwright');

const [base, code, work, out] = process.argv.slice(2);
mkdirSync(work, { recursive: true });
mkdirSync(out, { recursive: true });
const browser = await chromium.launch({ executablePath: process.env.CHROME || undefined });
const phone = {
  viewport: { width: 390, height: 844 },
  deviceScaleFactor: 2,
  isMobile: true,
  hasTouch: true,
  userAgent: devices['iPhone 13'].userAgent,
};
const errors = [];

// Pair once; every capture reuses the device token.
const stateFile = `${work}/state.json`;
{
  const ctx = await browser.newContext(phone);
  const p = await ctx.newPage();
  await p.goto(base + '/');
  await p.fill('.code-in', code);
  await p.click('button.b1');
  await p.waitForSelector('.top');
  await ctx.storageState({ path: stateFile });
  await ctx.close();
}

async function capture(name, scheme, setup) {
  const ctx = await browser.newContext({ ...phone, colorScheme: scheme, storageState: stateFile });
  await ctx.addInitScript(() => localStorage.setItem('iris.notify.dismissed', '1'));
  const page = await ctx.newPage();
  page.on('pageerror', (e) => errors.push(`${name}: ${e.message}`));
  page.on('console', (m) => m.type() === 'error' && errors.push(`${name}: ${m.text()}`));
  let socket;
  await page.routeWebSocket(/\/api\/ws$/, (ws) => {
    ws.connectToServer();
    socket = ws;
  });
  const inject = (ev) => socket.send(JSON.stringify({ at: Date.now(), ...ev }));
  await page.goto(base + '/');
  await page.waitForSelector('.top');
  await page.waitForFunction(() => document.querySelector('.st')?.textContent !== 'reconnecting…');
  const store = () => page.evaluate(async () => {
    const { S } = await import('/js/store.js');
    return { main: S.me.main_conversation_id, convs: S.convs };
  });
  await setup(page, inject, store);
  await page.waitForTimeout(700);
  const file = `${work}/${name}-${scheme}.png`;
  await page.screenshot({ path: file });
  await ctx.close();
  return file;
}

const go = (page, hash) => page.evaluate((h) => { location.hash = h; }, hash);
const live = (cid, at) => ({
  id: 'live-demo', conversation_id: cid, role: 'assistant', author_user_id: null, kind: 'text', content: '',
  attachments: [], card: null, status: 'streaming', harness: null, model: null, effort: null, meta: {},
  created_at: at, updated_at: at,
});

async function chatWithStatus(page, inject, store) {
  const { main } = await store();
  await page.waitForSelector('.umsg');
  inject({ type: 'message.created', conversation_id: main, message: live(main, Date.now()) });
  inject({ type: 'message.activity', conversation_id: main, message_id: 'live-demo',
    activity: { id: 'demo-1', kind: 'mcp', name: 'calendar/list_events', status: 'started', summary: 'tomorrow morning' } });
  await page.waitForSelector('.status');
}

async function openChat(page, store, title) {
  const { convs } = await store();
  await go(page, `#/chat/${convs.find((c) => c.title === title).id}`);
  await page.waitForSelector('.card-wrap');
}

const shots = {};
for (const scheme of ['light', 'dark']) {
  shots[`chat-${scheme}`] = await capture('chat', scheme, chatWithStatus);
}
shots.feed = await capture('feed', 'light', async (page) => {
  await go(page, '#/foryou');
  await page.waitForSelector('.post img');
});
shots.story = await capture('story', 'light', async (page) => {
  await go(page, '#/foryou');
  await page.click('.stile >> nth=0');
  await page.waitForSelector('.viewer img');
  await page.waitForTimeout(1400);
});
shots.approval = await capture('approval', 'light', async (page, inject, store) => {
  await openChat(page, store, 'Friday dinner');
  await page.waitForSelector('.ap');
});
shots.hub = await capture('hub', 'light', async (page) => {
  await go(page, '#/iris');
  await page.waitForSelector('.hub-hero');
});
shots.ai = await capture('ai-engine', 'light', async (page) => {
  await go(page, '#/settings/ai');
  await page.waitForSelector('.rad');
});
shots.reconnect = await capture('reconnect', 'light', async (page, inject, store) => {
  const { main } = await store();
  await page.waitForSelector('.umsg');
  const at = Date.now();
  inject({ type: 'message.created', conversation_id: main, message: {
    ...live(main, at), id: 'signin-demo', kind: 'card', status: 'complete', content: 'Reconnect Claude',
    card: { type: 'engine_signin', title: 'Reconnect Claude', harness: 'claude', flow: 'code_paste', owner_only: true,
      body: "Claude on your Iris server is signed out. Until then I'm using a different engine, so replies may feel a bit different.",
      state: 'waiting_for_code', url: 'https://claude.ai/oauth/authorize?demo=1', action_label: 'Reconnect' } } });
  await page.waitForSelector('.steps-list');
});

// ---------------------------------------------------------------- frames

const data = (f) => `data:image/png;base64,${readFileSync(f).toString('base64')}`;

async function frame(name, files, scale = 1) {
  const page = await browser.newPage({ deviceScaleFactor: 2, viewport: { width: 1800, height: 1200 } });
  const w = Math.round(390 * scale);
  const h = Math.round(844 * scale);
  const r = Math.round(46 * scale);
  await page.setContent(`<html><body style="margin:0;background:transparent">
    <div id="row" style="display:inline-flex;gap:${Math.round(44 * scale)}px;padding:${Math.max(6, Math.round(9 * scale)) + 2}px">
    ${files.map((f) => `<div style="width:${w}px;height:${h}px;border-radius:${r}px;overflow:hidden;flex:none;
      box-shadow:0 0 0 ${Math.max(6, Math.round(9 * scale))}px #1d1a22">
      <img src="${data(f)}" style="width:100%;height:100%;display:block"></div>`).join('')}
    </div></body></html>`);
  await page.waitForTimeout(200);
  await page.locator('#row').screenshot({ path: `${out}/${name}.png`, omitBackground: true });
  await page.close();
}

await frame('hero', [shots['chat-light'], shots.feed, shots.approval], 0.62);
await frame('light-dark', [shots['chat-light'], shots['chat-dark']], 0.7);
await frame('chat', [shots['chat-light']], 0.8);
await frame('feed', [shots.feed], 0.8);
await frame('story', [shots.story], 0.8);
await frame('hub', [shots.hub], 0.8);
await frame('ai-engine', [shots.ai], 0.8);
await frame('reconnect-card', [shots.reconnect], 0.8);
await browser.close();
if (errors.length) {
  console.error('console errors:\n' + errors.join('\n'));
  process.exit(1);
}
console.log('screens captured without console errors');
