// chat-time.js: day separators, message times, and the new-message count.
// A fixed zone with daylight saving (node applies TZ changes at runtime).
process.env.TZ = 'America/New_York';

import test from 'node:test';
import assert from 'node:assert/strict';
import { dayNumber, dayLabel, timeLabel, timeline, newSince, badge, RUN_GAP } from '../../web/js/chat-time.js';

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

test('a time ends each run of messages from one sender', () => {
  const t0 = at(2026, 10, 14, 9, 0);
  const marks = timeline([
    { at: t0, who: 'u:me' },
    { at: t0 + MIN, who: 'u:me' },
    { at: t0 + 2 * MIN, who: 'a' },
    { at: t0 + 3 * MIN, who: null }, // a system note: no time, keeps the run
    { at: t0 + 4 * MIN, who: 'a' },
    { at: t0 + 5 * MIN, who: 'u:sam' },
    { at: t0 + 6 * MIN, who: 'u:me' },
  ], NOW, 'en-US');
  assert.deepEqual(marks.map((m) => m.time), [false, true, false, false, true, true, true]);
  assert.deepEqual(marks.map((m) => m.day), ['Today', null, null, null, null, null, null]);
});

test('a long pause or a new day splits a run', () => {
  const t0 = at(2026, 10, 13, 23, 40);
  const marks = timeline([
    { at: t0, who: 'u:me' },
    { at: t0 + RUN_GAP, who: 'u:me' }, // exactly the gap: same run
    { at: t0 + 2 * RUN_GAP + 1, who: 'u:me' }, // 00:00 the next day, and past the gap
    { at: t0 + 2 * RUN_GAP + 2, who: 'u:me' },
  ], NOW, 'en-US');
  assert.deepEqual(marks, [
    { day: 'Yesterday', time: false },
    { day: null, time: true },
    { day: 'Today', time: false },
    { day: null, time: true },
  ]);
  // Same sender across midnight, a minute apart: each day ends its run.
  const night = timeline([
    { at: at(2026, 10, 13, 23, 59), who: 'a' },
    { at: at(2026, 10, 14, 0, 0), who: 'a' },
  ], NOW, 'en-US');
  assert.deepEqual(night, [{ day: 'Yesterday', time: true }, { day: 'Today', time: true }]);
});

test('lines with no time of their own still get a day separator', () => {
  const marks = timeline([{ at: at(2026, 10, 12), who: null }, { at: at(2026, 10, 14), who: 'a' }], NOW, 'en-US');
  assert.deepEqual(marks, [{ day: 'Monday', time: false }, { day: 'Today', time: true }]);
  assert.deepEqual(timeline([], NOW), []);
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
