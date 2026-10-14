// web/js/routines.js: plain-language run statuses, the hub line, loading.
import { onFetch, reply } from './env.mjs';
import test from 'node:test';
import assert from 'node:assert/strict';

const { runStatus, routinesSub, onActivate, mergeDraft, draftChanges } = await import('../../web/js/routines.js');
const { S, set, loadRoutines } = await import('../../web/js/store.js');

test('every run status reads as words, not just a colour', () => {
  assert.deepEqual(runStatus({ status: 'ok' }), ['ok', 'Done']);
  assert.deepEqual(runStatus({ status: 'skipped' }), ['mu', 'Skipped']);
  assert.deepEqual(runStatus({ status: 'waiting' }), ['warn', 'Waiting for your OK']);
  assert.deepEqual(runStatus({ status: 'running' }), ['ac', 'Running now']);
  assert.deepEqual(runStatus({ status: 'failed', retry_at: 1 }), ['warn', 'Trying again soon']);
  assert.deepEqual(runStatus({ status: 'failed', retry_at: null }), ['bad', "Couldn't finish"]);
});

test('Enter and Space open a focused row, not keys meant for a control inside it', () => {
  let opened = 0;
  const handler = onActivate(() => { opened += 1; });
  const row = {};
  const key = (k, target = row) => {
    let prevented = false;
    handler({ key: k, target, currentTarget: row, preventDefault: () => { prevented = true; } });
    return prevented;
  };
  assert.equal(key('Enter'), true);
  assert.equal(key(' '), true);
  assert.equal(opened, 2);
  assert.equal(key('Tab'), false);
  assert.equal(key('Enter', { toggle: true }), false);
  assert.equal(opened, 2);
});

test("an open editor takes newer saved settings but keeps the owner's own edits", () => {
  const base = { title: 'Dinner', ask_before_acting: false, max_runs: null, rule: { freq: 'weekly', days: [6] } };
  // The owner renamed it here; another device turned "ask first" on.
  const draft = { ...base, title: 'Saturday dinner' };
  const fresh = { ...base, ask_before_acting: true };
  const m = mergeDraft(draft, base, fresh);
  assert.equal(m.draft.title, 'Saturday dinner');
  assert.equal(m.draft.ask_before_acting, true);
  // Saving sends only the rename, never the stale "ask first: off".
  assert.deepEqual(draftChanges(m.draft, m.base), { title: 'Saturday dinner' });
  assert.deepEqual(draftChanges(m.base, m.base), {});
  // Nested values compare by content.
  assert.deepEqual(draftChanges({ ...base, rule: { freq: 'weekly', days: [6, 0] } }, base), { rule: { freq: 'weekly', days: [6, 0] } });
});

test('the hub line counts active routines and what waits for the owner', async () => {
  set({ me: { user: { id: 'o', is_owner: true } }, routines: null });
  assert.equal(routinesSub(), ' ');
  onFetch((url, opts) => {
    assert.equal(url, '/api/routines');
    return reply(opts, {
      body: {
        default_tz: 'America/Chicago',
        max_doc_bytes: 1048576,
        routines: [
          { id: 'a', state: 'active' },
          { id: 'b', state: 'active', open_run: { status: 'running' } },
          { id: 'c', state: 'paused' },
        ],
      },
    });
  });
  await loadRoutines();
  assert.equal(S.routines.length, 3);
  assert.equal(S.routinesInfo.max_doc_bytes, 1048576);
  assert.equal(routinesSub(), '2 active');
  set({ routines: [...S.routines, { id: 'd', state: 'pending_approval', pending_change: { id: 'x', action: 'create' } }] });
  assert.equal(routinesSub(), '1 waiting for you');
  set({ routines: [] });
  assert.equal(routinesSub(), 'Nothing scheduled yet');
});
