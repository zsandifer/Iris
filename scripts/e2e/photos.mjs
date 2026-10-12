// Shared photo viewer regression against the real PWA, without a backend.
// NODE_PATH=/path/to/node_modules node scripts/e2e/photos.mjs
import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import { createServer } from 'node:http';
import { createRequire } from 'node:module';
import { fileURLToPath } from 'node:url';
import { join } from 'node:path';

const require = createRequire((process.env.NODE_PATH || process.cwd()) + '/');
const { webkit, devices } = require('playwright');
const web = fileURLToPath(new URL('../../web/', import.meta.url));
const cid = '1'.repeat(32), uid = '2'.repeat(32), aid = '3'.repeat(32);
const posts = [{ id: '4'.repeat(32), title: 'Sample landscape', body: 'A sample photo post.',
  topic: 'Outdoors', images: ['/photo-wide.svg', '/photo-tall.svg'], created_at: 1700000000000 }];
const messages = [{ id: '5'.repeat(32), conversation_id: cid, role: 'user', author_user_id: uid,
  kind: 'text', status: 'done', content: 'A sample photo.', attachments: [aid], created_at: 1700000000000 },
{ id: '6'.repeat(32), conversation_id: cid, role: 'assistant', kind: 'card', status: 'done',
  card: { type: 'result', title: 'Sample result', image_url: '/photo-wide.svg' }, created_at: 1700000001000 }];
const api = (p) => {
  if (p === '/api/me') return { user: { id: uid, is_owner: true, display_name: 'Owner' }, main_conversation_id: cid };
  if (p === '/api/conversations') return { conversations: [{ id: cid, kind: 'main', member_count: 1 }] };
  if (p === `/api/conversations/${cid}/messages`) return { messages, has_more: false };
  if (p === '/api/settings') return { agent_name: 'Iris' };
  if (p === '/api/feed') return { posts, has_more: false };
  if (p === '/api/attachments') return { attachments: [{ id: aid, conversation_id: cid }], has_more: false };
  if (p === '/api/library') return { items: [{ id: '7'.repeat(32), title: 'Sample generated photo', mime: 'image/svg+xml' }], has_more: false };
  return {};
};
let attachmentRequests = 0;
const server = createServer(async (req, res) => {
  const p = new URL(req.url, 'http://localhost').pathname;
  if (p.endsWith('.svg') && p.startsWith('/photo-') || p === `/api/attachments/${aid}` || p.endsWith('/content')) {
    if (p.startsWith('/api/')) {
      assert.equal(req.headers.authorization, 'Bearer synthetic-token');
      attachmentRequests++;
    }
    const tall = p.includes('tall');
    res.writeHead(200, { 'Content-Type': 'image/svg+xml' });
    res.end(`<svg xmlns="http://www.w3.org/2000/svg" width="${tall ? 600 : 900}" height="${tall ? 1200 : 600}"><rect width="100%" height="100%" fill="#7c5cf6"/><circle cx="200" cy="200" r="100" fill="#eee"/></svg>`);
    return;
  }
  if (p.startsWith('/api/')) {
    res.writeHead(200, { 'Content-Type': 'application/json' }); res.end(JSON.stringify(api(p))); return;
  }
  try {
    const data = await readFile(join(web, p === '/' ? 'index.html' : p));
    res.writeHead(200, { 'Content-Type': p.endsWith('.js') ? 'text/javascript' : p.endsWith('.css') ? 'text/css'
      : p === '/' ? 'text/html' : 'application/octet-stream' });
    res.end(data);
  } catch { res.writeHead(404); res.end(); }
});
const settle = (page) => page.evaluate(() => new Promise((r) => requestAnimationFrame(() => requestAnimationFrame(r))));
const metrics = (page) => page.locator('.photo-layer').evaluate((el) => {
  const m = new DOMMatrix(getComputedStyle(el.querySelector('img')).transform);
  return { scale: m.a, x: m.e, y: m.f, width: el.clientWidth, height: el.clientHeight };
});
const clamped = (m) => {
  assert.ok(m.scale >= 1 && m.scale <= 4, JSON.stringify(m));
  assert.ok(Math.abs(m.x) <= m.width * (m.scale - 1) / 2 + 1, JSON.stringify(m));
  assert.ok(Math.abs(m.y) <= m.height * (m.scale - 1) / 2 + 1, JSON.stringify(m));
};

await new Promise((r) => server.listen(0, '127.0.0.1', r));
let browser;
try {
  browser = await webkit.launch({ executablePath: process.env.WEBKIT || undefined });
  const context = await browser.newContext({ ...devices['iPhone 13'], serviceWorkers: 'block' });
  await context.addInitScript(() => {
    Object.defineProperty(navigator, 'standalone', { value: true });
    localStorage.setItem('iris.token', 'synthetic-token');
  });
  const page = await context.newPage();
  const errors = [];
  page.on('pageerror', (e) => errors.push(e.message));
  await page.routeWebSocket('**/api/ws', (ws) => ws.onMessage(() => {}));
  await page.goto(`http://127.0.0.1:${server.address().port}/`, { waitUntil: 'domcontentloaded' });
  await page.locator('.photo img').waitFor();
  const chatPhoto = page.locator('.photo img');
  await chatPhoto.evaluate((el) => el.decode());
  await page.evaluate(() => {
    document.documentElement.style.setProperty('--sat', '47px');
    document.documentElement.style.setProperty('--sab', '34px');
    // WebKit exposes only single-touch tap via Playwright. Inject touch
    // lists for multi-touch while using native taps for opening/double tap.
    window.photoTouch = (type, points) => {
      const e = new Event(type, { bubbles: true, cancelable: true });
      Object.defineProperty(e, 'touches', { value: points.map(([identifier, clientX, clientY]) => ({ identifier, clientX, clientY })) });
      document.querySelector('.photo-layer').dispatchEvent(e);
      return e.defaultPrevented;
    };
  });
  const opened = async () => {
    if (process.env.PHOTO_DEBUG) console.log('Waiting for photo dialog');
    await page.locator('.photo-layer img').waitFor();
    await page.waitForFunction(() => document.querySelector('.photo-layer')?.clientWidth > 0);
    // Wait for the short entry animation before measuring bounds/gestures.
    await page.waitForFunction(() => document.querySelector('.photo-layer').getAnimations().every((a) => a.playState === 'finished'));
    assert.equal(await page.locator('.photo-viewer').getAttribute('role'), 'dialog');
    assert.equal(await page.locator('.photo-viewer').getAttribute('aria-modal'), 'true');
    assert.equal(await page.locator('.photo-close').evaluate((el) => document.activeElement === el), true);
    assert.equal(await page.locator('#app').evaluate((el) => el.inert), true);
    assert.equal(await page.locator('.chat .scr').count() ? await page.locator('.chat .scr').evaluate((el) => getComputedStyle(el).overflowY) : 'hidden', 'hidden');
    const rect = await page.locator('.photo-layer').boundingBox();
    assert.ok(rect.x >= 15 && rect.y >= 62);
    assert.ok(rect.x + rect.width <= 375 && rect.y + rect.height <= 795);
    const btn = await page.locator('.photo-close').boundingBox();
    assert.ok(btn.width >= 44 && btn.height >= 44 && btn.y >= 47);
    clamped(await metrics(page));
  };
  const closed = async () => {
    if (process.env.PHOTO_DEBUG) console.log('Waiting for photo dismissal');
    await page.locator('.photo-viewer').waitFor({ state: 'detached' });
    assert.equal(await page.locator('#app').evaluate((el) => el.inert), false);
    assert.equal(await page.evaluate(() => !!history.state?.irisPhoto), false);
  };
  const route = await page.evaluate(() => location.href);
  const historyLength = await page.evaluate(() => history.length);
  await chatPhoto.tap(); await opened();
  assert.equal(await page.evaluate(() => history.length), historyLength + 1);
  assert.equal(await page.locator('.photo-layer img').getAttribute('alt'), 'Photo');
  await page.locator('.photo-close').tap(); await closed();
  assert.equal(await page.locator('.photo').evaluate((el) => document.activeElement === el), true);
  assert.equal(await page.evaluate(() => location.href), route);
  await chatPhoto.tap(); await opened();
  await page.touchscreen.tap(5, 400); await closed();
  await chatPhoto.tap(); await opened();
  await page.keyboard.press('Escape'); await closed();
  await chatPhoto.tap(); await opened();
  await page.goBack(); await closed();
  assert.equal(await page.evaluate(() => location.href), route);

  await chatPhoto.tap(); await opened();
  let box = await page.locator('.photo-layer').boundingBox();
  const x = box.x + box.width * .65, y = box.y + box.height * .4;
  await page.touchscreen.tap(x, y);
  assert.equal((await metrics(page)).scale, 1, 'one tap leaves the photo open at 1×');
  await page.touchscreen.tap(x, y); await settle(page);
  let m = await metrics(page);
  assert.equal(m.scale, 2.5); assert.ok(m.x < 0 && m.y > 0); clamped(m);
  await page.evaluate(() => {
    photoTouch('touchstart', [[1, 180, 400]]);
    photoTouch('touchmove', [[1, 5000, -5000]]);
    photoTouch('touchend', []);
  });
  await settle(page); m = await metrics(page); clamped(m);
  assert.ok(Math.abs(m.x - m.width * .75) < 1 && Math.abs(m.y + m.height * .75) < 1);
  await page.evaluate(() => {
    photoTouch('touchstart', [[1, 140, 400], [2, 240, 400]]);
    photoTouch('touchmove', [[1, 40, 450], [2, 340, 450]]);
    photoTouch('touchend', []);
  });
  await settle(page); m = await metrics(page); assert.equal(m.scale, 4); clamped(m);
  await page.evaluate(() => {
    photoTouch('touchstart', [[1, 40, 400], [2, 340, 400]]);
    photoTouch('touchmove', [[1, 180, 400], [2, 200, 400]]);
    photoTouch('touchend', []);
  });
  await settle(page); assert.equal((await metrics(page)).scale, 1); clamped(await metrics(page));
  // Pinch from center while its midpoint moves, and verify the anchor.
  box = await page.locator('.photo-layer').boundingBox();
  const cx = box.x + box.width / 2, cy = box.y + box.height / 2;
  const prevented = await page.evaluate(({ cx, cy }) => {
    photoTouch('touchstart', [[1, cx - 40, cy], [2, cx + 40, cy]]);
    const stopped = photoTouch('touchmove', [[1, cx - 70, cy + 20], [2, cx + 90, cy + 20]]);
    photoTouch('touchend', []);
    return stopped;
  }, { cx, cy });
  await settle(page); m = await metrics(page);
  assert.equal(prevented, true); assert.equal(m.scale, 2);
  assert.ok(Math.abs(m.x - 10) < 1 && Math.abs(m.y - 20) < 1); clamped(m);
  assert.equal(await page.evaluate(() => visualViewport.scale), 1);
  await page.touchscreen.tap(cx, cy); await page.touchscreen.tap(cx, cy); await settle(page);
  assert.equal((await metrics(page)).scale, 1);
  // Cancel and then pan must never become an accidental double tap.
  await page.evaluate(({ cx, cy }) => {
    photoTouch('touchstart', [[1, cx, cy]]); photoTouch('touchcancel', []);
    photoTouch('touchstart', [[1, cx, cy]]); photoTouch('touchmove', [[1, cx + 50, cy]]); photoTouch('touchend', []);
  }, { cx, cy });
  await settle(page); assert.equal((await metrics(page)).scale, 1);
  await page.keyboard.press('Tab');
  assert.equal(await page.locator('.photo-close').evaluate((el) => document.activeElement === el), true);
  await page.locator('.photo-close').tap(); await closed();

  await page.locator('.res-im').tap(); await opened();
  assert.equal(await page.locator('.photo-layer img').getAttribute('alt'), 'Sample result');
  await page.keyboard.press('Escape'); await closed();
  await page.getByRole('button', { name: 'For you', exact: true }).tap();
  const feedPhoto = page.locator('.post .car img').first();
  await feedPhoto.waitFor(); await feedPhoto.tap(); await opened();
  assert.equal(await page.locator('.photo-layer img').getAttribute('alt'), posts[0].title);
  await page.locator('.photo-close').tap(); await closed();
  // A cancelled pointer sequence followed by a compatibility click must
  // leave the carousel free to scroll and never open a photo.
  await page.locator('.car').evaluate((el) => {
    const img = el.querySelector('img');
    img.dispatchEvent(new PointerEvent('pointerdown', { bubbles: true, clientX: 300, clientY: 300 }));
    img.dispatchEvent(new PointerEvent('pointermove', { bubbles: true, clientX: 100, clientY: 300 }));
    img.dispatchEvent(new PointerEvent('pointercancel', { bubbles: true }));
    el.scrollLeft = el.clientWidth;
    img.dispatchEvent(new MouseEvent('click', { bubbles: true, cancelable: true, detail: 1 }));
  });
  await settle(page);
  assert.equal(await page.locator('.photo-viewer').count(), 0);
  assert.ok(await page.locator('.car').evaluate((el) => el.scrollLeft > 0));
  await page.locator('.post .car img').nth(1).tap(); await opened();
  clamped(await metrics(page)); await page.touchscreen.tap(5, 400); await closed();

  // Open the story using its label, then tap the image; Escape closes only
  // the photo and leaves the paused story available underneath.
  await page.locator('.stile small').tap();
  await page.locator('.viewer-im').tap({ position: { x: 195, y: 250 } }); await opened();
  assert.equal(await page.locator('.bars b').evaluate((el) => getComputedStyle(el).animationPlayState), 'paused');
  await page.keyboard.press('Escape'); await closed();
  assert.equal(await page.locator('.viewer').count(), 1);
  await page.locator('.viewer-top .x').tap();

  await page.getByRole('button', { name: 'Library', exact: true }).tap();
  await page.locator('.grid img[alt="Sample generated photo"]').waitFor();
  assert.equal(await page.locator('.grid img[alt="Sample generated photo"]').evaluate(async (el) => {
    await el.decode();
    await new Promise((r) => setTimeout(r, 400));
    return el.isConnected;
  }), true, 'authenticated Library thumbnails stay mounted after loading');
  await page.locator('.grid img[alt="Sample generated photo"]').tap(); await opened();
  await page.keyboard.press('Escape'); await closed();
  await page.getByRole('button', { name: 'Photos', exact: true }).tap();
  await page.locator('.grid img').tap(); await opened();
  assert.equal(await page.getByRole('button', { name: 'Open chat', exact: true }).count(), 1);
  await page.getByRole('button', { name: 'Open chat', exact: true }).tap();
  await page.locator('.photo-viewer').waitFor({ state: 'detached' });
  await page.locator('.chat').waitFor();

  await page.emulateMedia({ reducedMotion: 'reduce' });
  await chatPhoto.tap();
  assert.equal(await page.locator('.photo-viewer').evaluate((el) => getComputedStyle(el).animationName), 'none');
  assert.equal(await page.locator('.photo-layer').evaluate((el) => getComputedStyle(el).transitionDuration), '0s');
  await page.keyboard.press('Escape'); await closed();
  assert.ok(attachmentRequests > 0);
  assert.deepEqual(errors, []);
  console.log('PASS: WebKit iPhone photo surfaces, dismissals/history/focus, safe areas, pinch/double tap/pan bounds, carousel, story pause and reduced motion.');
} finally {
  await browser?.close();
  server.closeAllConnections();
  await new Promise((r) => server.close(r));
}
