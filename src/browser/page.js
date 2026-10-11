// Helpers Iris installs in an isolated JavaScript world of the page (the
// page's own scripts cannot see or change them). They read the page into a
// compact text snapshot with element refs and locate elements for real
// input events. Values of password fields and fields Iris filled from the
// vault are never read out.
if (!globalThis.__iris) {
  globalThis.__iris = (() => {
    const refs = new Map(); // ref -> element
    const ids = new WeakMap(); // element -> ref
    const secret = new WeakSet(); // fields filled from the vault
    let next = 1;
    // Refs and this id are unique per document, so a ref from an earlier
    // page can never point at an element of a later one.
    const letters = () => Array.from(crypto.getRandomValues(new Uint8Array(3)), (b) => 'abcdefghijklmnopqrstuvwxyz'[b % 26]).join('');
    const prefix = letters();
    const doc = letters() + letters() + letters();

    const refOf = (el) => {
      let r = ids.get(el);
      if (!r) {
        r = prefix + next++;
        ids.set(el, r);
      }
      refs.set(r, el);
      return r;
    };

    // Secrets Iris typed in this browser session (sent by Iris with each
    // call; this isolated world is invisible to the page). Everything read
    // out of the page passes through redact() first, before any
    // whitespace folding, cutting, or quoting could disguise a copy.
    let secrets = [];
    const setSecrets = (list) => {
      const all = new Set();
      for (const x of list || []) {
        all.add(x);
        all.add(x.replace(/\s+/g, ' ').trim());
        // As it appears in addresses and submitted forms.
        const enc = encodeURIComponent(x);
        all.add(enc);
        all.add(enc.replace(/%20/g, '+'));
      }
      secrets = [...all].filter((x) => x.length >= 3).sort((a, b) => b.length - a.length);
    };
    // Percent-decoding that never throws: runs of escapes are decoded
    // together (multi-byte UTF-8, any letter case); bad runs byte by byte.
    // Pages in legacy encodings escape bytes as Latin-1 (ä as %E4).
    const decodeLoose = (s) => s.replace(/\+/g, ' ').replace(/(%[0-9a-f]{2})+/gi, (run) => {
      try { return decodeURIComponent(run); } catch {
        return run.replace(/%([0-9a-f]{2})/gi, (m, h) => String.fromCharCode(parseInt(h, 16)));
      }
    });
    // Card numbers in any grouping ("3782 822463 10005", "4242-4242-...").
    const digitRes = () => secrets.filter((x) => /^\d{12,19}$/.test(x))
      .map((x) => new RegExp(x.split('').join('[\\s.\\u00a0-]*'), 'g'));
    const leaks = (t) => secrets.some((x) => t.includes(x)) || digitRes().some((r) => { r.lastIndex = 0; return r.test(t); });
    const redact = (s) => {
      let out = String(s ?? '');
      for (const x of secrets) if (out.includes(x)) out = out.split(x).join('••••');
      for (const r of digitRes()) out = out.replace(r, '••••');
      // Still there once percent-decoded (an address in another escaping):
      // drop the query, or the whole piece if the rest still shows it.
      if (secrets.length && leaks(decodeLoose(out))) {
        const q = out.indexOf('?');
        out = q >= 0 && !leaks(decodeLoose(out.slice(0, q))) ? out.slice(0, q) + '?••••' : '••••';
      }
      return out;
    };

    const clean = (s, n = 160) => {
      s = redact(s).replace(/\s+/g, ' ').trim();
      return s.length > n ? s.slice(0, n - 1) + '…' : s;
    };
    const q = (s) => JSON.stringify(s);

    const visible = (el) => {
      if (el.getAttribute && el.getAttribute('aria-hidden') === 'true') return false;
      if (el.checkVisibility) return el.checkVisibility({ visibilityProperty: true });
      const st = el.ownerDocument.defaultView.getComputedStyle(el);
      return st.display !== 'none' && st.visibility !== 'hidden';
    };

    const TEXTY = /^(text|email|tel|url|search|number|password|date|datetime-local|month|week|time|)$/;

    function role(el) {
      const r = el.getAttribute('role');
      if (r) return r.split(' ')[0];
      const t = el.tagName.toLowerCase();
      if (t === 'a') return el.hasAttribute('href') ? 'link' : null;
      if (t === 'button' || t === 'summary') return 'button';
      if (t === 'select') return el.multiple ? 'listbox' : 'combobox';
      if (t === 'textarea') return 'textbox';
      if (t === 'input') {
        const ty = (el.getAttribute('type') || 'text').toLowerCase();
        if (ty === 'hidden') return null;
        if (['button', 'submit', 'reset', 'image'].includes(ty)) return 'button';
        if (ty === 'checkbox' || ty === 'radio') return ty;
        if (ty === 'range') return 'slider';
        if (ty === 'file') return 'file upload';
        if (ty === 'password') return 'password field';
        return 'textbox';
      }
      if (el.isContentEditable && el.getAttribute('contenteditable') !== null) return 'textbox';
      return null;
    }

    const INTERACTIVE = new Set(['link', 'button', 'textbox', 'password field', 'checkbox', 'radio',
      'combobox', 'listbox', 'slider', 'switch', 'tab', 'menuitem', 'menuitemcheckbox',
      'menuitemradio', 'option', 'searchbox', 'spinbutton', 'file upload', 'treeitem']);

    function name(el) {
      const doc = el.ownerDocument;
      const lb = el.getAttribute('aria-labelledby');
      if (lb) {
        const s = lb.split(/\s+/).map((id) => doc.getElementById(id)?.innerText || '').join(' ');
        if (clean(s)) return clean(s);
      }
      const al = el.getAttribute('aria-label');
      if (al && clean(al)) return clean(al);
      if (el.labels && el.labels.length) {
        const s = [...el.labels].map((l) => l.innerText).join(' ');
        if (clean(s)) return clean(s);
      }
      const t = el.tagName.toLowerCase();
      if (t === 'input') {
        const ty = (el.getAttribute('type') || '').toLowerCase();
        if (['button', 'submit', 'reset'].includes(ty)) return clean(el.value || ty);
        if (ty === 'image') return clean(el.alt || 'submit');
      }
      if (t === 'img') return clean(el.alt);
      const ph = el.getAttribute('placeholder');
      if (['input', 'textarea'].includes(t)) return clean(ph || el.title || el.name || '');
      if (t === 'select') return clean(el.title || el.name || '');
      let txt = clean(el.innerText, 120);
      if (!txt) {
        const img = el.querySelector && el.querySelector('img[alt]');
        txt = clean((img && img.alt) || el.title || '');
      }
      return txt;
    }

    function value(el, r) {
      const t = el.tagName.toLowerCase();
      if (r === 'checkbox' || r === 'radio' || r === 'switch') {
        const on = el.checked ?? el.getAttribute('aria-checked') === 'true';
        return on ? ' [checked]' : '';
      }
      if (t === 'select') {
        const o = el.selectedOptions && el.selectedOptions[0];
        return o ? ` value=${q(clean(o.text, 60))}` : '';
      }
      if (t === 'input' || t === 'textarea') {
        const v = el.value || '';
        if (!v) return '';
        const ty = (el.getAttribute('type') || '').toLowerCase();
        const cc = /cc-number|cc-csc/.test(el.getAttribute('autocomplete') || '');
        if (ty === 'password' || cc || secret.has(el)) return ' value="••••"';
        return ` value=${q(clean(v, 80))}`;
      }
      return '';
    }

    function snapshot(max) {
      const lines = [];
      let size = 0;
      let full = false;
      let buf = '';
      const push = (line) => {
        if (full) return;
        if (size + line.length > max) {
          full = true;
          return;
        }
        lines.push(line);
        size += line.length + 1;
      };
      const flush = () => {
        const t = clean(buf, 400);
        buf = '';
        if (t) push('- text: ' + t);
      };
      const BLOCK = /^(block|flex|grid|list-item|table|table-row|table-cell|flow-root)$/;
      const walk = (node, depth, textOk) => {
        if (full || depth > 200) return;
        if (node.nodeType === 3) {
          // Exactly as written: adjacent inline pieces stay together, so a
          // secret split across elements is still recognized.
          if (textOk) buf += node.nodeValue;
          return;
        }
        if (node.nodeType !== 1 && node.nodeType !== 11) return;
        const el = node;
        if (node.nodeType === 1) {
          const t = el.tagName.toLowerCase();
          if (['script', 'style', 'noscript', 'template', 'svg', 'head', 'meta', 'link'].includes(t)) return;
          if (!visible(el)) return;
          const r = role(el);
          if (r && INTERACTIVE.has(r)) {
            flush();
            let line = `- ${r} ${q(name(el))} [ref=${refOf(el)}]${value(el, r)}`;
            if (el.disabled || el.getAttribute('aria-disabled') === 'true') line += ' [disabled]';
            const ex = el.getAttribute('aria-expanded');
            if (ex) line += ex === 'true' ? ' [expanded]' : ' [collapsed]';
            if (el === el.ownerDocument.activeElement) line += ' [focused]';
            if (r === 'link') {
              const h = el.getAttribute('href') || '';
              if (h && !h.startsWith('javascript:') && h !== '#') line += ' -> ' + clean(el.href, 120);
            }
            if (t === 'select') {
              const opts = [...el.options].slice(0, 25).map((o) => clean(o.text, 40)).filter(Boolean);
              if (opts.length) line += ' options: ' + opts.map(q).join(', ') + (el.options.length > 25 ? ', …' : '');
            }
            push(line);
            return;
          }
          if (/^h[1-6]$/.test(t) || r === 'heading') {
            flush();
            push(`- heading ${q(clean(el.innerText, 200))} [level=${t[1] || el.getAttribute('aria-level') || 2}]`);
            textOk = false;
          } else if (t === 'img') {
            const alt = clean(el.alt, 120);
            if (alt) {
              flush();
              push(`- img ${q(alt)}`);
            }
            return;
          } else if (t === 'iframe' || t === 'frame') {
            flush();
            let doc = null;
            try { doc = el.contentDocument; } catch { doc = null; }
            if (doc && doc.body) walk(doc.body, depth + 1, true);
            else push(`- iframe ${q(clean(el.title || el.name || ''))} (contents not readable)`);
            flush();
            return;
          } else if (t === 'br') {
            buf += ' ';
            return;
          }
          const block = BLOCK.test(el.ownerDocument.defaultView.getComputedStyle(el).display);
          if (block) flush();
          let kids;
          if (el.shadowRoot) kids = el.shadowRoot.childNodes;
          else if (t === 'slot') kids = el.assignedNodes().length ? el.assignedNodes() : el.childNodes;
          else kids = el.childNodes;
          for (const k of kids) walk(k, depth + 1, textOk);
          if (block || /^h[1-6]$/.test(t)) flush();
          return;
        }
        for (const k of el.childNodes) walk(k, depth + 1, textOk);
      };
      refs.clear();
      walk(document.body || document.documentElement, 0, true);
      flush();
      return { text: lines.join('\n'), truncated: full, url: redact(location.href), title: clean(document.title, 300) };
    }

    function el(ref) {
      const e = refs.get(ref);
      if (!e || !e.isConnected) return null;
      return e;
    }

    // Center of an element in top-level viewport CSS pixels.
    function point(ref) {
      let e = el(ref);
      if (!e) return { error: 'stale' };
      let rect = e.getBoundingClientRect();
      if ((rect.width < 1 || rect.height < 1) && e.labels && e.labels[0]) {
        e = e.labels[0];
      }
      e.scrollIntoView({ block: 'center', inline: 'center', behavior: 'instant' });
      rect = e.getBoundingClientRect();
      if (rect.width < 1 || rect.height < 1) return { error: 'hidden' };
      let x = rect.left + rect.width / 2;
      let y = rect.top + rect.height / 2;
      let win = e.ownerDocument.defaultView;
      while (win && win.frameElement) {
        const fe = win.frameElement;
        const fr = fe.getBoundingClientRect();
        x += fr.left + fe.clientLeft;
        y += fr.top + fe.clientTop;
        win = fe.ownerDocument.defaultView;
      }
      const hit = document.elementFromPoint(x, y);
      let covered = null;
      if (hit && e.ownerDocument === document && hit !== e && !e.contains(hit) && !hit.contains(e)) {
        covered = clean(hit.innerText || hit.getAttribute('aria-label') || hit.tagName.toLowerCase(), 60);
      }
      return { x, y, covered };
    }

    function focus(ref) {
      const e = el(ref);
      if (!e) return { error: 'stale' };
      const t = e.tagName.toLowerCase();
      if (t === 'select') return { error: 'select' };
      e.scrollIntoView({ block: 'center', behavior: 'instant' });
      e.focus();
      if (t === 'input' || t === 'textarea') {
        try { e.select(); } catch { /* some input types cannot select */ }
      } else if (e.isContentEditable) {
        const r = e.ownerDocument.createRange();
        r.selectNodeContents(e);
        const s = e.ownerDocument.defaultView.getSelection();
        s.removeAllRanges();
        s.addRange(r);
      }
      return { ok: focused(e) };
    }

    // The element really has focus, in its document and every frame up.
    function focused(e) {
      // :focus also works inside shadow roots.
      if (!(e.matches(':focus') || e.matches(':focus-within'))) return false;
      let win = e.ownerDocument.defaultView;
      while (win && win.frameElement) {
        const fe = win.frameElement;
        if (fe.ownerDocument.activeElement !== fe) return false;
        win = fe.ownerDocument.defaultView;
      }
      return true;
    }

    // Fill one field in a single step, bound to the exact element: check
    // it belongs to the expected document (and so origin), mask it first if
    // it gets a secret, set the value through the native setter (so page
    // frameworks see it), and fire the events a typing user would. Nothing
    // goes through "whatever has focus", so a page cannot redirect it.
    function fillField(ref, value, hide, expectDoc) {
      if (expectDoc !== doc) return { ok: false, changed: true };
      const e = el(ref);
      if (!e) return { ok: false };
      const t = e.tagName.toLowerCase();
      if (!['input', 'textarea'].includes(t) || e.disabled || e.readOnly) return { ok: false };
      if (e.ownerDocument.location.origin !== location.origin) return { ok: false, changed: true };
      if (hide) {
        secret.add(e);
        e.style.setProperty('-webkit-text-security', 'disc', 'important');
        if (!maskedNow(e)) return { ok: false };
      }
      e.focus();
      const proto = t === 'input' ? HTMLInputElement.prototype : HTMLTextAreaElement.prototype;
      const setter = Object.getOwnPropertyDescriptor(proto, 'value').set;
      setter.call(e, '');
      setter.call(e, value);
      e.dispatchEvent(new InputEvent('input', { bubbles: true, inputType: 'insertText' }));
      e.dispatchEvent(new Event('change', { bubbles: true }));
      // Card forms often reformat numbers ("4242 4242 ..."): same digits
      // count as filled.
      const digits = /^\d+$/.test(value) && e.value.replace(/\D/g, '') === value;
      return { ok: e.value === value || digits };
    }

    // Whether the element still has the focus (before pressing Enter).
    function hasFocus(ref) {
      const e = el(ref);
      return { ok: !!e && focused(e) };
    }

    // The type tool: replace an input's or textarea's text on that exact
    // element in one step (no ambient focus a page could redirect).
    // Rich editors (contenteditable) are focused and typed into instead.
    function setText(ref, value) {
      const e = el(ref);
      if (!e) return { error: 'stale' };
      const t = e.tagName.toLowerCase();
      if (t === 'select') return { error: 'select' };
      if (t === 'input' || t === 'textarea') {
        if (e.disabled || e.readOnly) return { error: 'hidden' };
        e.scrollIntoView({ block: 'center', behavior: 'instant' });
        e.focus();
        const proto = t === 'input' ? HTMLInputElement.prototype : HTMLTextAreaElement.prototype;
        const setter = Object.getOwnPropertyDescriptor(proto, 'value').set;
        setter.call(e, value);
        e.dispatchEvent(new InputEvent('input', { bubbles: true, inputType: 'insertText', data: value }));
        e.dispatchEvent(new Event('change', { bubbles: true }));
        return { ok: true, set: true };
      }
      const f = focus(ref);
      return f.error ? f : { ok: f.ok, set: false };
    }

    // The field shows dots right now (its type or style may have changed
    // since Iris masked it).
    function maskedNow(e) {
      if ((e.type || '').toLowerCase() === 'password') return true;
      const st = e.ownerDocument.defaultView.getComputedStyle(e);
      const sec = st.getPropertyValue('-webkit-text-security');
      return !!sec && sec !== 'none';
    }

    // Everything the page shows as text, for checking that no secret is
    // visible before a screenshot (never returned to the agent). Masked
    // fields and password fields show dots, so their values are left out.
    function pageText() {
      return docs().map((d) => {
        const out = [d.title, d.location.href, d.body ? d.body.innerText : ''];
        // Open shadow roots too: innerText skips them.
        const walk = (root) => {
          for (const e of root.querySelectorAll('*')) {
            const t = e.tagName.toLowerCase();
            if ((t === 'input' || t === 'textarea') && !maskedNow(e)) out.push(e.value || '');
            if (e.shadowRoot) {
              out.push(e.shadowRoot.textContent || '');
              walk(e.shadowRoot);
            }
          }
        };
        walk(d);
        return out.join('\n');
      }).join('\n');
    }

    function choose(ref, wanted) {
      const e = el(ref);
      if (!e) return { error: 'stale' };
      if (e.tagName.toLowerCase() !== 'select') return { error: 'not a select' };
      const want = (Array.isArray(wanted) ? wanted : [wanted]).map((w) => String(w).trim().toLowerCase());
      const o = [...e.options].find((x) => want.includes(x.value.trim().toLowerCase()) || want.includes(clean(x.text).toLowerCase()));
      if (!o) return { error: 'no such option' };
      e.value = o.value;
      e.dispatchEvent(new Event('input', { bubbles: true }));
      e.dispatchEvent(new Event('change', { bubbles: true }));
      return { ok: true, chosen: clean(o.text, 60) };
    }

    // Every same-origin document (main page and readable iframes).
    function docs() {
      const out = [document];
      for (let i = 0; i < out.length && i < 20; i++) {
        for (const f of out[i].querySelectorAll('iframe,frame')) {
          try { if (f.contentDocument) out.push(f.contentDocument); } catch { /* cross-origin */ }
        }
      }
      return out;
    }

    const usable = (e) => visible(e) && !e.disabled && !e.readOnly && e.getBoundingClientRect().width > 0;
    const attrList = (e) => [e.name, e.id, e.getAttribute('autocomplete'), e.getAttribute('placeholder'),
      e.getAttribute('aria-label'), e.labels && [...e.labels].map((l) => l.innerText).join(' ')]
      .filter(Boolean).map((a) => String(a).trim());
    const attrs = (e) => attrList(e).join(' ');
    // Some attribute (name, id, placeholder, label...) matches.
    const any = (e, re) => attrList(e).some((a) => re.test(a));

    function loginFields() {
      const inputs = docs().flatMap((d) => [...d.querySelectorAll('input')]).filter(usable);
      const pass = inputs.find((e) => (e.type || '').toLowerCase() === 'password'
        && !/new-password/.test(e.getAttribute('autocomplete') || ''))
        || inputs.find((e) => (e.type || '').toLowerCase() === 'password');
      const userish = (e) => TEXTY.test((e.getAttribute('type') || '').toLowerCase()) && e.type !== 'password';
      let user = null;
      const scope = pass ? inputs.filter((e) => (pass.form ? e.form === pass.form : true)) : inputs;
      const tagged = scope.find((e) => userish(e) && /username|email/.test(e.getAttribute('autocomplete') || ''));
      if (tagged) user = tagged;
      else if (pass) {
        const before = scope.filter((e) => userish(e) && (e.compareDocumentPosition(pass) & Node.DOCUMENT_POSITION_FOLLOWING));
        user = before.at(-1) || null;
      } else {
        user = scope.find((e) => userish(e) && (e.type === 'email' || /user|email|login|account|identifier|phone/i.test(attrs(e)))) || null;
      }
      return { user: user && refOf(user), pass: pass && refOf(pass) };
    }

    function cardFields() {
      const all = docs().flatMap((d) => [...d.querySelectorAll('input,select')]).filter(usable);
      const ac = (e, re) => re.test(e.getAttribute('autocomplete') || '');
      const pick = (list, acRe, re) => list.find((e) => ac(e, acRe)) || list.find((e) => any(e, re));
      const number = pick(all, /cc-number/, /card.?num|cc.?num|cardnum|card.?no\b/i);
      const cvc = pick(all, /cc-csc/, /^(cvc|cvv|csc|cvn)\b|security.?code|card.?code|verification/i);
      const holder = pick(all, /cc-name/, /name.?on.?card|card.?holder/i);
      const rest = all.filter((e) => e !== number && e !== cvc && e !== holder);
      // One "MM/YY" field, or separate month and year fields.
      const exp = rest.find((e) => ac(e, /(^|\s)cc-exp(\s|$)/))
        || rest.find((e) => any(e, /expir|exp.?date|mm\s*\/\s*yy/i) && !any(e, /month|year/i));
      const parts = rest.filter((e) => e !== exp);
      const month = pick(parts, /cc-exp-month/, /exp\w*.?month|^mm$|month/i);
      const year = pick(parts.filter((e) => e !== month), /cc-exp-year/, /exp\w*.?year|^yy(yy)?$|year/i);
      const split = !!(month && year);
      const r = (e) => (e ? refOf(e) : null);
      const isSelect = (e) => !!e && e.tagName.toLowerCase() === 'select';
      return {
        number: r(number), cvc: r(cvc), name: r(holder),
        exp: split ? null : r(exp),
        month: split ? r(month) : null, year: split ? r(year) : null,
        selects: { exp: isSelect(exp), month: isSelect(month), year: isSelect(year) },
        // A two-digit year box wants "31", not "2031".
        shortYear: !!year && !isSelect(year) && (year.maxLength === 2 || any(year, /^yy$|\byy\b/i) && !any(year, /yyyy/i)),
      };
    }

    function hasText(text) {
      const want = String(text).toLowerCase();
      return docs().some((d) => (d.body ? d.body.innerText : '').toLowerCase().includes(want));
    }

    return { snapshot, point, focus, hasFocus, setText, choose, loginFields, cardFields, fillField, pageText, hasText, setSecrets,
      info: () => ({ url: location.href, title: document.title, origin: location.origin, doc }) };
  })();
}
