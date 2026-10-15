// schedule-text.js: plain wording for the check-in schedule in Settings.
import test from 'node:test';
import assert from 'node:assert/strict';
import { fmt12, everyLabel, everyChoices, betweenLabel, EVERY_PRESETS } from '../../web/js/schedule-text.js';

test('how often reads in plain words', () => {
  assert.deepEqual(EVERY_PRESETS.map(everyLabel),
    ['Every 30 minutes', 'Every hour', 'Every 2 hours', 'Every 4 hours', 'Once a day']);
  assert.equal(everyLabel(15), 'Every 15 minutes');
  assert.equal(everyLabel(90), 'Every 90 minutes');
  assert.equal(everyLabel(180), 'Every 3 hours');
});

test('a saved value that is not a preset is still offered, in order', () => {
  assert.deepEqual(everyChoices(60), EVERY_PRESETS);
  assert.deepEqual(everyChoices(45), [30, 45, 60, 120, 240, 1440]);
  assert.deepEqual(everyChoices(undefined), EVERY_PRESETS);
});

test('the window reads like a clock', () => {
  assert.equal(fmt12('08:00'), '8 AM');
  assert.equal(fmt12('00:15'), '12:15 AM');
  assert.equal(fmt12('12:00'), '12 PM');
  assert.equal(betweenLabel('08:00', '22:00'), '8 AM–10 PM');
  assert.equal(betweenLabel('22:00', '06:30'), '10 PM–6:30 AM');
  assert.equal(betweenLabel('09:00', '09:00'), 'All day');
  assert.equal(betweenLabel(null, '09:00'), '');
});
