import test from 'node:test';
import assert from 'node:assert/strict';
import { googlePasteError } from '../../web/js/google-paste.js';

const id = '123-example.apps.googleusercontent.com', secret = 'GOCSPX-paste-canary_123';
test('immediate feedback accepts downloads and pairs in either order', () => {
  const pair = { client_id: id, client_secret: secret };
  for (const paste of [JSON.stringify({ web: pair }), JSON.stringify({ installed: pair }), `Client ID: ${id}\nClient secret: ${secret}`, `${id} ${secret}`, `Secret=${secret}\nID=${id}`, ` \r\n${secret}\t${id}\n `, `${id}\n${secret}\n${id}`]) assert.equal(googlePasteError(paste), null);
});
test('missing, malformed and ambiguous pastes give friendly errors without secrets', () => {
  for (const paste of ['', 'junk', '{}.', id, secret, JSON.stringify({ web: { client_secret: secret } }), `${id} ${secret} GOCSPX-another`, `bad.id.apps.googleusercontent.com ${secret}`, 'x'.repeat(65537)]) {
    const error = googlePasteError(paste);
    assert.ok(error);
    assert.ok(!error.includes(secret));
  }
});
