// chat-time.js: time headers, the owner's receipt, and the new-message count.
// A fixed zone with daylight saving (node applies TZ changes at runtime).
process.env.TZ = 'America/New_York';

import test from 'node:test';
import assert from 'node:assert/strict';
import { dayNumber, dayLabel, timeLabel, timeHeaders, receiptFor, newSince, badge, HEADER_GAP } from '../../web/js/chat-time.js';

const at = (y, mo, d, h = 12, mi = 0) => new Date(y, mo - 1, d, h, mi).getTime();
const MIN = 60 * 1000;
const NOW = at(2026, 10, 14, 15, 0); // Wednesday, Oct 14, 2026

test('calendar days count across daylight-saving changes and midnight', () => {
  // Spring forward (Mar 8) and fall back (Nov 1) in New York.
  for (const [mo, from, to] of [[3, 1, 15], [10, 25, 31], [11, 1, 7]]) {
    for (let d = from; d < to; d++) {
      assert.equal(dayNumber(at(2026, mo, d + 1, 0, 0)) - dayNumber(at(2026, mo, d, 23, 59)), 1);
      assert.equal(dayNumber(at(2026, mo, d, 0, 0)), dayNumber(at(2026, mo, d, 23, 59)));
    }
  }
});

test('day labels: Today, Yesterday, weekday this week, then dates', () => {
  assert.equal(dayLabel(at(2026, 10, 14, 0, 1), NOW, 'en-US'), 'Today');
  assert.equal(dayLabel(at(2026, 10, 13, 23, 59), NOW, 'en-US'), 'Yesterday');
  assert.equal(dayLabel(at(2026, 10, 12), NOW, 'en-US'), 'Monday');
  assert.equal(dayLabel(at(2026, 10, 8), NOW, 'en-US'), 'Thursday');
  assert.equal(dayLabel(at(2026, 10, 7), NOW, 'en-US'), 'Wed, Oct 7');
  assert.equal(dayLabel(at(2025, 12, 31), NOW, 'en-US'), 'Wed, Dec 31, 2025');
  // Just after midnight, the previous evening is already yesterday.
  assert.equal(dayLabel(at(2026, 10, 13, 23, 30), at(2026, 10, 14, 0, 5), 'en-US'), 'Yesterday');
  // A clock running ahead of the device never reads as "Today".
  assert.equal(dayLabel(at(2026, 10, 15), NOW, 'en-US'), 'Thu, Oct 15');
});

test('times follow the locale', () => {
  assert.equal(timeLabel(at(2026, 10, 14, 15, 42), 'en-US').replace(/\s/g, ' '), '3:42 PM');
  assert.equal(timeLabel(at(2026, 10, 14, 15, 42), 'en-GB'), '15:42');
});

test('a time header starts the chat and follows each gap of over an hour', () => {
  const t0 = at(2026, 10, 14, 13, 30);
  const heads = timeHeaders([t0, t0 + MIN, t0 + MIN + HEADER_GAP, t0 + 2 * MIN + 2 * HEADER_GAP + 1], NOW, 'en-US');
  assert.deepEqual(heads.map((h) => h && h.day), ['Today', null, null, 'Today']);
  assert.equal(heads[0].time.replace(/\s/g, ' '), '1:30 PM');
  assert.deepEqual(timeHeaders([], NOW), []);
});

test('a new day always gets a header', () => {
  const heads = timeHeaders([at(2026, 10, 13, 23, 50), at(2026, 10, 14, 0, 5)], NOW, 'en-US');
  assert.deepEqual(heads.map((h) => h.day), ['Yesterday', 'Today']);
});

const me = 'u1';
const user = (id, extra = {}) => ({ id, role: 'user', kind: 'text', author_user_id: me, ...extra });
const reply = (id, status, meta = {}) => ({ id, role: 'assistant', kind: 'text', status, meta });

test('receipt: Delivered until the CLI starts, then Read with the time', () => {
  const list = [user('m1'), reply('r1', 'streaming')];
  assert.deepEqual(receiptFor(list, me), { id: 'm1', state: 'delivered', at: null });
  assert.deepEqual(receiptFor(list, me, { r1: { readAt: 5 } }), { id: 'm1', state: 'read', at: 5 });
  const stored = [user('m1'), reply('r1', 'streaming', { read_at: 7 })];
  assert.deepEqual(receiptFor(stored, me), { id: 'm1', state: 'read', at: 7 });
});

test('receipt: only the latest owner message, gone once answered', () => {
  assert.equal(receiptFor([user('m1'), reply('r1', 'complete')], me), null);
  assert.equal(receiptFor([user('m1'), reply('r1', 'error')], me), null);
  const two = [user('m1'), reply('r1', 'complete'), user('m2'), reply('r2', 'streaming')];
  assert.equal(receiptFor(two, me).id, 'm2');
  // A card the agent shows mid-turn doesn't count as the answer.
  const card = [user('m1'), { id: 'c', role: 'assistant', kind: 'card', status: 'complete' }, reply('r1', 'streaming')];
  assert.equal(receiptFor(card, me).state, 'delivered');
  // Someone else's message, or none from the owner: no receipt.
  assert.equal(receiptFor([user('m1', { author_user_id: 'u2' })], me), null);
  assert.equal(receiptFor([], me), null);
  // Sent, reply not created yet: Delivered.
  assert.equal(receiptFor([user('m1')], me).state, 'delivered');
});

test('new messages since the last one in view, not counting my own', () => {
  const list = [
    { id: 'a', role: 'user', author_user_id: 'me' },
    { id: 'b', role: 'assistant' },
    { id: 'c', role: 'user', author_user_id: 'me' },
    { id: 'd', role: 'assistant' },
    { id: 'e', role: 'user', author_user_id: 'sam' },
    { id: 'f', role: 'system', kind: 'system_note' },
  ];
  assert.equal(newSince(list, 'f', 'me'), 0);
  assert.equal(newSince(list, 'b', 'me'), 3);
  assert.equal(newSince(list, 'a', 'me'), 4);
  assert.equal(newSince(list, 'gone', 'me'), 0);
  assert.equal(newSince(list, null, 'me'), 0);
  assert.equal(badge(3), '3');
  assert.equal(badge(10), '9+');
});
