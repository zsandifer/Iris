// The Iris hub: Routines (routines.js), Connections, Logins & wallet, System files (Iris's computer
// lives in computer.js).
import { html, useEffect, useState } from './h.js';
import { get, post, patch, put, del, seg, sessionGen } from './api.js';
import { S, set, useStore, go, back, toast, isOwner, loadRoutines } from './store.js';
import {
  Icon, Avatar, Row, Group, Page, Empty, Spinner, Toggle, LetterTile,
  agentName, ago, since, confirmSheet,
} from './ui.js';
import { describe, visible } from './status.js';
import { md } from './md.js';
import { routinesSub } from './routines.js';

// The last answer per path, so a screen you come back to shows it at once
// instead of an empty state that reflows when the request returns. Keyed by
// session so a new sign-in never sees the previous one's answers.
let seen = new Map();
let seenGen = sessionGen();
function remembered() {
  if (seenGen !== sessionGen()) { seen = new Map(); seenGen = sessionGen(); }
  return seen;
}
function useData(path, deps = []) {
  const [data, setData] = useState(() => remembered().get(path) ?? null);
  const [err, setErr] = useState(null);
  const reload = () => {
    const gen = sessionGen();
    return get(path).then((d) => { if (gen === sessionGen()) remembered().set(path, d); setData(d); }).catch((e) => setErr(e.message));
  };
  useEffect(() => { reload(); }, deps);
  return [data, reload, err];
}

// --------------------------------------------------------------------- hub

export function Hub() {
  useStore();
  const owner = isOwner();
  const [cons] = useData(owner ? '/connectors' : '/me');
  const [creds] = useData(owner ? '/credentials' : '/me');
  useEffect(() => { if (owner) loadRoutines().catch(() => {}); }, [owner]);
  const working = Object.keys(S.live).length > 0;
  const computer = S.settings?.paused ? 'paused' : S.browser?.busy ? 'browsing' : working ? 'working' : 'idle';
  const nCon = cons?.connectors?.filter((c) => c.enabled).length;
  const logins = creds?.credentials?.filter((c) => c.kind === 'login').length;
  const cards = creds?.credentials?.filter((c) => c.kind === 'card').length;
  return html`<${Page} title=${agentName()} sub="Everything about your ${agentName()}" onBack=${() => go('chat')}>
    <div class="hub-hero"><${Avatar} size=${64} /><div><b>${agentName()}</b>
      <small>${S.online ? 'Online on your server' : 'Reconnecting to your server…'}</small></div></div>
    <${Group}>
      ${owner && html`
      <${Row} icon="repeat" title="Routines" sub=${routinesSub()} onClick=${() => go('iris/routines')} />
      <${Row} icon="plug" title="Connections" sub=${nCon == null ? '\u00a0' : nCon ? `${nCon} connected` : 'Nothing connected yet'} onClick=${() => go('iris/connections')} />
      <${Row} icon="lock" title="Logins & wallet" sub=${creds ? `${logins} login${logins === 1 ? '' : 's'} · ${cards} card${cards === 1 ? '' : 's'}` : '\u00a0'} onClick=${() => go('iris/logins')} />
      <${Row} icon="monitor" title=${`${agentName()}'s computer`} sub=${computer} onClick=${() => go('iris/computer')} />
      <${Row} icon="memory" title="Memory" sub="What I remember about you" onClick=${() => go('iris/memory')} />
      <${Row} icon="folder" title="System files" sub="Personality, memory, and notes" onClick=${() => go('iris/files')} />`}
      <${Row} icon="sliders" title="Settings" onClick=${() => go('settings')} />
    <//>
  <//>`;
}

// ---------------------------------------------------------- logins/wallet

function CredentialForm({ kind, onDone }) {
  const [f, setF] = useState({});
  const [busy, setBusy] = useState(false);
  const up = (k) => (e) => setF({ ...f, [k]: e.target.value });
  const save = async (e) => {
    e.preventDefault();
    setBusy(true);
    const body = { kind, ...f };
    if (kind === 'card') {
      const [m, y] = String(f.exp || '').split('/').map((x) => parseInt(x, 10));
      delete body.exp;
      if (m) body.exp_month = m;
      if (y) body.exp_year = y < 100 ? 2000 + y : y;
    }
    try { await post('/credentials', body); onDone(); } catch (err) { toast(err.message); }
    setBusy(false);
  };
  return html`<form class="form" onSubmit=${save}>
    <label class="lbl">Name</label><input class="field" required placeholder=${kind === 'card' ? 'Everyday card' : 'Grocery store'} onInput=${up('label')} />
    ${kind === 'login' ? html`
      <label class="lbl">Website (exact address, including port)</label><input class="field" placeholder="https://example.com" autocapitalize="off" onInput=${up('site')} />
      <label class="lbl"><input type="checkbox" checked=${!!f.allow_subdomains} onChange=${(e) => setF({ ...f, allow_subdomains: e.target.checked })} /> Also allow subdomains (same scheme and port)</label>
      <label class="lbl">Username or email</label><input class="field" autocapitalize="off" autocomplete="off" onInput=${up('username')} />
      <label class="lbl">Password</label><input class="field" type="password" autocomplete="new-password" required onInput=${up('password')} />`
    : html`
      <label class="lbl">Card number</label><input class="field" inputmode="numeric" autocomplete="off" required onInput=${up('number')} />
      <div class="two"><div><label class="lbl">Expires (MM/YY)</label><input class="field" placeholder="08/29" onInput=${up('exp')} /></div>
      <div><label class="lbl">CVC</label><input class="field" inputmode="numeric" autocomplete="off" onInput=${up('cvc')} /></div></div>
      <label class="lbl">Name on card</label><input class="field" onInput=${up('cardholder')} />`}
    <button class="b1 wide" disabled=${busy}>Save</button>
  </form>`;
}

export function Logins() {
  const [data, reload] = useData('/credentials');
  const [adding, setAdding] = useState(null);
  const list = data?.credentials || [];
  const remove = async (c) => {
    if (!(await confirmSheet({ title: `Delete ${c.label}?`, yes: 'Delete', danger: true }))) return;
    try { await del(`/credentials/${seg(c.id)}`); reload(); } catch (e) { toast(e.message); }
  };
  if (adding) {
    return html`<${Page} title=${adding === 'card' ? 'Add card' : 'Add login'} sub="Stored encrypted on your server. You won't see the secret again." onBack=${() => setAdding(null)}>
      <${CredentialForm} kind=${adding} onDone=${() => { setAdding(null); reload(); }} /><//>`;
  }
  const logins = list.filter((c) => c.kind === 'login');
  const cards = list.filter((c) => c.kind === 'card');
  return html`<${Page} title="Logins & wallet" sub=${`${agentName()} can use these but never sees them`}>
    <div class="lock"><${Icon} name="lock" size=${15} /> Stored encrypted on your server, separate from ${agentName()}. ${agentName()} signs in for you in its own browser: the password is typed into the site directly and never enters its chat or memory. Cards always ask you first, every time. When you take over the browser, tap the key to fill one yourself.</div>
    ${!data ? html`<${Spinner} />` : html`
    <${Group} label="Logins">
      ${logins.map((c) => html`<${Row} icon=${html`<${LetterTile} name=${c.metadata.site || c.label} />`} title=${c.label}
        sub=${[c.metadata.site, c.metadata.username].filter(Boolean).join(' · ')} value=${c.last_used_at ? `used ${since(c.last_used_at)}` : ''}
        chevron=${false} right=${html`<button class="icon-btn" aria-label="Delete" onClick=${() => remove(c)}><${Icon} name="trash" size=${16} /></button>`} />`)}
      <${Row} title="Add login" accent icon="plus" onClick=${() => setAdding('login')} chevron=${false} />
    <//>
    <${Group} label="Wallet">
      ${cards.map((c) => html`<${Row} icon="card" title=${`${(c.metadata.brand || 'Card').replace(/^./, (x) => x.toUpperCase())} ••${c.metadata.last4 || ''}`}
        sub=${[c.label, c.metadata.exp_month && `exp ${String(c.metadata.exp_month).padStart(2, '0')}/${String(c.metadata.exp_year || '').slice(-2)}`].filter(Boolean).join(' · ')}
        value="always asks" chevron=${false} right=${html`<button class="icon-btn" aria-label="Delete" onClick=${() => remove(c)}><${Icon} name="trash" size=${16} /></button>`} />`)}
      <${Row} title="Add card" accent icon="plus" onClick=${() => setAdding('card')} chevron=${false} />
    <//>`}
  <//>`;
}

// --------------------------------------------------------------- computer

export function activityTitle(item) {
  const steps = [];
  for (const a of (item.activity || []).filter(visible)) {
    const p = describe(a)[1];
    if (steps.at(-1) !== p) steps.push(p);
  }
  if (!steps.length) return 'Worked on something';
  return steps.length === 1 ? steps[0] : `${steps[0]} + ${steps.length - 1} more`;
}

export function placeLabel(item) {
  if (item.conversation_kind === 'job') {
    if (item.conversation_title === 'feed') return 'Feed';
    if (String(item.conversation_title || '').startsWith('Routine: ')) return item.conversation_title;
    return 'Background check-in';
  }
  if (item.conversation_kind === 'main') return 'Main chat';
  return item.conversation_title || 'Side chat';
}

// ------------------------------------------------------------ system files

const FRIENDLY = {
  'IDENTITY.md': `Who ${'{name}'} is`,
  'SOUL.md': 'How {name} talks and thinks',
  'USER.md': 'What {name} knows about you',
  'TOOLS.md': 'Notes on tools and accounts',
  'PROACTIVE_PREFERENCES.md': 'When {name} acts on its own',
  'HEARTBEAT.md': 'What {name} checks on',
  'MEMORY.md': 'What {name} always keeps in mind',
};
const AREA = { memory: "{name}'s memory", workspace: "{name}'s working folder", persona: 'Persona files' };
// Generated from what Iris remembers; changed through chat, not edited here.
const READ_ONLY_NOTE = 'A copy of what I remember, kept up to date as I learn. To change something, open Memory or ask me in a chat.';
function ReadOnlyNote() {
  return html`<div class="lock"><${Icon} name="lock" size=${15} /> ${READ_ONLY_NOTE}
    <button class="lk-m" onClick=${() => go('iris/memory')}>Open Memory ›</button></div>`;
}
const fill = (s) => s.replace(/\{name\}/g, agentName());

export function Files() {
  const [data] = useData('/files');
  return html`<${Page} title="System files" sub=${`What makes ${agentName()}, ${agentName()}. Plain text you can read and edit.`}>
    ${!data ? html`<${Spinner} />` : html`
      <${Group} label="Personality and memory">
        ${data.system_files.map((f) => html`<${Row} icon="file" title=${fill(FRIENDLY[f.name] || f.description)}
          sub=${f.name + (f.read_only ? ' · read-only copy' : f.exists ? '' : ' · empty')} onClick=${() => go(`iris/files/system/${f.name}`)} />`)}
      <//>
      <${Group} label="Folders">
        ${data.folders.filter((d) => d.area !== 'persona').map((d) => html`<${Row} icon="folder" title=${fill(AREA[d.area] || d.description)}
          sub=${d.read_only ? 'Read-only copy' : d.area} onClick=${() => go(`iris/files/${d.area}`)} />`)}
      <//>`}
  <//>`;
}

export function FileEditor({ area, path }) {
  const system = area === 'system';
  const readOnly = area === 'memory' || (system && path === 'MEMORY.md');
  const url = system ? `/files/system/${seg(path)}` : `/files/content/${seg(area)}?path=${encodeURIComponent(path)}`;
  const [text, setText] = useState(null);
  const [orig, setOrig] = useState('');
  const [busy, setBusy] = useState(false);
  // Files open as rendered Markdown; Edit switches to the plain text.
  const [editing, setEditing] = useState(false);
  useEffect(() => {
    get(url).then((r) => { setText(r.content); setOrig(r.content); if (!readOnly && !r.content.trim()) setEditing(true); })
      .catch((e) => toast(e.message));
  }, [url]);
  const dirty = text != null && text !== orig;
  const save = async () => {
    setBusy(true);
    try { await put(url, { content: text }); setOrig(text); setEditing(false); toast('Saved. I’ll use it from my next reply.'); } catch (e) { toast(e.message); }
    setBusy(false);
  };
  const remove = async () => {
    if (!(await confirmSheet({ title: `Delete ${path}?`, yes: 'Delete', danger: true }))) return;
    try { await del(url); back(); } catch (e) { toast(e.message); }
  };
  const name = path.split('/').pop();
  // HTML comments are notes for whoever edits the file; the view hides them
  // (md() still escapes whatever is left).
  let shown = text || '';
  for (let prev; prev !== shown;) { prev = shown; shown = shown.replace(/<!--[\s\S]*?-->/g, ''); }
  return html`<${Page} title=${system ? fill(FRIENDLY[path] || path) : name} sub=${system ? path : `${area}/${path}`}
    right=${readOnly || text == null ? null : editing
      ? html`<button class="b1 sm" disabled=${busy || (!dirty && !orig.trim())} onClick=${dirty ? save : () => setEditing(false)}>${dirty ? 'Save' : 'Done'}</button>`
      : html`<button class="b2 sm" onClick=${() => setEditing(true)}>Edit</button>`}>
    ${readOnly && html`<${ReadOnlyNote} />`}
    ${text == null ? html`<${Spinner} />` : editing ? html`<textarea class="editor" value=${text} spellcheck=${false}
      onInput=${(e) => setText(e.target.value)} aria-label=${`Edit ${name}`} />`
      : shown.trim() ? html`<div class="mdview" dangerouslySetInnerHTML=${{ __html: md(shown) }} />`
      : html`<${Empty} icon="file" title="Empty file">Nothing in here yet.<//>`}
    ${!system && !readOnly && html`<${Group}><${Row} title="Delete file" danger chevron=${false} onClick=${remove} /><//>`}
  <//>`;
}

export function Folder({ area, path = '' }) {
  const [data, , err] = useData(`/files/browse/${seg(area)}${path ? `?path=${encodeURIComponent(path)}` : ''}`, [area, path]);
  const sub = path ? `${area}/${path}` : area;
  const open = (e) => go(`iris/files/${area}/${[path, e.name].filter(Boolean).join('/')}${e.type === 'dir' ? '' : '?edit=1'}`);
  return html`<${Page} title=${path ? path.split('/').pop() : fill(AREA[area] || area)} sub=${sub}>
    ${area === 'memory' && html`<${ReadOnlyNote} />`}
    ${err ? html`<${Empty} icon="folder" title="Nothing here yet">This folder is empty.<//>`
      : !data ? html`<${Spinner} />` : data.entries.length ? html`<${Group}>
        ${data.entries.map((e) => html`<${Row} icon=${e.type === 'dir' ? 'folder' : 'file'} title=${e.name}
          sub=${e.type === 'dir' ? 'Folder' : `${Math.max(1, Math.round((e.size || 0) / 1024))} KB · ${ago(e.updated_at)}`} onClick=${() => open(e)} />`)}<//>`
      : html`<${Empty} icon="folder" title="Empty folder">Nothing in here yet.<//>`}
  <//>`;
}
