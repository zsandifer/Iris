// md.js: safe output, linear-time link parsing, bounded quote nesting.
import test from 'node:test';
import assert from 'node:assert/strict';
import { md, plain } from '../../web/js/md.js';

const timed = (f) => { const t = performance.now(); f(); return performance.now() - t; };

test('links render for http(s) only, escaped', () => {
  assert.equal(md('See [the docs](https://example.com/docs).'),
    '<p>See <a href="https://example.com/docs" target="_blank" rel="noopener noreferrer">the docs</a>.</p>');
  assert.ok(!md('[x](javascript:alert(1))').includes('<a'));
  assert.ok(!md('<img src=x onerror=alert(1)>').includes('<img'));
  assert.equal(plain('Read [this](https://example.com) now'), 'Read this now');
});

test('many unclosed brackets stay fast (no quadratic backtracking)', { timeout: 10000 }, () => {
  // Before the fix 40,000 brackets took seconds (quadratic); now about 1 ms.
  for (const s of ['['.repeat(60000), '[a](http://x'.repeat(6000), '![!['.repeat(15000), '[a]('.repeat(15000)]) {
    assert.ok(timed(() => md(s)) < 1000, `md ${s.slice(0, 12)}`);
    assert.ok(timed(() => plain(s)) < 1000, `plain ${s.slice(0, 12)}`);
  }
});

test('quotes nest up to 8 levels; deeper markers are text, and huge depth does not overflow', () => {
  assert.equal(md('> a\n>> b'), '<blockquote><p>a</p><blockquote><p>b</p></blockquote></blockquote>');
  const nine = md('>'.repeat(9) + ' x');
  assert.equal(nine.split('<blockquote>').length - 1, 8);
  assert.ok(nine.includes('<p>&gt; x</p>'));
  let out;
  assert.ok(timed(() => { out = md('>'.repeat(10000) + 'x'); }) < 1000);
  assert.equal(out.split('<blockquote>').length - 1, 8);
});
