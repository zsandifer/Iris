// Settings, AI engine (the only place model names appear), devices,
// people, and the activity log.
import { html, useEffect, useState, useRef } from './h.js';
import { api, get, post, put, del, setToken, seg } from './api.js';
import { S, set, useStore, go, toast, isOwner, loadSlots, loadEngines } from './store.js';
import {
  Icon, Row, Group, Page, Spinner, Toggle, LetterTile, Empty,
  agentName, ago, since, stamp, confirmSheet, promptSheet, openSheet, closeSheet,
} from './ui.js';
import { SignIn } from './cards.js';
import { pushState, enablePush, disablePush, isIOS } from './push.js';
import { activityTitle, placeLabel } from './hub.js';

const fmt12 = (hhmm) => {
  const [h, m] = String(hhmm || '').split(':').map(Number);
  if (Number.isNaN(h)) return hhmm;
  return `${h % 12 || 12}${m ? ':' + String(m).padStart(2, '0') : ''} ${h < 12 ? 'AM' : 'PM'}`;
};

function QuietHours() {
  const q = S.me?.quiet_hours;
  const Body = () => {
    const [start, setStart] = useState(q?.start || '22:00');
    const [end, setEnd] = useState(q?.end || '07:00');
    const save = async (value) => {
      try {
        const r = await put('/me/quiet-hours', { quiet_hours: value });
        set({ me: { ...S.me, quiet_hours: r.quiet_hours } });
        closeSheet();
      } catch (e) { toast(e.message); }
    };
    return html`<div class="sheet-body"><h3>Quiet hours</h3>
      <p class="mu">No notifications during these hours. Messages still arrive in the app, and anything that came in gets one summary notification when quiet hours end.</p>
      <div class="two"><div><label class="lbl">From</label><input class="field" type="time" value=${start} onInput=${(e) => setStart(e.target.value)} /></div>
      <div><label class="lbl">Until</label><input class="field" type="time" value=${end} onInput=${(e) => setEnd(e.target.value)} /></div></div>
      <div class="btns"><button class="b2" onClick=${() => save(null)}>Turn off</button>
      <button class="b1" onClick=${() => save({ start, end })}>Save</button></div></div>`;
  };
  return html`<${Row} icon="moon" title="Quiet hours" value=${q ? `${fmt12(q.start)}–${fmt12(q.end)}` : 'Off'} onClick=${() => openSheet(() => html`<${Body} />`)} />`;
}

function Notifications() {
  const [st, setSt] = useState(null);
  const refresh = () => pushState().then(setSt);
  useEffect(() => { refresh(); }, []);
  const toggle = async (on) => {
    try { if (on) await enablePush(); else await disablePush(); } catch (e) { toast(e.message); }
    refresh();
  };
  const test = async () => {
    try {
      const r = await post('/push/test');
      toast(r.delivered ? 'Sent. Check your notifications.' : 'No device is set up for notifications yet.');
    } catch (e) { toast(e.message); }
  };
  const note = st === 'needs_install' ? 'On iPhone, add Iris to your Home Screen first (Share, then Add to Home Screen), then open it from there.'
    : st === 'denied' ? 'Notifications are blocked for Iris. Allow them in your phone settings.'
    : st === 'unsupported' ? "This browser can't show notifications." : null;
  return html`<${Group} label="Notifications" note=${note}>
    <${Row} icon="bell" title=${`Messages from ${agentName()}`} sub="On this device"
      right=${html`<${Toggle} on=${st === 'on'} disabled=${!st || ['needs_install', 'denied', 'unsupported'].includes(st)} onChange=${toggle} label="Notifications" />`} />
    <${QuietHours} />
    ${st === 'on' && html`<${Row} icon="send" title="Send a test notification" onClick=${test} chevron=${false} />`}
  <//>`;
}

export function Settings() {
  useStore();
  const owner = isOwner();
  const file = useRef();
  const [version, setVersion] = useState('');
  const [counts, setCounts] = useState({});
  useEffect(() => {
    get('/health').then((h) => setVersion(h.version)).catch(() => {});
    get('/devices').then((r) => setCounts((c) => ({ ...c, devices: r.devices.filter((d) => !d.revoked_at).length }))).catch(() => {});
    if (owner) get('/users').then((r) => setCounts((c) => ({ ...c, people: r.users.length }))).catch(() => {});
    if (owner && !S.slots) loadSlots().catch(() => {});
  }, []);
  const rename = async () => {
    const name = await promptSheet({ title: `Rename ${agentName()}`, value: agentName(), yes: 'Save' });
    if (name == null || !name.trim()) return;
    try { set({ settings: await put('/settings', { agent_name: name.trim() }) }); } catch (e) { toast(e.message); }
  };
  const renameMe = async () => {
    const name = await promptSheet({ title: 'Your name', body: `What should ${agentName()} call you?`, value: S.me.user.display_name });
    if (name == null || !name.trim()) return;
    try { await put('/me', { display_name: name.trim() }); set({ me: { ...S.me, user: { ...S.me.user, display_name: name.trim() } } }); } catch (e) { toast(e.message); }
  };
  const picture = () => openSheet(() => html`<div class="sheet-body"><h3>Picture</h3>
    <div class="grp">
      <div class="r tap" onClick=${() => { closeSheet(); file.current.click(); }}><div class="rt">Choose a photo</div></div>
      ${S.settings?.avatar_custom && html`<div class="r tap danger" onClick=${async () => { closeSheet(); try { set({ settings: await del('/settings/avatar') }); } catch (e) { toast(e.message); } }}><div class="rt">Use the default</div></div>`}
    </div></div>`);
  const upload = async (e) => {
    const f = e.target.files[0];
    e.target.value = '';
    if (!f) return;
    try { set({ settings: await api('PUT', '/settings/avatar', f) }); toast('Picture updated'); } catch (err) { toast(err.message); }
  };
  const chat = S.slots?.find((s) => s.slot === 'chat');
  const engineSub = !S.slots ? ' ' : chat?.selection?.mode === 'pinned' ? `Chat pinned to ${chat.models.find((m) => m.id === chat.effective)?.label || 'a model'}` : 'Automatic';
  const signOut = async () => {
    if (!(await confirmSheet({ title: 'Forget this device?', body: "You'll need a new pairing code to use it again.", yes: 'Forget', danger: true }))) return;
    try { await del(`/devices/${seg(S.me.device.id)}`); } catch { /* already gone */ }
    setToken('');
    location.reload();
  };
  return html`<${Page} title="Settings" sub=${`${agentName()} on your home server · ${S.online ? 'online' : 'offline'}`}>
    <input ref=${file} type="file" accept="image/*" hidden onChange=${upload} />
    <${Group} label=${agentName()}>
      <${Row} icon="edit" title="Name" value=${agentName()} onClick=${owner ? rename : undefined} chevron=${owner} />
      <${Row} icon="user" title="Picture" value=${S.settings?.avatar_custom ? 'Custom' : 'Default'} onClick=${owner ? picture : undefined} chevron=${owner} />
      <${Row} icon="volume" title="Voice replies" sub="Coming soon: replies as voice notes plus text"
        right=${html`<${Toggle} on=${false} disabled onChange=${() => {}} label="Voice replies" />`} />
    <//>
    <${Group} label="You">
      <${Row} icon="user" title="Your name" value=${S.me?.user?.display_name} onClick=${renameMe} />
      ${isOwner() && html`<${Row} icon="sparkles" title="For you" sub="Sources and daily posts" onClick=${() => go('settings/feed')} />`}
    <//>
    <${Notifications} />
    <${Group} label="Advanced">
      ${owner && html`<${Row} icon="cpu" title="AI engine" sub=${engineSub} onClick=${() => go('settings/ai')} />`}
      <${Row} icon="phone" title="Devices" value=${counts.devices ?? ''} onClick=${() => go('settings/devices')} />
      ${owner && html`<${Row} icon="users" title="People" value=${counts.people ?? ''} onClick=${() => go('settings/people')} />`}
      <${Row} icon="list" title="Activity log" onClick=${() => go('settings/activity')} />
    <//>
    <${Group}>
      <${Row} title="Forget this device" danger chevron=${false} onClick=${signOut} />
    <//>
    <div class="foot mu">Iris ${version} · self-hosted, no telemetry</div>
  <//>`;
}

// --------------------------------------------------------------- AI engine

const SLOT_LABEL = {
  chat: ['Chat', 'Everyday conversations'],
  chat_strong: ['Hard requests', 'When a message clearly needs more thought'],
  background: ['Background', 'Check-ins and other scheduled work'],
  feed: ['Feed', 'Finding and summarizing your For you posts'],
};
const PROVIDER = (m) => (m.local ? 'Local' : { claude: 'Claude', codex: 'OpenAI', gemini: 'Google' }[m.harness] || m.provider || 'Open');
const COST = { free: 'free', low: 'low cost', medium: 'medium cost', high: 'higher cost' };

function Slot({ slot }) {
  const [label, desc] = SLOT_LABEL[slot.slot];
  const pinned = slot.selection.mode === 'pinned' ? slot.selection : null;
  const choose = async (body) => {
    try { set({ slots: (await put(`/models/slots/${seg(slot.slot)}`, body)).slots }); } catch (e) { toast(e.message); }
  };
  const pinnedModel = pinned && slot.models.find((m) => m.id === pinned.model);
  return html`<div class="gl">${label}</div>
    <div class="grp">
      <div class="r tap" onClick=${() => choose({ mode: 'automatic' })}>
        <div class="rt">Automatic<small>${slot.recommended_label ? `recommended: ${slot.recommended_label}` : 'Nothing available'}</small></div>
        <div class=${'rad' + (!pinned ? ' on' : '')} /></div>
      ${slot.models.map((m) => html`<div class=${'r tap' + (m.allowed === false ? ' dim' : '')} onClick=${() => choose({ mode: 'pinned', model: m.id })}>
        <div class="rt"><span><b>${m.label}</b><span class="bdg">${PROVIDER(m)}</span>${m.recommended && html`<span class="bdg ac">best</span>`}</span>
          <small>${[COST[m.cost], m.local && 'runs on your network', m.images && 'sees photos'].filter(Boolean).join(' · ') || ' '}</small></div>
        <div class=${'rad' + (pinned?.model === m.id ? ' on' : '')} /></div>`)}
    </div>
    ${pinnedModel?.efforts?.length > 0 && html`<div class="effort"><span class="mu">Effort</span>
      ${[null, ...pinnedModel.efforts].map((e) => html`<button class=${(pinned.effort || null) === e ? 'on' : ''}
        onClick=${() => choose({ mode: 'pinned', model: pinned.model, ...(e ? { effort: e } : {}) })}>${e || 'auto'}</button>`)}</div>`}
    <div class="gnote">${desc}.${slot.pin_unavailable ? ' The pinned model isn’t available right now, so the recommendation is used.' : ''}
      ${slot.status === 'no_model' ? ' No model is available for this yet.' : ''}</div>`;
}

function EngineRow({ e, h }) {
  const op = e?.operation;
  const installing = op?.state === 'running';
  const act = async (path, what) => {
    try { await post(path); loadEngines(); toast(`${what} started`); } catch (err) { toast(err.message); }
  };
  const signIn = () => openSheet(() => html`<${SignInSheet} h=${h} label=${e?.label || h.label} />`);
  let right;
  if (installing) right = html`<span class="rv shimmer">${op.op === 'uninstall' ? 'Removing…' : 'Installing…'}</span>`;
  else if (!h?.installed && !e?.installed) {
    right = e?.requires ? html`<span class="rv">Needs Node.js</span>` : html`<button class="cn" onClick=${() => act(`/engines/${seg(e.name)}/install`, 'Install')}>Set up</button>`;
  } else if (h?.auth === 'auth_required') right = html`<button class="cn" onClick=${signIn}>Reconnect</button>`;
  else if (h?.auth === 'ok') right = html`<span class="ok">● signed in</span>`;
  else right = html`<button class="cn" onClick=${signIn}>Check sign-in</button>`;
  const sub = [e?.version || h?.version, e?.managed ? 'installed by Iris' : e?.external ? 'installed on the server' : null,
    op?.state === 'failed' ? `last ${op.op} failed: ${op.error}` : null].filter(Boolean).join(' · ');
  return html`<${Row} icon=${html`<${LetterTile} name=${e?.name || h?.name} />`} title=${e?.label || h?.label} sub=${sub || ' '} right=${right} chevron=${false}
    onClick=${e?.managed && !installing ? () => openSheet(() => html`<div class="sheet-body"><h3>${e.label}</h3>
      <p class="mu">${e.source}</p><div class="grp">
      <div class="r tap" onClick=${() => { closeSheet(); act(`/engines/${seg(e.name)}/update`, 'Update'); }}><div class="rt">Update now</div></div>
      <div class="r tap danger" onClick=${async () => { closeSheet(); if (await confirmSheet({ title: `Remove ${e.label}?`, yes: 'Remove', danger: true })) { try { await del(`/engines/${seg(e.name)}`); loadEngines(); } catch (er) { toast(er.message); } } }}><div class="rt">Remove</div></div>
      </div></div>`) : undefined} />`;
}

function SignInSheet({ h, label }) {
  useStore();
  const live = S.harnesses.find((x) => x.name === h.name) || h;
  return html`<div class="sheet-body"><h3>Sign in to ${label}</h3>
    <p class="mu">This runs ${label}'s own sign-in on your server. I never see your password.</p>
    ${live.auth === 'ok' ? html`<div class="ok-line"><${Icon} name="check" size=${16} /> Signed in</div>`
      : html`<${SignIn} harness=${h.name} flow=${h.login_flow} instructions=${h.login_hint} />`}
  </div>`;
}

export function AiEngine() {
  useStore();
  useEffect(() => {
    loadSlots().catch((e) => toast(e.message));
    loadEngines().catch(() => {});
    get('/harnesses').then((r) => set({ harnesses: r.harnesses })).catch(() => {});
  }, []);
  const slots = (S.slots || []).filter((s) => SLOT_LABEL[s.slot]);
  const chat = slots.find((s) => s.slot === 'chat');
  const bg = slots.filter((s) => ['background', 'feed'].includes(s.slot));
  const claudeBg = bg.some((s) => String(s.effective || '').startsWith('claude:'));
  const engines = S.engines?.engines || [];
  const names = [...new Set([...engines.map((e) => e.name), ...S.harnesses.map((h) => h.name)])];
  const allow = async (on) => {
    try { set({ settings: await put('/settings', { allow_claude_for_shared: on }) }); loadSlots(); } catch (e) { toast(e.message); }
  };
  return html`<${Page} title="AI engine" sub="Only models installed on your server are shown. Automatic picks the best one for each job.">
    ${!S.slots ? html`<${Spinner} />` : slots.map((s) => html`<${Slot} slot=${s} key=${s.slot} />`)}
    ${claudeBg && html`<div class="warn">Claude plans are meant for personal use. Running background jobs on them can use up your limits faster.</div>`}
    ${chat?.shared && html`<${Group} label="Shared chats" note=${chat.shared.paused ? 'Shared chats are paused because Claude is the only engine available.' : `Shared chats use ${chat.models.find((m) => m.id === chat.shared.effective)?.label || 'the next allowed model'}.`}>
      <${Row} title="Allow Claude in shared chats" sub="Claude subscriptions are for one person"
        right=${html`<${Toggle} on=${!!S.settings?.allow_claude_for_shared} onChange=${allow} label="Allow Claude in shared chats" />`} />
    <//>`}
    <${Group} label="Engines on this server">
      ${names.map((n) => html`<${EngineRow} e=${engines.find((e) => e.name === n)} h=${S.harnesses.find((h) => h.name === n)} />`)}
    <//>
    ${S.engines && html`<div class="gnote">${S.engines.auto_update ? 'Engines installed by Iris update themselves once a day.' : 'Automatic updates are off.'}</div>`}
  <//>`;
}

// ----------------------------------------------------------------- devices

export function Devices() {
  const [data, setData] = useState(null);
  const load = () => get('/devices').then(setData).catch((e) => toast(e.message));
  useEffect(() => { load(); }, []);
  const revoke = async (d) => {
    const me = d.id === data.current_device_id;
    if (!(await confirmSheet({ title: `Remove ${d.name}?`, body: me ? 'This is the device you are using. You will need a new pairing code.' : 'It will be signed out right away.', yes: 'Remove', danger: true }))) return;
    try {
      await del(`/devices/${seg(d.id)}`);
      if (me) { setToken(''); location.reload(); } else load();
    } catch (e) { toast(e.message); }
  };
  const list = (data?.devices || []).filter((d) => !d.revoked_at);
  return html`<${Page} title="Devices" sub="Phones and computers paired with your Iris">
    ${!data ? html`<${Spinner} />` : html`<${Group}>${list.map((d) => html`<${Row} icon="phone" title=${d.name}
      sub=${d.id === data.current_device_id ? 'This device' : d.last_seen_at ? `Last used ${since(d.last_seen_at)}` : `Paired ${since(d.created_at)}`}
      chevron=${false} right=${html`<button class="cn danger" onClick=${() => revoke(d)}>Remove</button>`} />`)}<//>`}
    <div class="gnote">To add a device, run <code>iris pair</code> on the server${isOwner() ? ' or create a code for a person under People' : ''}.</div>
  <//>`;
}

// ------------------------------------------------------------------ people

function CodeSheet({ name, code, expires }) {
  return html`<div class="sheet-body"><h3>Pairing code for ${name}</h3>
    <p class="mu">On their phone, open this Iris address and enter the code. It works once, for ${Math.max(1, Math.round((expires - Date.now()) / 60000))} minutes.</p>
    <button class="big-code wide" onClick=${() => navigator.clipboard?.writeText(code).then(() => toast('Code copied'))}>${code} <${Icon} name="copy" size=${16} /></button>
    <div class="btns"><button class="b1" onClick=${closeSheet}>Done</button></div></div>`;
}

export function People() {
  const [users, setUsers] = useState(null);
  const load = () => get('/users').then((r) => setUsers(r.users)).catch((e) => toast(e.message));
  useEffect(() => { load(); }, []);
  const code = async (u) => {
    try {
      const r = await post(`/users/${seg(u.id)}/pairing-code`);
      openSheet(() => html`<${CodeSheet} name=${u.display_name} code=${r.code} expires=${r.expires_at} />`);
    } catch (e) { toast(e.message); }
  };
  const add = async () => {
    const name = await promptSheet({ title: 'Add a person', body: `They get their own chats with ${agentName()}, and you can chat together. Your memory, files, and connections stay yours.`, placeholder: 'Their name', yes: 'Add' });
    if (!name?.trim()) return;
    try { const r = await post('/users', { display_name: name.trim() }); await load(); code(r.user); } catch (e) { toast(e.message); }
  };
  const together = async (u) => {
    try {
      const r = await post('/conversations', { kind: 'group', title: `You and ${u.display_name}`, member_ids: [u.id] });
      go(`chat/${r.conversation.id}`);
    } catch (e) { toast(e.message); }
  };
  return html`<${Page} title="People" sub=${`Everyone with their own ${agentName()} on this server`}>
    ${!users ? html`<${Spinner} />` : html`<${Group}>${users.map((u) => html`<${Row} icon=${html`<${LetterTile} name=${u.display_name} radius=${15} />`}
      title=${u.display_name} sub=${u.is_owner ? 'Owner' : `Added ${since(u.created_at)}`} chevron=${false}
      right=${!u.is_owner && html`<span class="row-btns"><button class="cn" onClick=${() => together(u)}>Chat together</button>
        <button class="cn" onClick=${() => code(u)}>Pair</button></span>`} />`)}
      <${Row} icon="plus" title="Add a person" accent onClick=${add} chevron=${false} /><//>`}
  <//>`;
}

// ------------------------------------------------------------ activity log

export function ActivityLog() {
  const [items, setItems] = useState(null);
  const [more, setMore] = useState(false);
  const load = async (before) => {
    const r = await get(`/activity?limit=50${before ? `&before=${before}` : ''}`);
    setItems((cur) => (before ? [...(cur || []), ...r.items] : r.items));
    setMore(r.has_more);
  };
  useEffect(() => { load().catch((e) => toast(e.message)); }, []);
  const open = (it) => openSheet(() => html`<div class="sheet-body"><h3>${activityTitle(it)}</h3>
    <p class="mu">${placeLabel(it)} · ${stamp(it.created_at)}</p>
    <div class="rcpt-steps static">${(it.activity || []).map((a) => html`<div><b>${a.name}</b> ${a.status === 'failed' ? '(failed)' : ''}
      ${a.summary && html`<div class="mono small mu">${a.summary}</div>`}</div>`)}</div>
    ${it.conversation_kind !== 'job' && html`<div class="btns"><button class="b1" onClick=${() => { closeSheet(); go(`chat/${it.conversation_id}`); }}>Open chat</button></div>`}
  </div>`);
  return html`<${Page} title="Activity log" sub=${`Every step ${agentName()} took with tools, newest first`}>
    ${!items ? html`<${Spinner} />` : items.length ? html`<${Group}>${items.map((it) => html`<${Row} title=${activityTitle(it)}
      sub=${`${placeLabel(it)} · ${stamp(it.created_at)}`} onClick=${() => open(it)} />`)}<//>
      ${more && html`<button class="b2 wide" onClick=${() => load(items.at(-1).created_at)}>Show more</button>`}`
      : html`<${Empty} icon="list" title="Nothing yet">When I search, read, or use a connected service, it shows up here.<//>`}
  <//>`;
}
