// Every browser module must at least parse; most are not imported by the
// other tests because they render UI. Checked as .mjs copies: `node --check`
// on a plain .js file outside a "type": "module" package can pass a file
// that has a syntax error.
import test from 'node:test';
import assert from 'node:assert/strict';
import { execFileSync } from 'node:child_process';
import { readdirSync, readFileSync, writeFileSync, mkdtempSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { fileURLToPath } from 'node:url';

const web = fileURLToPath(new URL('../../web/', import.meta.url));
const files = ['sw.js', ...readdirSync(join(web, 'js')).filter((f) => f.endsWith('.js')).map((f) => 'js/' + f)];
const dir = mkdtempSync(join(tmpdir(), 'iris-web-syntax-'));
test.after(() => rmSync(dir, { recursive: true, force: true }));

for (const f of files) {
  test(`web/${f} parses`, () => {
    // sw.js is a classic script, the rest are modules.
    const copy = join(dir, f.replace('/', '_') + (f === 'sw.js' ? '.cjs' : '.mjs'));
    writeFileSync(copy, readFileSync(join(web, f)));
    assert.doesNotThrow(() => execFileSync(process.execPath, ['--check', copy], { stdio: 'pipe' }));
  });
}
