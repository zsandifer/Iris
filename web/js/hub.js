// The Iris hub: Connections, Logins & wallet, Iris's computer, System files.
import { html, useEffect, useState } from './h.js';
import { get, post, patch, put, del, seg } from './api.js';
import { S, set, useStore, go, toast, isOwner, mainId } from './store.js';
import {
  Icon, Avatar, Row, Group, Page, Empty, Spinner, Toggle, LetterTile,
  agentName, ago, since, confirmSheet, stamp,
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
      <${Row} icon="monitor" title=${`${agentName()}'s computer`} sub=${working ? 'working' : 'idle'} onClick=${() => go('iris/computer')} />
      <${Row} icon="folder" title="System files" sub="Personality, memory, and notes" onClick=${() => go('iris/files')} />`}
      <${Row} icon="sliders" title="Settings" onClick=${() => go('settings')} />
    <//>
  <//>`;
}

// ------------------------------------------------------------- connections

const POPULAR = [
  ['Google', 'Gmail, Calendar, and Drive', '#4285f4'],
  ['Spotify', 'Music and podcasts', '#1db954'],
  ['Reddit', 'Feeds your For you', '#ff4500'],
  ['Notion', 'Notes and docs', '#111'],
  ['GitHub', 'Code and issues', '#24292f'],
];

function askIris(service) {
  const cid = mainId();
  set({ drafts: { ...S.drafts, [cid]: `Help me connect ${service}` } });
  go(`chat/${cid}`);
}

export function Connections() {
  const [data, reload] = useData('/connectors');
  const service = S.route.query.get('service');
  const list = data?.connectors || [];
  return html`<${Page} title="Connections" sub=${`Services ${agentName()} can use for you. Or just ask: "help me set up Spotify".`}>
    ${service && html`<div class="lock">Connecting ${service} takes a sign-in on its own site. Tap Ask ${agentName()} below and I'll walk you through it, or add it yourself as a custom connection.</div>`}
    ${!data ? html`<${Spinner} />` : html`
      <${Group} label="Connected">
        ${list.length ? list.map((c) => html`<${Row} icon=${html`<${LetterTile} name=${c.name} />`} title=${c.name}
          sub=${c.enabled ? (c.description || 'On') : 'Off'} onClick=${() => go(`iris/connections/${c.id}`)} />`)
          : html`<div class="r"><div class="rt mu">Nothing connected yet</div></div>`}
      <//>
      <${Group} label="Popular">
        ${POPULAR.map(([name, sub, color]) => html`<${Row} icon=${html`<div class="tile" style=${`background:${color}`}>${name[0]}</div>`}
          title=${name} sub=${sub} chevron=${false} right=${html`<button class="cn" onClick=${() => askIris(name)}>Ask ${agentName()}</button>`} />`)}
        <${Row} icon="plus" title="Custom (MCP server)" sub="For tinkerers: any MCP server" onClick=${() => go('iris/connections/new')} />
      <//>`}
  <//>`;
}

function KeyValues({ label, keys, value, onChange, placeholder }) {
  return html`<label class="lbl">${label}</label>
    <textarea class="field mono" rows="3" value=${value} placeholder=${placeholder}
      onInput=${(e) => onChange(e.target.value)} />
    ${keys?.length > 0 && html`<small class="mu">Saved: ${keys.join(', ')} (values hidden). Leave empty to keep them.</small>`}`;
}

const parseKV = (text) => Object.fromEntries(text.split('\n').map((l) => l.trim()).filter(Boolean).map((l) => {
  const i = l.search(/[=:]/);
  return i < 0 ? [l, ''] : [l.slice(0, i).trim(), l.slice(i + 1).trim()];
}));

export function ConnectorForm({ id }) {
  const isNew = id === 'new';
  const [c, setC] = useState(null);
  const [f, setF] = useState({ name: '', description: '', transport: 'stdio', command: '', args: '', url: '', env: '', headers: '' });
  const [busy, setBusy] = useState(false);
  useEffect(() => {
    if (isNew) return;
    get('/connectors').then((r) => {
      const x = r.connectors.find((k) => k.id === id);
      if (!x) return toast('That connection is gone.');
      setC(x);
      setF({ name: x.name, description: x.description, transport: x.transport, command: x.command || '', args: (x.args || []).join('\n'), url: x.url || '', env: '', headers: '' });
    }).catch((e) => toast(e.message));
  }, [id]);
  const up = (k) => (v) => setF({ ...f, [k]: v });
  const save = async (e) => {
    e.preventDefault();
    setBusy(true);
    const body = { description: f.description };
    if (f.transport === 'stdio') {
      body.command = f.command.trim();
      body.args = f.args.split('\n').map((a) => a.trim()).filter(Boolean);
      if (f.env.trim() || isNew) body.env = parseKV(f.env);
    } else {
      body.url = f.url.trim();
      if (f.headers.trim() || isNew) body.headers = parseKV(f.headers);
    }
    try {
      if (isNew) await post('/connectors', { ...body, name: f.name.trim(), transport: f.transport });
      else await patch(`/connectors/${seg(id)}`, body);
      toast('Saved');
      go('iris/connections');
    } catch (err) { toast(err.message); }
    setBusy(false);
  };
  const toggle = async (on) => {
    try { setC((await post(`/connectors/${seg(id)}/${on ? 'connect' : 'disconnect'}`)).connector); } catch (e) { toast(e.message); }
  };
  const remove = async () => {
    if (!(await confirmSheet({ title: `Remove ${c.name}?`, body: `${agentName()} won't be able to use it anymore.`, yes: 'Remove', danger: true }))) return;
    try { await del(`/connectors/${seg(id)}`); go('iris/connections'); } catch (e) { toast(e.message); }
  };
  if (!isNew && !c) return html`<${Page} title="Connection"><${Spinner} /><//>`;
  return html`<${Page} title=${isNew ? 'Custom connection' : c.name}
    sub=${isNew ? 'Add any MCP server. Secrets are encrypted on your server and never shown again.' : (c.description || 'A connected service')}>
    ${!isNew && html`<${Group}><${Row} title="On" sub="Off means it isn't offered to me" right=${html`<${Toggle} on=${c.enabled} onChange=${toggle} label="Connection on" />`} /><//>`}
    <form class="form" onSubmit=${save}>
      ${isNew && html`<label class="lbl">Name</label>
        <input class="field" value=${f.name} placeholder="e.g. calendar" pattern="[A-Za-z0-9_\\-]{1,48}" required onInput=${(e) => up('name')(e.target.value)} />
        <label class="lbl">How it runs</label>
        <div class="seg">${['stdio', 'http'].map((t) => html`<button type="button" class=${f.transport === t ? 'on' : ''} onClick=${() => up('transport')(t)}>${t === 'stdio' ? 'Program on the server' : 'Web address'}</button>`)}</div>`}
      <label class="lbl">Description</label>
      <input class="field" value=${f.description} placeholder="What it's for" onInput=${(e) => up('description')(e.target.value)} />
      ${f.transport === 'stdio' ? html`
        <label class="lbl">Command</label>
        <input class="field mono" value=${f.command} placeholder="npx" required onInput=${(e) => up('command')(e.target.value)} />
        <label class="lbl">Arguments (one per line)</label>
        <textarea class="field mono" rows="3" value=${f.args} placeholder=${'-y\nsome-mcp-server'} onInput=${(e) => up('args')(e.target.value)} />
        <${KeyValues} label="Secrets (KEY=value, one per line)" keys=${c?.env_keys} value=${f.env} onChange=${up('env')} placeholder="API_TOKEN=..." />`
      : html`
        <label class="lbl">URL</label>
        <input class="field mono" type="url" value=${f.url} placeholder="https://..." required onInput=${(e) => up('url')(e.target.value)} />
        <${KeyValues} label="Headers (Name: value, one per line)" keys=${c?.header_keys} value=${f.headers} onChange=${up('headers')} placeholder="Authorization: Bearer ..." />`}
      <button class="b1 wide" disabled=${busy}>${isNew ? 'Add connection' : 'Save'}</button>
    </form>
    ${!isNew && html`<${Group}><${Row} title="Remove connection" danger onClick=${remove} chevron=${false} /><//>`}
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
      <label class="lbl">Website</label><input class="field" placeholder="example.com" autocapitalize="off" onInput=${up('site')} />
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
    <div class="lock"><${Icon} name="lock" size=${15} /> Stored encrypted on your server, separate from ${agentName()}. When it's switched on, ${agentName()} signs in for you and the password never enters its chat or memory. Cards always ask before paying.
      ${data?.injection === 'not_implemented' && html`<br /><b>Not switched on yet:</b> ${agentName()} can't use these yet. You can store them now.`}</div>
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

export function Computer() {
  useStore();
  const [act] = useData('/activity?limit=8');
  const working = Object.keys(S.live).length > 0;
  const disconnectAll = async () => {
    if (!(await confirmSheet({ title: 'Disconnect everything?', body: `${agentName()} will stop using every connected service until you turn them back on in Connections.`, yes: 'Disconnect', danger: true }))) return;
    try {
      const { connectors } = await get('/connectors');
      await Promise.all(connectors.filter((c) => c.enabled).map((c) => post(`/connectors/${seg(c.id)}/disconnect`)));
      toast('All connections are off');
    } catch (e) { toast(e.message); }
  };
  return html`<${Page} title=${`${agentName()}'s computer`} sub=${`${agentName()}'s own space on your server`}>
    <div class="screen">
      <${Icon} name="monitor" size=${30} />
      <b>${working ? `${agentName()} is working` : `${agentName()} is idle`}</b>
      <small>A live view of ${agentName()}'s browser isn't available yet. When it is, you'll be able to watch and take over here, for example to log in.</small>
    </div>
    <div class="big2"><button disabled><${Icon} name="eye" size=${16} /> Watch</button><button class="p" disabled><${Icon} name="hand" size=${16} /> Take over</button></div>
    <${Group} label="Recently">
      ${!act ? html`<${Spinner} />` : act.items.length ? act.items.map((it) => html`<${Row} title=${activityTitle(it)} sub=${placeLabel(it)}
        value=${stamp(it.created_at)} chevron=${false} onClick=${it.conversation_kind === 'job' ? undefined : () => go(`chat/${it.conversation_id}`)} />`)
        : html`<div class="r"><div class="rt mu">Nothing yet</div></div>`}
      ${act?.items.length > 0 && html`<${Row} title="See all activity" accent onClick=${() => go('settings/activity')} />`}
    <//>
    <${Group} label="Safety" note="Pausing is coming soon. Disconnecting turns off every connection at once.">
      <${Row} title=${`Pause ${agentName()}`} right=${html`<${Toggle} on=${false} disabled onChange=${() => {}} label="Pause" />`} />
      <${Row} title="Disconnect everything" danger chevron=${false} onClick=${disconnectAll} />
    <//>
  <//>`;
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
    try { await del(url); history.back(); } catch (e) { toast(e.message); }
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
