// The Iris hub: Connections, Logins & wallet, System files (Iris's computer
// lives in computer.js).
import { html, useEffect, useState } from './h.js';
import { get, post, patch, put, del, seg } from './api.js';
import { S, set, useStore, go, back, toast, isOwner } from './store.js';
import {
  Icon, Avatar, Row, Group, Page, Empty, Spinner, Toggle, LetterTile,
  agentName, ago, since, confirmSheet,
} from './ui.js';
import { describe, visible } from './status.js';

function useData(path, deps = []) {
  const [data, setData] = useState(null);
  const [err, setErr] = useState(null);
  const reload = () => get(path).then(setData).catch((e) => setErr(e.message));
  useEffect(() => { reload(); }, deps);
  return [data, reload, err];
}

// --------------------------------------------------------------------- hub

export function Hub() {
  useStore();
  const owner = isOwner();
  const [cons] = useData(owner ? '/connectors' : '/me');
  const [creds] = useData(owner ? '/credentials' : '/me');
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
      <${Row} icon="plug" title="Connections" sub=${nCon == null ? ' ' : nCon ? `${nCon} connected` : 'Nothing connected yet'} onClick=${() => go('iris/connections')} />
      <${Row} icon="lock" title="Logins & wallet" sub=${creds ? `${logins} login${logins === 1 ? '' : 's'} · ${cards} card${cards === 1 ? '' : 's'}` : ' '} onClick=${() => go('iris/logins')} />
      <${Row} icon="monitor" title=${`${agentName()}'s computer`} sub=${computer} onClick=${() => go('iris/computer')} />
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
  if (item.conversation_kind === 'job') return item.conversation_title === 'feed' ? 'Feed' : 'Background check-in';
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
  'MEMORY.md': "{name}'s long-term memory",
};
const AREA = { memory: 'Memory notes', workspace: "{name}'s working folder", persona: 'Persona files' };
const fill = (s) => s.replace(/\{name\}/g, agentName());

export function Files() {
  const [data] = useData('/files');
  return html`<${Page} title="System files" sub=${`What makes ${agentName()}, ${agentName()}. Plain text you can read and edit.`}>
    ${!data ? html`<${Spinner} />` : html`
      <${Group} label="Personality and memory">
        ${data.system_files.map((f) => html`<${Row} icon="file" title=${fill(FRIENDLY[f.name] || f.description)}
          sub=${f.name + (f.exists ? '' : ' · empty')} onClick=${() => go(`iris/files/system/${f.name}`)} />`)}
      <//>
      <${Group} label="Folders">
        ${data.folders.filter((d) => d.area !== 'persona').map((d) => html`<${Row} icon="folder" title=${fill(AREA[d.area] || d.description)}
          sub=${d.area} onClick=${() => go(`iris/files/${d.area}`)} />`)}
      <//>`}
  <//>`;
}

export function FileEditor({ area, path }) {
  const system = area === 'system';
  const url = system ? `/files/system/${seg(path)}` : `/files/content/${seg(area)}?path=${encodeURIComponent(path)}`;
  const [text, setText] = useState(null);
  const [orig, setOrig] = useState('');
  const [busy, setBusy] = useState(false);
  useEffect(() => {
    get(url).then((r) => { setText(r.content); setOrig(r.content); }).catch((e) => toast(e.message));
  }, [url]);
  const dirty = text != null && text !== orig;
  const save = async () => {
    setBusy(true);
    try { await put(url, { content: text }); setOrig(text); toast('Saved. I’ll use it from my next reply.'); } catch (e) { toast(e.message); }
    setBusy(false);
  };
  const remove = async () => {
    if (!(await confirmSheet({ title: `Delete ${path}?`, yes: 'Delete', danger: true }))) return;
    try { await del(url); back(); } catch (e) { toast(e.message); }
  };
  const name = path.split('/').pop();
  return html`<${Page} title=${system ? fill(FRIENDLY[path] || path) : name} sub=${system ? path : `${area}/${path}`}
    right=${html`<button class="b1 sm" disabled=${!dirty || busy} onClick=${save}>Save</button>`}>
    ${text == null ? html`<${Spinner} />` : html`<textarea class="editor" value=${text} spellcheck=${false}
      onInput=${(e) => setText(e.target.value)} aria-label=${`Edit ${name}`} />`}
    ${!system && html`<${Group}><${Row} title="Delete file" danger chevron=${false} onClick=${remove} /><//>`}
  <//>`;
}

export function Folder({ area, path = '' }) {
  const [data, , err] = useData(`/files/browse/${seg(area)}${path ? `?path=${encodeURIComponent(path)}` : ''}`, [area, path]);
  const sub = path ? `${area}/${path}` : area;
  const open = (e) => go(`iris/files/${area}/${[path, e.name].filter(Boolean).join('/')}${e.type === 'dir' ? '' : '?edit=1'}`);
  return html`<${Page} title=${path ? path.split('/').pop() : fill(AREA[area] || area)} sub=${sub}>
    ${err ? html`<${Empty} icon="folder" title="Nothing here yet">This folder is empty.<//>`
      : !data ? html`<${Spinner} />` : data.entries.length ? html`<${Group}>
        ${data.entries.map((e) => html`<${Row} icon=${e.type === 'dir' ? 'folder' : 'file'} title=${e.name}
          sub=${e.type === 'dir' ? 'Folder' : `${Math.max(1, Math.round((e.size || 0) / 1024))} KB · ${ago(e.updated_at)}`} onClick=${() => open(e)} />`)}<//>`
      : html`<${Empty} icon="folder" title="Empty folder">Nothing in here yet.<//>`}
  <//>`;
}
