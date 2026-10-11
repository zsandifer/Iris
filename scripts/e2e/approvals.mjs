// End-to-end check of the connector gateway with a real engine, driving the
// app like a person on a phone (390x844). Run through
// scripts/e2e/connectors.sh, which starts a throwaway Iris with the fake
// messenger connector (scripts/e2e/fake_messenger_mcp.py; nothing is ever
// really sent).
//
//   node approvals.mjs <base url> <pairing code> <messenger log> <shots dir>
//
// Covers: a send held for approval and approved from the card (and that
// exactly the previewed message ran), denial, a trusted recipient, the
// sends-per-hour tripwire, an approval that times out, Pause Iris, and
// Disconnect everything.
import { createRequire } from 'node:module';
import { existsSync, mkdirSync, readFileSync } from 'node:fs';

const require = createRequire((process.env.NODE_PATH || process.cwd()) + '/');
const { chromium, devices } = require('playwright');

const [base, code, logFile, shots] = process.argv.slice(2);
mkdirSync(shots, { recursive: true });
const browser = await chromium.launch({ executablePath: process.env.CHROME || undefined });
const ctx = await browser.newContext({
  viewport: { width: 390, height: 844 }, deviceScaleFactor: 2, isMobile: true, hasTouch: true,
  userAgent: devices['iPhone 13'].userAgent,
});
await ctx.addInitScript(() => localStorage.setItem('iris.notify.dismissed', '1'));
const page = await ctx.newPage();
const errors = [];
page.on('pageerror', (e) => errors.push(e.message));
page.on('console', (m) => m.type() === 'error' && !/Failed to load resource/.test(m.text()) && errors.push(m.text()));
const results = [];
const check = (name, ok, detail = '') => {
  results.push({ name, ok: !!ok, detail });
  console.log(`${ok ? 'PASS' : 'FAIL'} ${name}${detail ? ` (${detail})` : ''}`);
};
const sent = () => (existsSync(logFile) ? readFileSync(logFile, 'utf8').trim().split('\n').filter(Boolean).map((l) => JSON.parse(l)) : []);

async function shot(name) {
  for (const scheme of ['light', 'dark']) {
    await page.emulateMedia({ colorScheme: scheme });
    await page.waitForTimeout(250);
    await page.screenshot({ path: `${shots}/${name}-${scheme}.png` });
  }
  await page.emulateMedia({ colorScheme: 'light' });
}

// The app's store, exposed once so waits can use plain (synchronous)
// predicates.
const store = () => page.evaluate(() => {
  const S = window.__S;
  const cid = S.me.main_conversation_id;
  return { cid, list: S.msgs[cid]?.list || [], settings: S.settings };
});

async function openChat() {
  await page.evaluate(() => { location.hash = `#/chat/${window.__S.me.main_conversation_id}`; });
  await page.waitForSelector('.composer textarea');
}

let asked = 0;

/** Send a message from the composer; later waits refer to it. */
async function say(text) {
  await openChat();
  await page.fill('.composer textarea', text);
  await page.click('.composer .send');
  await page.waitForFunction((t) => {
    const S = window.__S;
    return (S.msgs[S.me.main_conversation_id]?.list || []).some((m) => m.role === 'user' && m.content === t);
  }, text);
  const { list } = await store();
  asked = [...list].reverse().find((m) => m.role === 'user' && m.content === text).created_at;
}

/** Wait until the reply to the last message is finished; returns it. */
async function reply(timeout = 180000) {
  await page.waitForFunction((after) => {
    const S = window.__S;
    const list = S.msgs[S.me.main_conversation_id]?.list || [];
    const a = list.find((m) => m.role === 'assistant' && m.kind === 'text' && m.created_at > after);
    return a && a.status !== 'streaming';
  }, asked, { timeout, polling: 500 });
  const { list } = await store();
  return list.find((m) => m.role === 'assistant' && m.kind === 'text' && m.created_at > asked);
}

async function pendingCard(timeout = 180000) {
  await page.waitForSelector('.ap:not(.decided) .ap-bt', { timeout });
  await page.locator('.ap:not(.decided)').last().scrollIntoViewIfNeeded();
  return page.locator('.ap:not(.decided)').last();
}

// ---------------------------------------------------------------- pairing
await page.goto(base + '/');
await page.fill('.code-in', code);
await page.click('button.b1');
await page.waitForSelector('.top');
await page.waitForFunction(() => document.querySelector('.st')?.textContent !== 'reconnecting…');
await page.evaluate(async () => { window.__S = (await import('/js/store.js')).S; });

// --------------------------------------------------------- approve a send
await say('Use the messenger send_message tool right now to text sam@example.com exactly: "Running 10 minutes late, sorry!" Do not ask me first; Iris shows me an approval card.');
let card = await pendingCard();
const preview = await card.innerText();
check('send is held with a preview built from the arguments', preview.includes('sam@example.com') && preview.includes('Running 10 minutes late'), preview.replace(/\s+/g, ' ').slice(0, 160));
check('nothing ran before approval', !sent().some((c) => c.tool === 'send_message'));
await page.waitForSelector('.status.shimmer');
check('status says it is waiting', (await page.locator('.status.shimmer').last().innerText()).includes('Waiting for your OK'));
await shot('01-approval-pending');
await card.locator('button.ap-raw-btn').click();
check('raw arguments can be inspected', (await card.locator('.ap-raw').innerText()).includes('"to"'));
await card.locator('.ap-bt .y').click();
let r = await reply();
let log = sent();
const ran = log.find((c) => c.tool === 'send_message');
check('approved send ran exactly as previewed', ran && ran.arguments.to === 'sam@example.com' && ran.arguments.text === 'Running 10 minutes late, sorry!', JSON.stringify(ran?.arguments));
check('reply after approval', r.status === 'complete', r.content.slice(0, 100));
await shot('02-approved');

// ------------------------------------------------------------------ deny
await say('Now use the messenger send_message tool to text kim@example.com: "Package arrives Tuesday." Do not ask me first.');
card = await pendingCard();
await card.locator('.ap-bt button:not(.y)').click();
r = await reply();
check('denied send did not run', !sent().some((c) => c.arguments?.to === 'kim@example.com'));
check('agent was told it was denied', /n.t|denied|didn/i.test(r.content), r.content.slice(0, 120));
await shot('03-denied');

// --------------------------------------------------------------- trusted
await page.evaluate(() => { location.hash = '#/iris/connections/trusted'; });
await page.fill('input[inputmode=email]', 'sam@example.com');
await page.click('.code-row .b1');
await page.waitForSelector('text=sam@example.com');
await shot('04-trusted-people');
const beforeTrusted = sent().length;
await say('Use the messenger send_message tool to text sam@example.com: "Here now." Do not ask me first.');
r = await reply();
const { list: afterTrusted } = await store();
check('trusted recipient: no approval card', !afterTrusted.some((m) => m.card?.type === 'approval' && m.card.preview?.body === 'Here now.'));
check('trusted send ran', sent().length === beforeTrusted + 1 && sent().at(-1).arguments.text === 'Here now.');

// -------------------------------------------------------------- tripwire
await say('Use the messenger send_message tool twice, one call after the other, to text sam@example.com: first "One", then "Two". Do not ask me first.');
r = await reply();
const { list: afterTrip } = await store();
const pausedCard = afterTrip.find((m) => m.card?.type === 'action' && /paused/i.test(m.card.title));
check('tripwire paused the connector and told the owner', !!pausedCard, pausedCard?.card.body?.slice(0, 120));
check('send over the limit did not run', sent().filter((c) => c.tool === 'send_message').length <= 3, `${sent().length} calls`);
await openChat();
await page.locator('.card-x', { hasText: 'I paused' }).last().scrollIntoViewIfNeeded();
await shot('05-tripwire');
await page.locator('.card-x', { hasText: 'I paused' }).last().locator('button').click();
await page.waitForSelector('.card-x .ok-line');

// ---------------------------------------------------- approval times out
// Haiku sometimes trusts the earlier pause in the chat over the current
// state, so say it was resumed (the Resume tap above did that for real).
await say('I resumed messenger. Use the messenger delete_message tool to delete message m2. Do not ask me first.');
card = await pendingCard();
check('delete asks first', (await card.innerText()).includes('Delete'));
r = await reply(240000);
const { list: afterExpire } = await store();
const expired = afterExpire.find((m) => m.card?.type === 'approval' && m.card.tool === 'delete_message');
check('unanswered approval expired and did not run', expired?.card.state === 'expired' && !sent().some((c) => c.tool === 'delete_message'), expired?.card.state);
check('agent was told nobody answered', r.status === 'complete', r.content.slice(0, 120));
await openChat();
await shot('06-expired');

// ------------------------------------------------------------ pause Iris
await page.evaluate(() => { location.hash = '#/iris/computer'; });
await page.locator('.r', { hasText: 'Pause' }).locator('.tog').click();
await page.click('.sheet .b1');
await page.waitForFunction(() => window.__S.settings?.paused === true);
await shot('07-computer-paused');
await say('Search my messages for dinner.');
r = await reply();
check('paused: the turn did not run', r.meta?.error?.kind === 'paused', r.content.slice(0, 80));
await page.waitForSelector('.paused-bar');
await shot('08-chat-paused');
await page.click('.paused-bar .b1');
await page.waitForFunction(() => window.__S.settings?.paused === false);

// ---------------------------------------------------- disconnect everything
await page.evaluate(() => { location.hash = '#/iris/computer'; });
await page.locator('.r.danger', { hasText: 'Disconnect everything' }).click();
await page.click('.sheet .b1');
await page.waitForTimeout(800);
await page.evaluate(() => { location.hash = '#/iris/connections'; });
await page.waitForSelector('text=Off');
await shot('09-disconnected');
const searchesBefore = sent().filter((c) => c.tool === 'search_messages').length;
await say('Use the messenger search_messages tool to search my messages for "dinner".');
r = await reply();
check('disconnected: the connector was not used', sent().filter((c) => c.tool === 'search_messages').length === searchesBefore, r.content.slice(0, 120));

console.log(JSON.stringify({ errors, failed: results.filter((x) => !x.ok).map((x) => x.name) }));
await browser.close();
process.exit(results.every((x) => x.ok) && !errors.length ? 0 : 1);
