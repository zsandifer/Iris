// web/js/catalog.js: catalog groups, row buttons and targets, permission choices.
import test from 'node:test';
import assert from 'node:assert/strict';
import {
  SERVICES, GOOGLE_PARTS, CATALOG_IDS, brandOf, topRows, rowAction, rowStatus, rowTarget, ruleFor,
} from '../../web/js/catalog.js';

const id = '0123456789abcdef0123456789abcdef';

test('a group shows its top five and offers the rest behind See all', () => {
  const rows = Array.from({ length: 7 }, (_, i) => ({ label: `S${i}` }));
  assert.deepEqual(topRows({ rows }).rows.map((r) => r.label), ['S0', 'S1', 'S2', 'S3', 'S4']);
  assert.equal(topRows({ rows }).more, true);
  assert.equal(topRows({ rows: rows.slice(0, 5) }).more, false);
  assert.deepEqual(topRows({}).rows, []);
});

test('rows say View once connected and open the right screen', () => {
  const gmail = { service: 'google', part: 'gmail', connector_id: id, state: 'connected' };
  assert.equal(rowAction(gmail), 'View');
  assert.equal(rowTarget(gmail), `iris/connections/${id}/gmail`);
  assert.equal(rowStatus(gmail), null);
  // A Google service turned off is still a View (its switch is there).
  assert.equal(rowAction({ ...gmail, state: 'off' }), 'View');
  assert.equal(rowStatus({ ...gmail, state: 'off' }), 'Off');
  // Not set up: Connect opens the setup.
  const notion = { service: 'notion', part: null, connector_id: null, state: 'not_connected' };
  assert.equal(rowAction(notion), 'Connect');
  assert.equal(rowTarget(notion), 'iris/connections/add/notion');
  assert.equal(rowStatus(notion), null);
  // Set up but signed out: Connect opens its page, which signs in.
  const signedOut = { ...notion, connector_id: id, state: 'needs_sign_in' };
  assert.equal(rowAction(signedOut), 'Connect');
  assert.equal(rowTarget(signedOut), `iris/connections/${id}`);
  assert.equal(rowStatus(signedOut), 'Needs sign-in');
  assert.equal(rowTarget({ service: 'google', part: 'drive', connector_id: null }), 'iris/connections/add/google');
});

test('a choice is always saved as an explicit rule; off is stored as block', () => {
  // Ask over an "ask unless trusted" default must still ask for trusted people.
  assert.equal(ruleFor('ask'), 'ask');
  assert.equal(ruleFor('allow'), 'allow');
  assert.equal(ruleFor('off'), 'block');
});

test('every catalog service and Google part has a local tile', () => {
  for (const s of CATALOG_IDS.filter((x) => x !== 'custom')) {
    const b = brandOf({ service: s });
    assert.ok(b && b.mark && /^#[0-9a-f]{6}$/i.test(b.bg), s);
  }
  for (const p of Object.keys(GOOGLE_PARTS)) assert.equal(brandOf({ service: 'google', part: p }), GOOGLE_PARTS[p]);
  assert.equal(brandOf({ service: 'custom' }), null);
  assert.equal(brandOf({ id: 'stripe' }), SERVICES.stripe);
  // Nothing is fetched from another site for a tile.
  assert.ok(!JSON.stringify({ SERVICES, GOOGLE_PARTS }).includes('http'));
});
