// Run the actual isolated-world helpers with a small synthetic DOM. No
// Chromium or third-party DOM package is needed for the masking regressions.
import test from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { runInNewContext } from 'node:vm';
import { webcrypto } from 'node:crypto';

const source = readFileSync(new URL('../../src/browser/page.js', import.meta.url), 'utf8');
const refusal = 'This page has your password filled in. Submit or leave the page first.';
const password = 'S3cret-Pa55-xyz!';

function page(parts, withInput = false) {
  const text = (value) => ({ nodeType: 3, nodeValue: value, childNodes: [] });
  let document;
  const element = (tag, children = []) => ({
    nodeType: 1, tagName: tag.toUpperCase(), childNodes: children,
    get innerText() { return this.childNodes.map((n) => n.nodeValue ?? n.innerText).join(tag === 'body' ? '\n' : ''); },
    getAttribute() { return null; }, checkVisibility() { return true; },
    get ownerDocument() { return document; },
  });
  class Input {
    get value() { return this.saved || ''; }
    set value(v) { this.saved = v; }
  }
  const input = Object.assign(new Input(), element('input'), {
    isConnected: true, type: 'password',
    getAttribute(key) { return key === 'type' ? 'password' : key === 'aria-label' ? 'Password' : null; },
    style: { setProperty() {} }, focus() {},
    dispatchEvent(event) {
      if (event.type === 'input') {
        // A transformation unknown to the redactor, reflected synchronously.
        const transformed = [...this.value].reverse().join('');
        document.title = transformed;
        document.body.childNodes.push(element('p', [text(transformed)]));
        document.location.href = `https://shop.test/${transformed}`;
      }
    },
  });
  Object.defineProperty(input, 'ownerDocument', { get() { return document; } });
  const body = element('body', parts.map((p) => element('p', [text(p)])));
  if (withInput) body.childNodes.push(input);
  document = {
    body, documentElement: body, title: 'Shop',
    location: { href: 'https://shop.test/login', origin: 'https://shop.test' },
    defaultView: { getComputedStyle() { return { display: 'block' }; } },
    querySelectorAll() { return []; },
    createTreeWalker(root) {
      const nodes = [];
      const walk = (n) => { if (n.nodeType === 3) nodes.push(n); else n.childNodes.forEach(walk); };
      walk(root);
      return { nextNode() { return nodes.shift() || null; } };
    },
  };
  class Event { constructor(type) { this.type = type; } }
  const context = { document, location: document.location, crypto: webcrypto,
    NodeFilter: { SHOW_TEXT: 4 }, HTMLInputElement: Input, Event, InputEvent: Event };
  runInNewContext(source, context);
  context.__iris.setSecrets([password]);
  return { iris: context.__iris, document, input };
}

test('emoji before a password split across paragraphs cannot shift its node mask', () => {
  const { iris } = page(['😀'.repeat(20), 'S3cret-', 'Pa55-xyz!']);
  const snapshot = iris.snapshot(12000).text;
  assert.ok(snapshot.includes('😀'));
  assert.ok(snapshot.includes('••••'));
  assert.ok(!snapshot.includes('S3cret-') && !snapshot.includes('Pa55-xyz!'), snapshot);
});

test('wait matches only the snapshot mask of split secrets', () => {
  const { iris } = page(['Welcome', 'S3cret-', 'Pa55-xyz!']);
  assert.equal(iris.hasText('Welcome'), true);
  assert.equal(iris.hasText('S3cret-'), false);
  assert.equal(iris.hasText('Pa55-xyz!'), false);
});

test('wait can find visible text past the normal snapshot limit', () => {
  const { iris } = page([...Array(100).fill('x'.repeat(400)), 'The final paragraph']);
  assert.equal(iris.snapshot(12000).truncated, true);
  assert.equal(iris.hasText('The final paragraph'), true);
});

test('vault fill blocks reads and transformed metadata until a new document', () => {
  const { iris, input } = page(['Sign in'], true);
  const ref = iris.snapshot(12000).text.match(/\[ref=(\w+)\]/)[1];
  assert.equal(iris.fillField(ref, password, true, iris.info().doc).ok, true);
  assert.equal(input.value, password);
  for (const read of [() => iris.snapshot(12000), () => iris.hasText('xyz'), () => iris.pageText()]) {
    assert.throws(read, (error) => error.message === refusal);
  }
  assert.equal(iris.info().url, 'https://shop.test');
  assert.equal(iris.info().title, '');
  assert.equal(iris.info().tainted, true);
  // A URL change in the existing document never clears its taint.
  assert.throws(() => iris.snapshot(12000), (error) => error.message === refusal);
  const next = page(['Welcome back']).iris;
  assert.equal(next.info().tainted, false);
  assert.ok(next.snapshot(12000).text.includes('Welcome back'));
});

test('a username-only vault fill also blocks reads of the current document', () => {
  const { iris } = page(['Sign in'], true);
  const ref = iris.snapshot(12000).text.match(/\[ref=(\w+)\]/)[1];
  assert.equal(iris.fillField(ref, 'pat@example.com', false, iris.info().doc).ok, true);
  assert.throws(() => iris.snapshot(12000), (error) => error.message === refusal);
});
