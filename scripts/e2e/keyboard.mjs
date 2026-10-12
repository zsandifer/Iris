// Standalone iPhone keyboard regression against the real app in WebKit.
// Requires an external Playwright install, just like approvals.mjs:
//   NODE_PATH=/path/to/node_modules node scripts/e2e/keyboard.mjs [frames.json]
// Set WEBKIT to override the browser executable. No real backend or CLI runs.
import assert from 'node:assert/strict';
import { readFile, writeFile } from 'node:fs/promises';
import { createServer } from 'node:http';
import { createRequire } from 'node:module';
import { fileURLToPath } from 'node:url';
import { join } from 'node:path';

const require = createRequire((process.env.NODE_PATH || process.cwd()) + '/');
const { webkit, devices } = require('playwright');
const web = fileURLToPath(new URL('../../web/', import.meta.url));
const cid = '1'.repeat(32), uid = '2'.repeat(32);
const messages = Array.from({ length: 90 }, (_, n) => ({
  id: (n + 100).toString(16).padStart(32, '0'), conversation_id: cid,
  role: n % 2 ? 'assistant' : 'user', author_user_id: uid,
  kind: 'text', status: 'done', content: `Sample message ${n} with enough text to scroll the chat.`,
  created_at: 1700000000000 + n * 1000,
}));
const api = (p) => {
  if (p === '/api/me') return { user: { id: uid, is_owner: true, display_name: 'Owner' }, main_conversation_id: cid };
  if (p === '/api/conversations') return { conversations: [{ id: cid, kind: 'main', member_count: 1 }] };
  if (p === `/api/conversations/${cid}/messages`) return { messages, has_more: false };
  if (p === '/api/settings') return { agent_name: 'Iris' };
  return {};
};
const server = createServer(async (req, res) => {
  const p = new URL(req.url, 'http://localhost').pathname;
  if (p.startsWith('/api/')) {
    res.writeHead(200, { 'Content-Type': 'application/json' });
    res.end(JSON.stringify(api(p)));
    return;
  }
  try {
    const data = await readFile(join(web, p === '/' ? 'index.html' : p));
    const type = p.endsWith('.js') ? 'text/javascript' : p.endsWith('.css') ? 'text/css'
      : p.endsWith('.png') ? 'image/png' : p === '/' ? 'text/html' : 'application/octet-stream';
    res.writeHead(200, { 'Content-Type': type });
    res.end(data);
  } catch {
    res.writeHead(404);
    res.end();
  }
});
const settle = (page) => page.evaluate(() => new Promise((r) => requestAnimationFrame(() => requestAnimationFrame(r))));

await new Promise((r) => server.listen(0, '127.0.0.1', r));
let browser;
try {
  browser = await webkit.launch({ executablePath: process.env.WEBKIT || undefined });
  const context = await browser.newContext({ ...devices['iPhone 13'],
    viewport: { width: 390, height: 844 }, serviceWorkers: 'block' });
  await context.addInitScript(() => {
    Object.defineProperty(navigator, 'standalone', { value: true });
    localStorage.setItem('iris.token', 'synthetic-token');
    // Playwright has no API for opening an iOS keyboard or panning the
    // visual viewport. Use actual window resizes plus synthetic pan metrics.
    window.mockViewport = { height: null, offset: 0 };
    const vv = visualViewport;
    const nativeHeight = Object.getOwnPropertyDescriptor(VisualViewport.prototype, 'height').get;
    Object.defineProperties(vv, {
      height: { get: () => mockViewport.height ?? nativeHeight.call(vv) },
      offsetTop: { get: () => mockViewport.offset },
    });
    window.focusCalls = [];
    const focus = HTMLElement.prototype.focus;
    HTMLElement.prototype.focus = function (options) {
      if (this.matches('.composer textarea')) focusCalls.push(options);
      return focus.call(this, options);
    };
  });
  const page = await context.newPage();
  const errors = [];
  page.on('pageerror', (e) => errors.push(e.message));
  await page.routeWebSocket('**/api/ws', (ws) => ws.onMessage(() => {}));
  await page.goto(`http://127.0.0.1:${server.address().port}/`, { waitUntil: 'domcontentloaded' });
  const textarea = page.locator('.composer textarea');
  await textarea.waitFor();
  await page.evaluate(async () => {
    document.documentElement.style.setProperty('--sat', '47px');
    document.documentElement.style.setProperty('--sab', '34px');
    const { set } = await import('/js/store.js');
    set({ settings: { agent_name: 'Iris', avatar_custom: true, avatar_url: '/icons/icon-192.png' } });
  });
  const avatar = page.locator('.top .avatar img');
  await avatar.waitFor();
  await avatar.evaluate((img) => img.decode());
  let avatarRequests = 0;
  page.on('request', (r) => { if (r.url().endsWith('/icons/icon-192.png')) avatarRequests++; });
  await page.evaluate(() => {
    window.avatarNode = document.querySelector('.top .avatar img');
    window.avatarSrc = avatarNode.currentSrc;
    window.avatarLoads = 0;
    avatarNode.addEventListener('load', () => avatarLoads++);
    window.headerFrames = [];
    window.recordHeader = true;
    const record = () => {
      const rect = document.querySelector('.top').getBoundingClientRect();
      // getBoundingClientRect uses layout viewport coordinates; subtract
      // offsetTop to measure the header's position on the visible screen.
      headerFrames.push({ phase: window.phase || 'focus', top: rect.top,
        visibleTop: rect.top - visualViewport.offsetTop, height: rect.height, scrollY });
      if (recordHeader) requestAnimationFrame(record);
    };
    requestAnimationFrame(record);
  });
  const box = await textarea.boundingBox();
  await page.touchscreen.tap(box.x + 16, box.y + 12);
  assert.equal(await textarea.evaluate((el) => document.activeElement === el), true);
  assert.equal(await page.evaluate(() => focusCalls[0]?.preventScroll), true);
  await textarea.fill('Sample text for caret placement');
  await textarea.evaluate((el) => el.blur());
  await page.touchscreen.tap(box.x + 8, box.y + 12);
  assert.ok(await textarea.evaluate((el) => el.selectionStart < el.value.length), 'native caret placement');
  // Composer controls and a second tap in the focused draft must not refocus.
  const focusCount = await page.evaluate(() => focusCalls.length);
  await page.locator('.composer .pl').dispatchEvent('pointerdown', { isPrimary: true, button: 0 });
  await page.touchscreen.tap(box.x + 16, box.y + 12);
  assert.equal(await page.evaluate(() => focusCalls.length), focusCount);

  await textarea.evaluate((el) => el.blur());
  await settle(page);
  const preShrink = await page.evaluate(() => {
    window.phase = 'pre-shrink';
    localStorage.setItem('iris.kb.portrait', '344');
    const el = document.querySelector('.composer textarea');
    el.dispatchEvent(new PointerEvent('pointerdown', { bubbles: true, isPrimary: true, button: 0 }));
    // These reads happen in the dispatching task, before any animation
    // frame or visualViewport notification can correct the layout.
    return { height: document.querySelector('#app').style.getPropertyValue('--app-height'),
      nav: getComputedStyle(document.querySelector('.nav')).display,
      top: document.querySelector('.top').getBoundingClientRect().top,
      bottom: el.getBoundingClientRect().bottom, focused: document.activeElement === el };
  });
  assert.equal(preShrink.height, '500px');
  assert.equal(preShrink.nav, 'none');
  assert.equal(preShrink.top, 0);
  assert.ok(preShrink.bottom <= 500, 'composer is above the future keyboard before focus');
  assert.equal(preShrink.focused, true);
  const predictedFrames = await page.evaluate(async () => {
    const samples = [];
    // iOS may keep reporting the full viewport for the opening animation.
    for (let n = 0; n < 3; n++) {
      await new Promise((r) => requestAnimationFrame(r));
      visualViewport.dispatchEvent(new Event('resize'));
      samples.push(document.querySelector('#app').style.getPropertyValue('--app-height'));
    }
    return samples;
  });
  assert.deepEqual(predictedFrames, ['500px', '500px', '500px']);
  const reconcileFrames = await page.evaluate(async () => {
    window.phase = 'pre-shrink-reconcile';
    const samples = [];
    for (const height of [620, 540, 480]) {
      await new Promise((resolve) => requestAnimationFrame(() => {
        mockViewport.height = height;
        visualViewport.dispatchEvent(new Event('resize'));
        samples.push({ height: document.querySelector('#app').getBoundingClientRect().height,
          top: document.querySelector('.top').getBoundingClientRect().top });
        resolve();
      }));
    }
    return samples;
  });
  assert.deepEqual(reconcileFrames, [620, 540, 480].map((height) => ({ height, top: 0 })));
  await page.waitForFunction(() => localStorage.getItem('iris.kb.portrait') === '364');
  await page.evaluate(() => {
    window.phase = 'pre-shrink-blur';
    document.querySelector('.composer textarea').blur();
    mockViewport.height = null;
    visualViewport.dispatchEvent(new Event('resize'));
  });
  await settle(page);
  assert.equal(await page.locator('#app').evaluate((el) => el.style.getPropertyValue('--app-height')), '844px');
  assert.equal(await page.locator('#app').evaluate((el) => el.classList.contains('keyboard-open')), false);
  assert.notEqual(await page.locator('.nav').evaluate((el) => getComputedStyle(el).display), 'none');
  // The touchstart fallback also pre-shrinks; blur must cancel it even if
  // the keyboard has never produced a resize.
  await textarea.dispatchEvent('touchstart');
  assert.equal(await page.locator('#app').evaluate((el) => el.style.getPropertyValue('--app-height')), '480px');
  const blurHeight = await textarea.evaluate((el) => {
    el.blur();
    return document.querySelector('#app').style.getPropertyValue('--app-height');
  });
  assert.equal(blurHeight, '844px');
  await settle(page);
  // Programmatic focus shares the prediction and the hardware-keyboard
  // fallback. With no resize, restore navigation after its 700ms deadline.
  await page.evaluate(async () => {
    const { focusComposer } = await import('/js/viewport.js');
    focusComposer(document.querySelector('.composer textarea'));
  });
  assert.equal(await page.locator('#app').evaluate((el) => el.style.getPropertyValue('--app-height')), '480px');
  await page.waitForFunction(() => !document.querySelector('#app').classList.contains('keyboard-open'));
  assert.equal(await page.locator('#app').evaluate((el) => el.style.getPropertyValue('--app-height')), '844px');

  await page.evaluate(() => { window.phase = 'resize'; });
  for (const height of [820, 780, 720, 660, 600, 540, 500]) {
    await page.setViewportSize({ width: 390, height });
    await settle(page);
  }
  assert.equal(await page.locator('#app').evaluate((el) => el.classList.contains('keyboard-open')), true);
  assert.equal(await page.locator('.nav').evaluate((el) => getComputedStyle(el).display), 'none');
  const documentSamples = await page.evaluate(async () => {
    window.phase = 'document-scroll';
    // Temporarily lift the overflow lock so injected document scrolls
    // really happen. Sample in the same frame, before any deferred fix.
    const rules = document.createElement('style');
    rules.textContent = 'html, body {position:static !important;overflow:visible !important;height:1800px !important}';
    document.head.append(rules);
    const samples = [];
    for (const y of [20, 80, 140, 60, 120]) {
      await new Promise((resolve) => requestAnimationFrame(() => {
        window.scrollTo(0, y);
        const attempted = scrollY;
        window.dispatchEvent(new Event('scroll'));
        samples.push({ attempted, scrollY, top: document.querySelector('.top').getBoundingClientRect().top });
        resolve();
      }));
    }
    rules.remove();
    return samples;
  });
  assert.ok(documentSamples.every((s) => s.attempted > 0), 'injected document scrolls took effect');
  assert.ok(documentSamples.every((s) => s.scrollY === 0 && s.top === 0), JSON.stringify(documentSamples));
  const panSamples = await page.evaluate(async () => {
    window.phase = 'visual-pan';
    const samples = [];
    for (const offset of [4, 18, 42.5, 60, 35, 12, 0]) {
      await new Promise((resolve) => requestAnimationFrame(() => {
        mockViewport.offset = offset;
        mockViewport.height = 500 - offset;
        // Height can change before WebKit delivers its resize notification.
        visualViewport.dispatchEvent(new Event('scroll'));
        const rect = document.querySelector('.top').getBoundingClientRect();
        samples.push({ offset, top: rect.top, visibleTop: rect.top - visualViewport.offsetTop,
          appHeight: document.querySelector('#app').getBoundingClientRect().height });
        visualViewport.dispatchEvent(new Event('resize'));
        resolve();
      }));
    }
    mockViewport.height = null;
    visualViewport.dispatchEvent(new Event('resize'));
    return samples;
  });
  assert.ok(panSamples.every((s) => s.visibleTop === 0), JSON.stringify(panSamples));
  assert.ok(panSamples.every((s) => s.appHeight === 500 - s.offset), JSON.stringify(panSamples));
  await textarea.fill('A multiline draft\n'.repeat(12));
  await settle(page);
  assert.ok(await textarea.evaluate((el) => el.getBoundingClientRect().bottom <= visualViewport.height));
  await textarea.evaluate((el) => el.blur());
  await page.evaluate(() => { window.phase = 'dismiss'; });
  for (const height of [560, 650, 740, 844]) {
    await page.setViewportSize({ width: 390, height });
    await settle(page);
  }
  assert.equal(await page.locator('#app').evaluate((el) => el.classList.contains('keyboard-open')), false);
  const result = await page.evaluate(() => {
    recordHeader = false;
    const header = document.querySelector('.top');
    const img = header.querySelector('img');
    return { frames: headerFrames, avatarSame: img === avatarNode && img.currentSrc === avatarSrc,
      avatarLoads, imageSize: [img.width, img.height], transition: getComputedStyle(header).transitionDuration,
      animation: getComputedStyle(header).animationName, fadeFilter: getComputedStyle(header, '::after').backdropFilter };
  });
  assert.ok(result.frames.every((f) => f.visibleTop === 0), JSON.stringify(result.frames));
  assert.ok(result.frames.filter((f) => f.phase !== 'visual-pan').every((f) => f.top === 0));
  assert.ok(result.frames.every((f) => f.height === 109));
  assert.equal(result.avatarSame, true);
  assert.equal(result.avatarLoads, 0);
  assert.equal(avatarRequests, 0);
  assert.deepEqual(result.imageSize, [36, 36]);
  assert.equal(result.transition, '0s');
  assert.equal(result.animation, 'none');
  assert.equal(result.fadeFilter, 'none');
  assert.deepEqual(errors, []);
  if (process.argv[2]) await writeFile(process.argv[2], JSON.stringify({ ...result, preShrink, predictedFrames,
    reconcileFrames, documentSamples, panSamples }, null, 2));
  console.log(`PASS: ${result.frames.length} WebKit iPhone frames, synchronous pre-shrink and zero visible header shift; caret, avatar, fade and navigation stable.`);
} finally {
  await browser?.close();
  await new Promise((r) => server.close(r));
}
