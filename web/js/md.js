// Small, safe Markdown renderer for agent replies. Everything is escaped
// first; only the tags created here reach the DOM, and links must be
// http(s).

const ESC = { '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;' };
export const esc = (s) => String(s).replace(/[&<>"']/g, (c) => ESC[c]);

const LINK = (url, label) => `<a href="${url}" target="_blank" rel="noopener noreferrer">${label}</a>`;

function fmt(t) {
  // t is already escaped.
  // Labels and URLs stop at the next '[', so text with many unclosed
  // brackets stays linear instead of backtracking quadratically.
  t = t.replace(/\[([^[\]\n]+)\]\((https?:\/\/[^\s)[]+)\)/g, (_, label, url) => LINK(url, label));
  t = t.replace(/(^|[\s(])(https?:\/\/[^\s<]*[^\s<.,;:!?)'"])/g, (_, pre, url) => pre + LINK(url, url));
  t = t.replace(/\*\*([^*\n]+)\*\*/g, '<strong>$1</strong>');
  t = t.replace(/__([^_\n]+)__/g, '<strong>$1</strong>');
  t = t.replace(/(^|[^*\w])\*([^*\n]+)\*(?!\w)/g, '$1<em>$2</em>');
  t = t.replace(/(^|[^_\w])_([^_\n]+)_(?!\w)/g, '$1<em>$2</em>');
  t = t.replace(/~~([^~\n]+)~~/g, '<del>$1</del>');
  return t;
}

export function inline(s) {
  return String(s)
    .split(/(`[^`\n]+`)/)
    .map((p) => (p.length > 1 && p.startsWith('`') && p.endsWith('`') ? `<code>${esc(p.slice(1, -1))}</code>` : fmt(esc(p))))
    .join('');
}

function table(rows) {
  const cells = (r) => r.trim().replace(/^\||\|$/g, '').split('|').map((c) => c.trim());
  const body = rows.filter((r) => !/^\s*\|?\s*:?-{2,}/.test(r));
  if (!body.length) return '';
  const [head, ...rest] = body;
  return `<div class="tbl"><table><thead><tr>${cells(head).map((c) => `<th>${inline(c)}</th>`).join('')}</tr></thead><tbody>${
    rest.map((r) => `<tr>${cells(r).map((c) => `<td>${inline(c)}</td>`).join('')}</tr>`).join('')}</tbody></table></div>`;
}

// Deeper quote markers render as plain text (no unbounded recursion).
const MAX_QUOTE_DEPTH = 8;

/** Markdown to an HTML string (safe to inject). */
export function md(src) {
  return render(src, 0);
}

function render(src, depth) {
  const lines = String(src || '').replace(/\r\n?/g, '\n').split('\n');
  const out = [];
  let para = [];
  const flush = () => {
    if (para.length) out.push(`<p>${para.map(inline).join('<br>')}</p>`);
    para = [];
  };
  for (let i = 0; i < lines.length; i++) {
    const line = lines[i];
    const fence = line.match(/^\s*```/);
    if (fence) {
      flush();
      const code = [];
      i++;
      while (i < lines.length && !/^\s*```/.test(lines[i])) code.push(lines[i++]);
      out.push(`<pre><code>${esc(code.join('\n'))}</code></pre>`);
      continue;
    }
    if (!line.trim()) { flush(); continue; }
    const hd = line.match(/^(#{1,6})\s+(.*)$/);
    if (hd) {
      flush();
      const n = Math.min(hd[1].length + 2, 6);
      out.push(`<h${n}>${inline(hd[2])}</h${n}>`);
      continue;
    }
    if (/^\s*([-*_])(\s*\1){2,}\s*$/.test(line)) { flush(); out.push('<hr>'); continue; }
    if (/^\s*\|.*\|\s*$/.test(line)) {
      flush();
      const rows = [];
      while (i < lines.length && /^\s*\|.*\|\s*$/.test(lines[i])) rows.push(lines[i++]);
      i--;
      out.push(table(rows));
      continue;
    }
    if (depth < MAX_QUOTE_DEPTH && /^\s*>/.test(line)) {
      flush();
      const q = [];
      while (i < lines.length && /^\s*>/.test(lines[i])) q.push(lines[i++].replace(/^\s*>\s?/, ''));
      i--;
      out.push(`<blockquote>${render(q.join('\n'), depth + 1)}</blockquote>`);
      continue;
    }
    const li = line.match(/^\s*([-*+]|\d+[.)])\s+(.*)$/);
    if (li) {
      flush();
      const ordered = /\d/.test(li[1]);
      const items = [];
      while (i < lines.length) {
        const m = lines[i].match(/^\s*([-*+]|\d+[.)])\s+(.*)$/);
        if (m) items.push(m[2]);
        else if (lines[i].trim() && /^\s{2,}/.test(lines[i]) && items.length) items[items.length - 1] += ' ' + lines[i].trim();
        else break;
        i++;
      }
      i--;
      const tag = ordered ? 'ol' : 'ul';
      out.push(`<${tag}>${items.map((x) => `<li>${inline(x.replace(/^\[( |x)\]\s+/i, (m0, c) => (c.trim() ? '✓ ' : '☐ ')))}</li>`).join('')}</${tag}>`);
      continue;
    }
    para.push(line);
  }
  flush();
  return out.join('');
}

/** Plain text preview (Markdown markers removed). */
export function plain(src, max = 0) {
  let t = String(src || '')
    .replace(/```[\s\S]*?```/g, ' ')
    .replace(/!\[[^[\]]*\]\([^)[]*\)/g, '')
    .replace(/\[([^[\]]+)\]\([^)[]*\)/g, '$1')
    .replace(/[#*_`>~|]/g, '')
    .replace(/\s+/g, ' ')
    .trim();
  if (max && t.length > max) t = t.slice(0, max - 1).replace(/\s+\S*$/, '') + '…';
  return t;
}
