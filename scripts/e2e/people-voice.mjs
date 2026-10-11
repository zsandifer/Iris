// End-to-end check of People, Together chats, and voice, driven through the
// real app in headless Chromium against a real `iris serve` with the real,
// signed-in CLIs. Run it through scripts/e2e/people-voice.sh, which builds
// iris, starts it on a temporary data directory, and passes:
//
//   node people-voice.mjs <base url> <owner pairing code> <data dir> <out dir>
//
// It uses a few tiny model requests and downloads the voice models (about
// 330 MB) through the app's own installer. Screenshots go to <out dir>.
import { createRequire } from 'node:module';
import { execFileSync } from 'node:child_process';
import { createECDH, randomBytes } from 'node:crypto';
import { mkdirSync, readFileSync, writeFileSync, existsSync } from 'node:fs';

const require = createRequire((process.env.NODE_PATH || process.cwd()) + '/');
const { chromium } = require('playwright');

const [base, ownerCode, dataDir, out] = process.argv.slice(2);
mkdirSync(out, { recursive: true });
const executablePath = process.env.CHROME || undefined;
const phone = { viewport: { width: 390, height: 844 }, deviceScaleFactor: 2, isMobile: true, hasTouch: true };
const failures = [];
const step = (msg) => console.log(`\n== ${msg}`);
const ok = (msg) => console.log(`   ok: ${msg}`);
function check(cond, msg) {
  if (cond) ok(msg);
  else { failures.push(msg); console.log(`   FAIL: ${msg}`); }
}
const shot = (page, name) => page.screenshot({ path: `${out}/${name}.png` });
const tokenOf = (page) => page.evaluate(() => localStorage.getItem('iris.token'));
async function call(page, method, path, body) {
  return page.evaluate(async ([m, p, b]) => {
    const r = await fetch('/api' + p, {
      method: m,
      headers: { Authorization: 'Bearer ' + localStorage.getItem('iris.token'), ...(b ? { 'Content-Type': 'application/json' } : {}) },
      body: b ? JSON.stringify(b) : undefined,
    });
    return { status: r.status, body: await r.json().catch(() => null) };
  }, [method, path, body]);
}
// Record real audio playback (the app's <audio> is not in the DOM).
const watchAudio = () => {
  const orig = HTMLMediaElement.prototype.play;
  HTMLMediaElement.prototype.play = function play(...args) {
    window.__audio = this;
    const r = orig.apply(this, args);
    r.catch(() => { window.__playRejected = (window.__playRejected || 0) + 1; });
    return r;
  };
};
function watchErrors(page, who) {
  page.on('pageerror', (e) => failures.push(`${who} page error: ${e.message}`));
  page.on('console', (m) => { if (m.type() === 'error') console.log(`   [${who} console] ${m.text()}`); });
}
async function pair(ctx, code, who) {
  const page = await ctx.newPage();
  watchErrors(page, who);
  await page.goto(base + '/');
  await page.fill('.code-in', code);
  await page.click('button.b1');
  await page.waitForSelector('.composer');
  return page;
}
/** Wait until the newest assistant message in a conversation is finished. */
async function reply(page, cid, after, timeout = 240000) {
  const end = Date.now() + timeout;
  while (Date.now() < end) {
    const r = await call(page, 'GET', `/conversations/${cid}/messages?limit=20`);
    const m = (r.body?.messages || []).filter((x) => x.role === 'assistant' && x.kind === 'text' && x.created_at > after).at(-1);
    if (m && m.status !== 'streaming') return m;
    await page.waitForTimeout(1000);
  }
  throw new Error('no reply in time');
}
async function typeAndSend(page, text) {
  await page.fill('.composer textarea', text);
  await page.click('.composer button[aria-label="Send"]');
}

// ------------------------------------------------------------ phase one
const browser = await chromium.launch({ executablePath });
const ownerCtx = await browser.newContext({ ...phone, colorScheme: 'light' });
await ownerCtx.addInitScript(() => localStorage.setItem('iris.notify.dismissed', '1'));
step('Owner pairs and names themselves');
let owner = await pair(ownerCtx, ownerCode, 'owner');
await call(owner, 'PUT', '/me', { display_name: 'Alex' });
const ownerMe = (await call(owner, 'GET', '/me')).body;
check(ownerMe.user.is_owner, 'owner device paired');

step('Avatar upload from Settings');
await owner.goto(base + '/#/settings');
await owner.click('text=Picture');
await owner.waitForSelector('.sheet');
const [chooser] = await Promise.all([owner.waitForEvent('filechooser'), owner.click('text=Choose a photo')]);
await chooser.setFiles(process.env.AVATAR_IMAGE);
await owner.waitForFunction(() => document.querySelector('.r .rv')?.textContent === 'Custom' || [...document.querySelectorAll('.rv')].some((e) => e.textContent === 'Custom'));
const settings = (await call(owner, 'GET', '/settings')).body;
check(settings.avatar_custom === true, 'custom picture saved');
await owner.goto(base + `/#/chat/${ownerMe.main_conversation_id}`);
await owner.waitForSelector('.top .avatar img');
check(await owner.$('.top .avatar img') !== null, 'top bar shows the custom picture');
await owner.click('.who-btn');
await owner.waitForSelector('.hub-hero .avatar img');
check(true, 'Iris hub shows the custom picture');
await shot(owner, '01-owner-hub-avatar');

step('Voice replies before the server has a voice (device fallback)');
await owner.goto(base + '/#/settings');
await owner.click('button[aria-label="Voice replies"]');
await owner.waitForFunction(() => document.querySelector('button[aria-label="Voice replies"]')?.getAttribute('aria-checked') === 'true');
check((await call(owner, 'GET', '/settings')).body.voice.enabled, 'voice replies turned on');
await shot(owner, '02-owner-settings-voice-off-server');
await owner.goto(base + `/#/chat/${ownerMe.main_conversation_id}`);
let t0 = Date.now();
await typeAndSend(owner, 'Reply with exactly: Good morning to you.');
let m = await reply(owner, ownerMe.main_conversation_id, t0 - 5000);
check(m.status === 'complete', `owner's own chat answered (engine: ${m.harness})`);
await owner.waitForSelector('.vn');
check(await owner.isVisible('text=Read aloud by this device'), 'fallback voice note is labeled as this device\'s voice');
await owner.click('.vn-play');
await owner.waitForTimeout(400);
await shot(owner, '03-owner-device-voice-note');

step('Set up the server voice and voice input from Settings');
await owner.goto(base + '/#/settings');
await owner.waitForSelector('text=Set up');
for (const b of await owner.$$('button.cn:has-text("Set up")')) await b.click();
await owner.waitForTimeout(500);
await shot(owner, '04-owner-settings-voice-installing');
{
  const until = Date.now() + 20 * 60 * 1000;
  let v = {};
  while (Date.now() < until) {
    v = (await call(owner, 'GET', '/settings')).body.voice;
    if (v.tts && v.stt) break;
    await owner.waitForTimeout(3000);
  }
  check(v.tts && v.stt, 'voice (Piper) and voice input (Whisper) installed through the app');
}
await owner.reload();
await owner.waitForSelector('.ok:has-text("ready")');
await shot(owner, '05-owner-settings-voice-ready');
const ownerState = `${out}/owner-state.json`;
await ownerCtx.storageState({ path: ownerState });

step('Owner adds a person and makes a pairing code');
await owner.goto(base + '/#/settings/people');
await owner.click('text=Add a person');
await owner.fill('.sheet input.field', 'Sam');
await owner.click('.sheet button[type=submit]');
await owner.waitForSelector('.big-code');
const samCode = (await owner.textContent('.big-code')).trim();
await shot(owner, '06-owner-people-code');
check(/^[0-9A-Z]{4}-[0-9A-Z]{4}$/.test(samCode), `pairing code for Sam (${samCode.slice(0, 2)}..)`);
await browser.close();

// ------------------------------------------------------------ phase two
// A fake microphone that says a known sentence, made with the server's own
// voice (so no audio file is checked in).
step('Make a spoken question for the fake microphone');
const eng = `${dataDir}/engines`;
const fixture = `${out}/question.wav`;
execFileSync(`${eng}/sherpa-onnx/bin/sherpa-onnx-offline-tts`, [
  `--vits-model=${eng}/tts/model.onnx`, `--vits-tokens=${eng}/tts/tokens.txt`,
  `--vits-data-dir=${eng}/tts/espeak-ng-data`, `--output-filename=${fixture}`, '--',
  'What is the capital of France?',
], { stdio: 'ignore' });
check(existsSync(fixture), 'question recorded with the server voice');
{
  // Pad with silence so the recording clearly contains the whole sentence.
  const wav = readFileSync(fixture);
  const dataAt = wav.indexOf('data') + 8;
  const rate = wav.readUInt32LE(24);
  const pad = Buffer.alloc(Math.round(rate * 0.6) * 2);
  const body = Buffer.concat([pad, wav.subarray(dataAt), pad, pad]);
  const head = Buffer.from(wav.subarray(0, dataAt));
  head.writeUInt32LE(36 + body.length, 4);
  head.writeUInt32LE(body.length, dataAt - 4);
  writeFileSync(fixture, Buffer.concat([head, body]));
}

const browser2 = await chromium.launch({
  executablePath,
  args: ['--use-fake-ui-for-media-stream', '--use-fake-device-for-media-stream', `--use-file-for-fake-audio-capture=${fixture}%noloop`],
});
const ownerCtx2 = await browser2.newContext({ ...phone, colorScheme: 'light', storageState: ownerState, permissions: ['microphone'] });
await ownerCtx2.addInitScript(watchAudio);
owner = await ownerCtx2.newPage();
watchErrors(owner, 'owner');
await owner.goto(base + `/#/chat/${ownerMe.main_conversation_id}`);
await owner.waitForSelector('.composer');

step('Voice input: record, transcribe on the server, send');
await owner.click('.composer button[aria-label="Talk"]');
await owner.waitForSelector('.rec-line');
await owner.waitForTimeout(3600);
await shot(owner, '07-owner-recording');
await owner.click('.composer button[aria-label="Stop recording"]');
await owner.waitForFunction(() => /capital/i.test(document.querySelector('.composer textarea')?.value || ''), null, { timeout: 60000 });
const heard = await owner.inputValue('.composer textarea');
check(/capital of france/i.test(heard), `transcribed on the server: "${heard}"`);
await shot(owner, '08-owner-transcribed');
t0 = Date.now();
await owner.click('.composer button[aria-label="Send"]');
m = await reply(owner, ownerMe.main_conversation_id, t0 - 2000);
check(/paris/i.test(m.content), 'Iris answered the spoken question');

step('Voice reply plays by itself as a server voice note');
await owner.waitForSelector('.row:last-child .vn', { timeout: 60000 });
await owner.waitForSelector('.vn.on', { timeout: 60000 });
const heardAudio = await owner.waitForFunction(() => {
  const a = window.__audio;
  return a && a.src.startsWith('blob:') && a.currentTime > 0.2 ? a.currentTime : false;
}, null, { timeout: 60000 }).then((h) => h.jsonValue()).catch(() => 0);
check(heardAudio > 0.2 && !(await owner.evaluate(() => window.__playRejected || 0)), `the voice note really played (${Number(heardAudio).toFixed(2)} s in)`);
check(!(await owner.isVisible('.row:last-child .vn-lbl')), 'server voice (no device label)');
const audioOk = (await owner.evaluate(async (id) => {
  const r = await fetch('/api/voice/speak', { method: 'POST', headers: { Authorization: 'Bearer ' + localStorage.getItem('iris.token'), 'Content-Type': 'application/json' }, body: JSON.stringify({ message_id: id }) });
  return { type: r.headers.get('content-type'), ms: Number(r.headers.get('x-voice-duration-ms')) };
}, m.id));
check(audioOk.type === 'audio/wav' && audioOk.ms > 300, `voice note is ${audioOk.ms} ms of WAV audio`);
await shot(owner, '09-owner-voice-note-playing');

step('Sam pairs on their own phone (dark mode) and gets their own Iris');
const samCtx = await browser2.newContext({ ...phone, colorScheme: 'dark' });
await samCtx.addInitScript(() => localStorage.setItem('iris.notify.dismissed', '1'));
const sam = await pair(samCtx, samCode, 'sam');
const samMe = (await call(sam, 'GET', '/me')).body;
check(!samMe.user.is_owner && samMe.user.display_name === 'Sam', 'Sam is a separate person');
await call(sam, 'PUT', '/settings', { agent_name: 'Juno', voice_replies: true });
check((await call(owner, 'GET', '/settings')).body.agent_name === 'Iris', "renaming Sam's Iris leaves the owner's alone");
check((await call(sam, 'GET', '/settings')).body.agent_name === 'Juno', "Sam's Iris is called Juno");
await call(owner, 'PUT', '/files/system/MEMORY.md', { content: 'Alex keeps a secret code word: PERIWINKLE-42. Never share it.' });
await call(sam, 'PUT', '/files/system/MEMORY.md', { content: "Sam's favorite food is dumplings." });
check(!(await call(sam, 'GET', '/files/system/MEMORY.md')).body.content.includes('PERIWINKLE'), "Sam's system files are Sam's own");
await call(owner, 'POST', '/goals', { title: 'Owner goal' });
check((await call(sam, 'GET', '/goals')).body.goals.length === 0, "Sam does not see the owner's goals");
await sam.reload();
await sam.waitForSelector('.composer');
await shot(sam, '10-sam-own-iris-dark');

step("Sam's own chat: their memory, not the owner's");
t0 = Date.now();
await typeAndSend(sam, 'Two quick questions: what is my favorite food, and do you know any secret code word? Answer in one sentence.');
m = await reply(sam, samMe.main_conversation_id, t0 - 2000);
check(m.status === 'complete' && m.content.trim().length > 0, `Sam's Iris answered (engine: ${m.harness})`);
check(m.harness !== 'claude', "Sam's own Iris does not use the owner's Claude (individual-use policy)");
check(/dumpling/i.test(m.content), "Sam's Iris knows Sam's memory");
check(!/periwinkle/i.test(m.content), "Sam's Iris does not know the owner's memory");
await shot(sam, '11-sam-private-chat-dark');

step('Push subscriptions for both people (fake endpoints, to see the fan-out)');
const keys = () => {
  const ec = createECDH('prime256v1');
  ec.generateKeys();
  return { p256dh: ec.getPublicKey('base64url'), auth: randomBytes(16).toString('base64url') };
};
const subOwner = (await call(owner, 'POST', '/push/subscriptions', { endpoint: 'https://127.0.0.1:9/push/owner', keys: keys() })).body;
const subSam = (await call(sam, 'POST', '/push/subscriptions', { endpoint: 'https://127.0.0.1:9/push/sam', keys: keys() })).body;
check(subOwner?.subscription?.id && subSam?.subscription?.id, 'both devices subscribed');

step('Together chat: owner starts it from the drawer');
await owner.click('button[aria-label="Menu"]');
await owner.click('text=New together chat');
await owner.click('.sheet .r:has-text("Sam")');
await owner.waitForSelector('.top .st:has-text("Alex and Sam")');
await owner.waitForSelector('.drw.on', { state: 'detached' }).catch(() => {});
await owner.waitForTimeout(400);
const convs = (await call(owner, 'GET', '/conversations')).body.conversations;
const together = convs.find((c) => c.kind === 'group');
check(together && together.member_count === 2, 'Together chat with two people');
await shot(owner, '12-owner-together-empty');
t0 = Date.now();
await typeAndSend(owner, 'Hi both! Say hello to Alex and Sam by name in one short sentence.');
m = await reply(owner, together.id, t0 - 2000);
check(m.status === 'complete', `Iris replied in the Together chat (engine: ${m.harness})`);
check(m.harness !== 'claude', 'no Claude in a shared chat without the opt-in');
await owner.waitForSelector('.vn');
await shot(owner, '13-owner-together-reply');

step('Sam sees it, replies, and Iris answers both');
await sam.click('button[aria-label="Menu"]');
await sam.click(`.drw .it:has-text("Alex and Sam")`);
await sam.waitForSelector(`.umsg.other .who:has-text("Alex")`);
check(true, "Sam sees Alex's message, labeled with Alex's name");
check(await sam.isVisible('.top .nm:has-text("Iris")'), "Alex's Iris hosts the chat on Sam's phone too");
t0 = Date.now();
await typeAndSend(sam, "Do you know Alex's secret code word or my favorite food? Answer in one sentence.");
m = await reply(sam, together.id, t0 - 2000);
check(m.status === 'complete' && m.content.trim().length > 0, `Iris answered Sam's question in the Together chat (engine: ${m.harness})`);
check(!/periwinkle/i.test(m.content) && !/dumpling/i.test(m.content), 'no private memory from either person in the Together chat');
await owner.waitForSelector(`.umsg.other .who:has-text("Sam")`);
check(true, "Alex sees Sam's message");
await shot(sam, '14-sam-together-dark');
await shot(owner, '15-owner-together-both');

step('Push to both when the app is closed');
await owner.close();
await sam.close();
const ownerProbe = await ownerCtx2.newPage(); // API only, no WebSocket
await ownerProbe.goto(base + '/manifest.webmanifest');
const before = Date.now();
const sent = await ownerProbe.evaluate(async ([cid, token]) => {
  const r = await fetch(`/api/conversations/${cid}/messages`, { method: 'POST', headers: { Authorization: 'Bearer ' + token, 'Content-Type': 'application/json' }, body: JSON.stringify({ text: 'One more: reply with just the word "done".' }) });
  return r.status;
}, [together.id, await ownerProbe.evaluate(() => localStorage.getItem('iris.token'))]);
check(sent === 202, 'message sent with both apps closed');
// The reply push goes to both people; the message push goes to Sam.
await ownerProbe.waitForTimeout(1000);
const deadline = Date.now() + 240000;
let pushed = { owner: 0, sam: 0 };
while (Date.now() < deadline) {
  const log = readFileSync(process.env.SERVE_LOG, 'utf8');
  pushed = {
    owner: (log.match(new RegExp(`push to subscription ${subOwner.subscription.id} failed`, 'g')) || []).length,
    sam: (log.match(new RegExp(`push to subscription ${subSam.subscription.id} failed`, 'g')) || []).length,
  };
  if (pushed.owner >= 1 && pushed.sam >= 2) break;
  await ownerProbe.waitForTimeout(2000);
}
check(pushed.owner >= 1, `reply push sent to Alex (${pushed.owner} attempt(s); the fake endpoint refuses it)`);
check(pushed.sam >= 2, `message and reply pushes sent to Sam (${pushed.sam} attempts)`);
console.log(`   (finished ${Math.round((Date.now() - before) / 1000)}s after sending)`);

await browser2.close();
console.log(failures.length ? `\n${failures.length} check(s) failed:\n - ${failures.join('\n - ')}` : '\nAll checks passed.');
process.exit(failures.length ? 1 : 0);
