// login-text.js: saving a login an agent asks for, and its sites.
import test from 'node:test';
import assert from 'node:assert/strict';
import { siteHost, bareDomain, saveLoginPath, loginDraft, parseSites, cardHosts } from '../../web/js/login-text.js';

test('hosts come from typed or filled-in websites', () => {
  assert.equal(siteHost('Shop.Example.test'), 'shop.example.test');
  assert.equal(siteHost('https://auth.example.test/sign_in?x=1'), 'auth.example.test');
  assert.equal(siteHost(''), '');
  assert.equal(siteHost('http://[bad'), '');
});

test('a bare domain the owner types means the whole site; a full address only its host', () => {
  for (const s of ['example.test', 'www.example.test', ' Example.test ']) {
    assert.equal(bareDomain(s), true, s);
  }
  for (const s of ['https://example.test', 'https://example.test/login', 'example.test/login', 'example.test:8443',
    'login.example.test', 'example.co.test.uk', '192.0.2.1', 'localhost', '']) {
    assert.equal(bareDomain(s), false, s);
  }
});

test('a card names every site a login would be used on', () => {
  assert.deepEqual(cardHosts({ site: 'https://order.shop.test', also_sites: ['https://auth.test', 'https://order.shop.test:443'] }),
    ['order.shop.test', 'auth.test']);
  assert.deepEqual(cardHosts({ site: 'https://shop.test' }), ['shop.test']);
});

test("an agent's card opens the add form with what Iris knows, and returns to the chat", () => {
  const card = { site: 'https://order.shop.test', host: 'order.shop.test', also_sites: ['https://auth.test'], username: 'sam' };
  const path = saveLoginPath(card, { id: 'm1', conversation_id: 'c1' });
  assert.ok(path.startsWith('iris/logins/new?'), path);
  const q = new URLSearchParams(path.split('?')[1]);
  assert.equal(q.get('return'), 'chat/c1');
  const draft = loginDraft(q);
  assert.deepEqual(draft, {
    card: 'm1',
    label: 'order.shop.test',
    site: 'https://order.shop.test',
    also_sites: ['https://auth.test'],
    username: 'sam',
    allow_subdomains: false,
  });
  // A name Iris suggested wins over the host; nothing else is made up.
  const named = loginDraft(new URLSearchParams(saveLoginPath({ site: 'https://example.test', host: 'example.test', label: 'Example' }, { id: 'm2', conversation_id: 'c2' }).split('?')[1]));
  assert.equal(named.label, 'Example');
  assert.equal(named.username, '');
  assert.deepEqual(named.also_sites, []);
  // The browser's full address stays scoped to its host.
  assert.equal(named.allow_subdomains, false);
  // Opened from Logins & wallet: an empty form.
  assert.deepEqual(loginDraft(new URLSearchParams('')), {
    card: null, label: '', site: '', also_sites: [], username: '', allow_subdomains: false,
  });
});

test('extra sites are typed one per line', () => {
  assert.deepEqual(parseSites('order.shop.test\n  https://auth.test, Order.Shop.test\n\n'), ['order.shop.test', 'https://auth.test']);
  assert.deepEqual(parseSites(''), []);
});
