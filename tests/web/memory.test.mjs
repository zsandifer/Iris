// Memory: plain-language wording (memory-text.js) and the store pieces the
// Memory screen and the Noted line use (store.js).
import { onFetch, reply, calls } from './env.mjs';
import test from 'node:test';
import assert from 'node:assert/strict';
import { setToken } from '../../web/js/api.js';
import { S, set, handleEvent, loadMemory, toast, parentRoute } from '../../web/js/store.js';
import {
  whenWord, noteMark, groupNotes, countLabel, historyLines, historyText, changeTag, notedLine, sourceOf,
  undoTracker, undos, withoutMemory,
} from '../../web/js/memory-text.js';

setToken('t');
const CID = 'c'.repeat(32);
const at = (y, mo, d, h = 12) => new Date(y, mo - 1, d, h).getTime();
const NOW = at(2026, 10, 14, 8);
const note = (id, extra = {}) => ({ id, topic: 'you', text: `note ${id}`, pinned: false, created_at: at(2026, 6, 2), updated_at: at(2026, 6, 2), ...extra });

test('recent changes read like a person would say them', () => {
  assert.equal(whenWord(at(2026, 10, 14, 3), NOW), 'last night');
  assert.equal(whenWord(at(2026, 10, 13, 23), NOW), 'last night');
  assert.equal(whenWord(at(2026, 10, 14, 7), NOW), 'today');
  assert.equal(whenWord(at(2026, 10, 13, 15), NOW), 'yesterday');
  assert.equal(whenWord(at(2026, 10, 10), NOW), null);
  assert.deepEqual(noteMark(note(1, { created_at: at(2026, 10, 14, 3), updated_at: at(2026, 10, 14, 3) }), NOW), { text: 'Learned last night', recent: true });
  assert.deepEqual(noteMark(note(1, { updated_at: at(2026, 10, 13, 10) }), NOW), { text: 'Updated yesterday', recent: true });
  const old = noteMark(note(1), NOW);
  assert.equal(old.recent, false);
  assert.match(old.text, /^Since /);
  assert.doesNotMatch(old.text, /2026/, 'this year needs no year');
  assert.match(noteMark(note(1, { created_at: at(2025, 3, 1) }), NOW).text, /2025/);
});

test('notes are grouped pinned first, then by topic; a topic shows only its notes', () => {
  const items = [
    note(1, { topic: 'people', pinned: true }),
    note(2, { topic: 'routines' }),
    note(3, { topic: 'people' }),
    note(4, { topic: 'you' }),
    note(5, { topic: 'someday' }),
  ];
  assert.deepEqual(groupNotes(items).map((g) => [g.label, g.items.map((i) => i.id)]),
    [['Pinned', [1]], ['You', [4]], ['People', [3]], ['Routines', [2]], ['Other', [5]]]);
  assert.deepEqual(groupNotes(items, 'people').map((g) => [g.label, g.items.map((i) => i.id)]), [['Pinned', [1]], ['People', [3]]]);
  assert.deepEqual(groupNotes(items, 'projects'), []);
  assert.equal(countLabel(1), '1 thing');
  assert.equal(countLabel(142), '142 things');
  assert.equal(countLabel(2000, true), '2000+ things');
});

test('history is told in plain words; a correction is one line and only the newest change has Undo', () => {
  const history = [
    { id: 9, item_id: 3, op: 'supersede', actor: 'agent', batch_id: 'turn-x', at: 3, before: { text: 'Runs Mon/Wed evenings' }, after: { text: 'Runs Mon/Wed evenings', superseded_by: 7 } },
    { id: 8, item_id: 7, op: 'add', actor: 'agent', batch_id: 'turn-x', at: 3, before: null, after: { text: 'Runs Tue/Thu mornings' } },
    { id: 2, item_id: 3, op: 'add', actor: 'dream', batch_id: 'dream-1', at: 1, before: null, after: { text: 'Runs Mon/Wed evenings' } },
  ];
  const lines = historyLines(history);
  assert.deepEqual(lines.map((l) => [l.id, l.text, !!l.undo]), [
    [9, 'Corrected in a chat from “Runs Mon/Wed evenings”', true],
    [2, 'First learned overnight', false],
  ]);
  assert.equal(historyText({ op: 'update', actor: 'owner', before: { text: 'a', pinned: false }, after: { text: 'a', pinned: true } }), 'Pinned by you');
  assert.equal(historyText({ op: 'update', actor: 'import', before: { text: 'a' }, after: { text: 'b' } }), 'Updated from “a”');
  assert.equal(historyText({ op: 'add', actor: 'owner' }), 'You added it');
  assert.equal(historyText({ op: 'add', actor: 'import' }), 'Brought over from my older notes');
  assert.equal(historyText({ op: 'restore', actor: 'owner' }), 'You undid a change');
  assert.equal(historyText({ op: 'archive', actor: 'dream' }), 'Set aside as no longer current');
  assert.equal(historyLines([]).length, 0);
  // Nothing technical leaks into the words.
  for (const l of lines) assert.doesNotMatch(l.text, /dream|agent|supersede|#\d/i);
});

test('change tags, the Noted line and sources', () => {
  assert.deepEqual(changeTag('add'), ['New', 'new']);
  assert.deepEqual(changeTag('update'), ['Updated', 'chg']);
  assert.deepEqual(changeTag('supersede'), ['Done', 'old']);
  assert.deepEqual(changeTag('supersede', { noted: true }), ['Updated', 'chg']);
  assert.equal(changeTag(undefined), null, 'older morning notes have no tags');
  assert.equal(notedLine([{ op: 'add', text: 'Likes  tea' }]), 'Noted: Likes tea');
  assert.equal(notedLine([{ op: 'delete', text: 'Old job' }, { op: 'add', text: 'x' }]), 'Forgot: Old job +1 more');
  assert.equal(notedLine([]), '');
  assert.deepEqual(sourceOf({ source: { type: 'conversation', conversation_id: CID }, created_at: at(2026, 10, 12) }, NOW).cid, CID);
  assert.equal(sourceOf({ source: { type: 'dream', messages: [{ conversation_id: CID, message_id: 'm' }] } }, NOW).cid, CID);
  assert.equal(sourceOf({ source: { type: 'import', file: 'people.md' } }, NOW).text, 'From my older notes');
  assert.equal(sourceOf({ source: { type: 'owner' } }, NOW).cid, undefined);
});

test('Memory loads every page once, and a memory change reloads it', async () => {
  set({ me: { user: { id: 'o', is_owner: true } } });
  const pages = { 0: [note(1), note(2)], 2: [note(2), note(3)], 4: [] };
  onFetch((url, opts) => {
    const u = new URL(url, 'http://iris.test');
    assert.equal(u.pathname, '/api/memory');
    const off = Number(u.searchParams.get('offset'));
    return reply(opts, { body: { items: pages[off], has_more: off < 2 } });
  });
  await loadMemory();
  assert.deepEqual(S.memory.items.map((i) => i.id), [1, 2, 3], 'a note seen on two pages shows once');
  assert.equal(S.memory.more, false);
  pages[0] = [note(9), note(2)];
  const before = calls.length;
  handleEvent({ type: 'entities.changed', kinds: ['memory'] });
  await new Promise((r) => setTimeout(r, 10));
  assert.ok(calls.length > before);
  assert.equal(S.memory.items[0].id, 9);
});

test('Memory is never loaded for someone who is not the owner', async () => {
  set({ me: { user: { id: 'g', is_owner: false } }, memory: null });
  onFetch(() => { throw new Error('no request expected'); });
  await loadMemory();
  assert.equal(S.memory, null);
});

test('a memory.noted event adds the Noted line to its reply', () => {
  const m = { id: 'm1', conversation_id: CID, role: 'assistant', content: 'Sure', status: 'complete', created_at: 1, updated_at: 1, meta: { purpose: 'chat' } };
  set({ msgs: { [CID]: { list: [m] } } });
  const changes = [{ history_id: 4, item_id: 2, op: 'add', text: 'Likes tea' }];
  handleEvent({ type: 'memory.noted', conversation_id: CID, message_id: 'm1', changes });
  const now = S.msgs[CID].list[0];
  assert.deepEqual(now.meta.noted, changes);
  assert.equal(now.meta.purpose, 'chat', 'other details stay');
  handleEvent({ type: 'memory.noted', conversation_id: CID, message_id: 'unknown', changes });
  assert.equal(S.msgs[CID].list.length, 1);
});

test('a toast can carry an action such as Undo', () => {
  let ran = false;
  toast('Forgotten', { label: 'Undo', run: () => { ran = true; } });
  assert.equal(S.toast, 'Forgotten');
  assert.equal(S.toastAction.label, 'Undo');
  S.toastAction.run();
  assert.ok(ran);
  toast('Plain');
  assert.equal(S.toastAction, null);
});

test('Memory and its settings go back the expected way', () => {
  assert.equal(parentRoute('#/iris/memory'), 'iris');
  assert.equal(parentRoute('#/settings/memory'), 'settings');
});

test('Undo, Redo, Undo, Redo always targets the newest change', async () => {
  // The server only undoes a note's newest change; each undo makes a new one.
  let newest = 10;
  const sent = [];
  const undo = (route) => async (id) => {
    sent.push([route, id]);
    if (id !== newest) throw new Error('That note changed again.');
    newest += 1;
    return { history_id: newest };
  };
  const t = undoTracker();
  await t.toggle(10, undo('card'), undo('history'));
  assert.equal(t.isOff(10), true);
  await t.toggle(10, undo('card'), undo('history'));
  assert.equal(t.isOff(10), false);
  await t.toggle(10, undo('card'), undo('history'));
  assert.equal(t.isOff(10), true);
  await t.toggle(10, undo('card'), undo('history'));
  assert.equal(t.isOff(10), false);
  assert.deepEqual(sent, [['card', 10], ['history', 11], ['history', 12], ['history', 13]],
    'only the first undo uses the morning-note route');
  // A failed step leaves the state as it was.
  newest = 99;
  await assert.rejects(t.toggle(10, undo('card'), undo('history')));
  assert.equal(t.isOff(10), false);
  t.clear();
  assert.equal(t.isOff(10), false);
});

test('Forget everything (here or on another device) clears cached Noted lines and morning notes', async () => {
  const card = { id: 'k', conversation_id: CID, role: 'assistant', kind: 'card', content: 'I tidied my notes overnight', card: { type: 'memory_morning', changes: [] }, created_at: 1, updated_at: 1 };
  const noted = { id: 'n', conversation_id: CID, role: 'assistant', kind: 'text', content: 'Sure', meta: { purpose: 'chat', noted: [{ history_id: 1 }] }, created_at: 2, updated_at: 2 };
  const other = { id: 'o', conversation_id: CID, role: 'user', kind: 'text', content: 'hi', created_at: 3, updated_at: 3 };
  assert.deepEqual(withoutMemory(other), other);
  assert.equal(withoutMemory(card).card, null);
  assert.equal(withoutMemory(card).content, 'I cleared my notes.');
  assert.deepEqual(withoutMemory(noted).meta, { purpose: 'chat' });
  await undos.toggle(7, async () => ({ history_id: 8 }), async () => ({ history_id: 9 }));
  assert.equal(undos.isOff(7), true);
  const sheet = () => 'note';
  sheet.memory = true;
  set({ msgs: { [CID]: { list: [card, noted, other], hasMore: true } }, memory: null, sheet });
  handleEvent({ type: 'memory.reset', reset: '5' });
  const list = S.msgs[CID].list;
  assert.equal(list[0].card, null);
  assert.equal(list[1].meta.noted, undefined);
  assert.deepEqual(list[2], other);
  assert.equal(S.msgs[CID].hasMore, true, 'paging state stays');
  assert.equal(undos.isOff(7), false, 'undo state starts over');
  assert.equal(S.sheet, null, 'an open memory sheet closes');
  // Another sheet stays open through a reset.
  const other2 = () => 'quiet hours';
  set({ sheet: other2 });
  handleEvent({ type: 'memory.reset', reset: '6' });
  assert.equal(S.sheet, other2);
});

test('a reset missed while disconnected is caught up from the reconnect settings', () => {
  const noted = (id) => ({ id, conversation_id: CID, role: 'assistant', kind: 'text', content: 'Sure', meta: { noted: [{ history_id: 1 }] }, created_at: 1, updated_at: 1 });
  set({ msgs: { [CID]: { list: [noted('a')] } }, memory: null });
  const ready = (reset) => handleEvent({ type: 'ready', settings: { memory: { learning_paused: false, morning_note: true, reset } }, harnesses: [] });
  onFetch((url, opts) => reply(opts, { body: { conversations: [], messages: [], has_more: false, browser: {} } }));
  ready('6');
  assert.ok(S.msgs[CID].list[0].meta.noted, 'the same marker changes nothing');
  ready('7');
  assert.equal(S.msgs[CID].list[0].meta.noted, undefined, 'a new marker clears cached copies');
});

test('a note that changes while Memory first loads is not lost', async () => {
  set({ me: { user: { id: 'o', is_owner: true } }, memory: null });
  // Page 1 then page 2; a chat updates note 3 (on page 2) after page 1 was
  // read, moving it to the top, so the first pass misses it.
  let pass = 0;
  onFetch((url, opts) => {
    const off = Number(new URL(url, 'http://iris.test').searchParams.get('offset'));
    if (off === 0) {
      pass += 1;
      const first = pass === 1 ? [note(1), note(2)] : [note(3), note(1)];
      if (pass === 1) queueMicrotask(() => handleEvent({ type: 'entities.changed', kinds: ['memory'] }));
      return reply(opts, { body: { items: first, has_more: true } });
    }
    return reply(opts, { body: { items: pass === 1 ? [note(4)] : [note(2), note(4)], has_more: false } });
  });
  await loadMemory();
  assert.deepEqual(S.memory.items.map((i) => i.id).sort(), [1, 2, 3, 4]);
});
