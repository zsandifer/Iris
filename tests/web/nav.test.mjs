// store.js: Back climbs one layer up the route, like a file browser.
import './env.mjs';
import test from 'node:test';
import assert from 'node:assert/strict';
import { parentRoute } from '../../web/js/store.js';

test('Back always goes one layer up and climbs out of settings', () => {
  const up = (h) => parentRoute('#/' + h);
  assert.equal(up('settings/ai'), 'settings');
  assert.equal(up('settings'), 'iris');
  assert.equal(up('iris'), 'chat');
  assert.equal(up('iris/connections'), 'iris');
  assert.equal(up('iris/connections/add/google'), 'iris/connections');
  assert.equal(up('iris/connections/trusted'), 'iris/connections');
  assert.equal(up('iris/connections/group/x'), 'iris/connections');
  const id = 'a'.repeat(32);
  assert.equal(up(`iris/connections/${id}/edit`), `iris/connections/${id}`);
  assert.equal(up(`iris/connections/${id}`), 'iris/connections');
  assert.equal(up('iris/computer?view=watch'), 'iris/computer');
  assert.equal(up(`iris/computer?view=control&return=chat%2F${id}`), `chat/${id}`);
  assert.equal(up('iris/computer?view=watch&return=settings'), 'iris/computer');
  assert.equal(up('iris/files/memory/notes/a.md?edit=1'), 'iris/files/memory/notes');
  assert.equal(up('iris/files/memory'), 'iris/files');
  assert.equal(up('iris/files/system/SOUL.md'), 'iris/files');
  assert.equal(up('goals'), 'chat');
  // Going up repeatedly from any depth always ends in chat, never loops.
  let h = 'settings/activity', n = 0;
  while (h !== 'chat' && n++ < 10) h = up(h);
  assert.equal(h, 'chat');
});
