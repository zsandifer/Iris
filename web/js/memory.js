// Memory: the Iris hub screen (search, topics, pinned first, recent changes
// marked), the note sheet (edit, where I learned it, history with Undo, Pin,
// Forget), the "Noted: …" line under a reply, the morning-note card, and
// Settings → Memory. Owner only: memory never shows to anyone else.
import { html, useEffect, useLayoutEffect, useRef, useState } from './h.js';
import { api, get, post, patch, put, del, seg } from './api.js';
import { S, set, useStore, go, toast, isOwner, loadMemory, memoryReset } from './store.js';
import { Icon, Row, Group, Page, Empty, Spinner, Toggle, openSheet, closeSheet, confirmSheet } from './ui.js';
import {
  TOPICS, TOPIC_TITLE, noteMark, groupNotes, countLabel, historyLines, shortDate, changeTag, notedLine, sourceOf, undos, reconcileDraft,
} from './memory-text.js';

const apiPath = (p) => String(p || '').replace(/^\/api(?=\/)/, '');
const num = (n) => seg(String(Number(n)));

// ------------------------------------------------------------ undo / redo

const flips = new Set();
const flipped = () => flips.forEach((f) => f());

function useFlips() {
  const [, tick] = useState(0);
  useEffect(() => {
    const f = () => tick((x) => x + 1);
    flips.add(f);
    return () => flips.delete(f);
  }, []);
}

const undoChange = (id) => post(`/memory/history/${num(id)}/undo`, {});

/**
 * Undo `historyId`, or redo it if it was undone here. `first` makes the
 * first undo (the morning note has its own route); later steps undo the
 * newest change through the general history route.
 */
async function toggleUndo(historyId, first = undoChange) {
  try {
    await undos.toggle(historyId, first, undoChange);
    flipped();
  } catch (e) { toast(e.message); }
}

// ------------------------------------------------------------ note sheet

/** A sheet showing memory: closed when everything is forgotten. */
function memorySheet(render) {
  render.memory = true;
  openSheet(render);
}

/** Open the sheet for one note (from the list, a Noted line, or a card).
 * `draft`: unsaved text to show instead of the saved text. */
export function openNote(id, draft = null) {
  memorySheet(() => html`<${NoteSheet} id=${id} draft=${draft} key=${'note-' + id} />`);
}

function Grow({ value, onInput, label }) {
  const ref = useRef();
  useLayoutEffect(() => {
    const el = ref.current;
    if (!el) return;
    el.style.height = 'auto';
    el.style.height = Math.min(el.scrollHeight + 2, 240) + 'px';
  }, [value]);
  return html`<textarea ref=${ref} class="field mem-ed" rows="1" value=${value} aria-label=${label}
    onInput=${(e) => onInput(e.target.value)} />`;
}

function NoteSheet({ id: first, draft }) {
  useStore();
  const [id, setId] = useState(first);
  const pending = useRef(draft);
  const [data, setData] = useState(null);
  const [err, setErr] = useState(null);
  const [text, setText] = useState('');
  const [busy, setBusy] = useState(false);
  const [moved, setMoved] = useState(false); // changed elsewhere while editing
  // Responses apply to the latest typing and the stored text it started
  // from (refs, not this render's values); only the newest request applies.
  const typed = useRef('');
  const base = useRef(null);
  const asked = useRef(0);
  const edit = (v) => { typed.current = v; setText(v); };
  // The owner's own Save or Undo is the new stored text, so the refresh its
  // change event triggers sees no draft and nothing "changed underneath".
  const settle = (t) => { base.current = t; edit(t); setMoved(false); };
  // `keepDraft`: a refresh (Pin, a change elsewhere) keeps unsaved text.
  const load = (which = id, keepDraft = false) => {
    const n = ++asked.current;
    return get(`/memory/${num(which)}`)
      .then((r) => {
        if (n !== asked.current) return;
        const d = pending.current;
        pending.current = null;
        const out = d != null ? reconcileDraft({ draft: d, saved: r.item.text, fresh: r.item.text })
          : reconcileDraft({ draft: typed.current, saved: keepDraft ? base.current : null, fresh: r.item.text });
        base.current = r.item.text;
        if (out.moved) setMoved(true);
        setData(r);
        edit(out.text);
        setErr(null);
      })
      .catch((e) => { if (n === asked.current) setErr(e.status === 404 ? "I can't find that note anymore. It may have been forgotten." : e.message); });
  };
  useEffect(() => { setMoved(false); load(id); }, [id]);
  // Changed elsewhere (another device, a chat, overnight): show the newest
  // version, keeping an unsaved draft.
  const rev = S.memoryRev;
  const seenRev = useRef(rev);
  useEffect(() => {
    if (seenRev.current === rev) return;
    seenRev.current = rev;
    load(id, true);
  }, [rev]);
  if (err) return html`<div class="sheet-body"><h3>Memory</h3><p class="mu">${err}</p>
    <div class="btns"><button class="b2" onClick=${closeSheet}>Close</button></div></div>`;
  if (!data) return html`<div class="sheet-body"><${Spinner} /></div>`;
  const { item, history } = data;
  const current = !item.archived && !item.superseded_by && !(item.valid_to && item.valid_to <= Date.now());
  const dirty = text.trim() && text.trim() !== item.text.trim();
  const src = sourceOf(item);
  const lines = historyLines(history);
  const act = async (fn) => {
    setBusy(true);
    try { await fn(); } catch (e) { toast(e.message); }
    setBusy(false);
  };
  const save = () => act(async () => {
    const r = await patch(`/memory/${num(item.id)}`, { text: text.trim() });
    if (r.duplicate) { toast('I already remember that.'); edit(item.text); return; }
    toast('Saved. I’ll remember it this way.');
    if (r.item) settle(r.item.text);
    await load(r.item?.id ?? item.id);
  });
  const pin = () => act(async () => {
    await patch(`/memory/${num(item.id)}`, { pinned: !item.pinned });
    toast(item.pinned ? 'Unpinned' : 'Pinned. I’ll always keep this in mind.');
    await load(item.id, true);
  });
  const forget = () => act(async () => {
    const ok = await confirmSheet({
      title: 'Forget this?',
      body: 'I’ll stop remembering it and it leaves this list. You can undo right after.',
      yes: 'Forget',
      danger: true,
    });
    if (!ok) { openNote(item.id, dirty ? text : null); return; }
    const r = await del(`/memory/${num(item.id)}`);
    closeSheet();
    toast('Forgotten', {
      label: 'Undo',
      run: () => post(`/memory/history/${num(r.history_id)}/undo`, {})
        .then((u) => { toast('Remembered again'); if (u.item) openNote(u.item.id); })
        .catch((e) => toast(e.message)),
    });
  });
  const undo = (h) => act(async () => {
    const r = await post(`/memory/history/${num(h)}/undo`, {});
    if (!r.item) {
      closeSheet();
      toast('Undone. I no longer remember that.', {
        label: 'Redo',
        run: () => post(`/memory/history/${num(r.history_id)}/undo`, {}).then(() => toast('Back again')).catch((e) => toast(e.message)),
      });
      return;
    }
    toast('Undone');
    settle(r.item.text);
    if (r.item.id !== id) setId(r.item.id);
    else await load(id);
  });
  const openChat = () => { closeSheet(); go(`chat/${src.cid}`); };
  return html`<div class="sheet-body mem-sheet">
    <h3>${TOPIC_TITLE[item.topic] || 'Memory'}${item.pinned && html` <span class="mem-pin">· always in mind</span>`}</h3>
    ${!current && html`<div class="lock"><${Icon} name="archive" size=${15} /> I've set this aside, so I don't use it any more. Undo the last change to bring it back.</div>`}
    <b class="mem-h">What I remember</b>
    ${moved && dirty && html`<div class="lock">This note changed while you were editing. It now says: “${item.text}”. Saving replaces that with your version.</div>`}
    ${current ? html`<${Grow} value=${text} onInput=${edit} label="What I remember" />`
      : html`<div class="mem-ed ro">${item.text}</div>`}
    <b class="mem-h">Where I learned it</b>
    <div class="mem-hist">${src.cid
      ? html`<button class="mem-src" onClick=${openChat}>${src.text} <span aria-hidden="true">›</span></button>`
      : html`<div>${src.text}</div>`}</div>
    ${lines.length > 0 && html`<b class="mem-h">History</b>
      <div class="mem-hist">${lines.map((l) => html`<div class="mem-hl" key=${l.id}>
        <span>${shortDate(l.at)} · ${l.text}</span>
        ${l.undo && html`<button class="mem-undo" disabled=${busy} onClick=${() => undo(l.id)}>Undo</button>`}</div>`)}</div>`}
    ${current && html`<div class="mem-acts">
      <button class="pri" disabled=${busy || !dirty} onClick=${save}>Save</button>
      <button disabled=${busy} onClick=${pin}>${item.pinned ? 'Unpin' : 'Pin'}</button>
      <button class="del" disabled=${busy} onClick=${forget}>Forget</button></div>
      <div class="gnote">${item.pinned ? 'Pinned notes are always in mind, in every private chat.' : 'Pin it to keep it always in mind.'}</div>`}
  </div>`;
}

// ------------------------------------------------------------ Memory screen

function NoteRow({ item }) {
  const mark = noteMark(item);
  return html`<button class="mem-row" onClick=${() => openNote(item.id)}>
    <span class="grow"><span class="mem-tx">${item.text}</span>
      ${(mark.text || item.pinned) && html`<small class=${'mem-mark' + (mark.recent ? ' recent' : '')}>${item.pinned && html`<span class="mem-pin">Always in mind</span>${mark.text ? ' · ' : ''}`}${mark.text}</small>`}</span>
    <${Icon} name="chev" size=${16} cls="rc" /></button>`;
}

function Groups({ items, topic }) {
  return groupNotes(items, topic).map((g) => html`<div class="gl" key=${'l' + g.key}>${g.label}</div>
    <div class="grp mem-grp" key=${'g' + g.key}>${g.items.map((i) => html`<${NoteRow} item=${i} key=${i.id} />`)}</div>`);
}

function LearningPaused() {
  if (!S.settings?.memory?.learning_paused) return null;
  const resume = async () => {
    try { set({ settings: await put('/settings', { memory_learning_paused: false }) }); toast('I’m learning again'); } catch (e) { toast(e.message); }
  };
  return html`<div class="lock mem-paused"><${Icon} name="pause" size=${15} /> Learning is paused. I still use what I know, but I’m not saving anything new.
    <button class="lk-m" onClick=${resume}>Resume learning</button></div>`;
}

export function MemoryScreen() {
  useStore();
  const [topic, setTopic] = useState(null);
  const [q, setQ] = useState('');
  const [found, setFound] = useState(null); // {query, topic, items, more} or with error
  const [err, setErr] = useState(null);
  const asked = useRef(0);
  useEffect(() => { loadMemory().catch((e) => setErr(e.message)); }, []);
  const query = q.trim();
  // Search with the same keyword search I use; again after any change.
  const search = (offset = 0, prev = []) => {
    const n = ++asked.current;
    const q = `/memory?limit=100&offset=${offset}&query=${encodeURIComponent(query)}${topic ? `&topic=${topic}` : ''}`;
    return get(q)
      .then((r) => {
        if (n !== asked.current) return;
        const ids = new Set(prev.map((i) => i.id));
        setFound({ query, topic, items: [...prev, ...r.items.filter((i) => !ids.has(i.id))], more: !!r.has_more, offset: offset + r.items.length });
      })
      .catch((e) => {
        if (n !== asked.current) return;
        if (prev.length) toast(e.message);
        setFound({ query, topic, items: prev, error: e.message, more: prev.length > 0, offset });
      });
  };
  useEffect(() => {
    if (!query) { setFound(null); asked.current++; return undefined; }
    const t = setTimeout(() => search(), 250);
    return () => clearTimeout(t);
  }, [query, topic, S.memory]);
  const all = S.memory?.items;
  const searching = !!query;
  const fresh = found && found.query === query && found.topic === topic;
  const shown = searching ? (fresh ? found.items : null) : all;
  const label = TOPICS.find(([k]) => k === topic)?.[1];
  let body;
  if (err && !all) body = html`<${Empty} icon="memory" title="I can't open my notes right now">${err}<//>`;
  else if (!shown) body = html`<${Spinner} />`;
  else if (searching) {
    body = shown.length ? html`<div class="gl">Results</div><div class="grp mem-grp">${shown.map((i) => html`<${NoteRow} item=${i} key=${i.id} />`)}</div>
      ${found.more && html`<button class="b2 wide" onClick=${() => search(found.offset, found.items)}>Show more</button>`}`
      : html`<${Empty} icon="search" title="Nothing found">${found?.error || `I don't remember anything about “${query}”${label ? ` under ${label}` : ''}.`}<//>`;
  } else {
    const groups = groupNotes(shown, topic);
    body = groups.length ? html`<${Groups} items=${shown} topic=${topic} />`
      : html`<${Empty} icon="memory" title=${topic ? `Nothing under ${label} yet` : 'Nothing yet'}>
          As we talk, I’ll note what matters: people, plans, routines, and how you like things done. You’ll see it all here.<//>`;
  }
  return html`<${Page} title="Memory" sub=${`What I know about you${all?.length ? ` · ${countLabel(all.length, S.memory.more)}` : ''}`}
    right=${html`<button class="btn" aria-label="Memory settings" onClick=${() => go('settings/memory')}><${Icon} name="sliders" size=${19} /></button>`}>
    <${LearningPaused} />
    <label class="srch mem-search"><${Icon} name="search" size=${17} />
      <input type="search" value=${q} placeholder="Search my memory" aria-label="Search my memory" enterkeyhint="search"
        onInput=${(e) => setQ(e.target.value)} />
      ${q && html`<button class="icon-btn" aria-label="Clear search" onClick=${() => setQ('')}><${Icon} name="x" size=${15} /></button>`}</label>
    <div class="seg mem-seg" role="tablist">${[[null, 'All'], ...TOPICS].map(([k, l]) => html`<button role="tab"
      aria-selected=${topic === k ? 'true' : 'false'} class=${topic === k ? 'on' : ''} onClick=${() => setTopic(k)}>${l}</button>`)}</div>
    ${body}
  <//>`;
}

// ------------------------------------------------------------ changes list

function ChangeRow({ ch, first, edit = false }) {
  useFlips();
  const off = undos.isOff(ch.history_id);
  const tag = changeTag(ch.op, { noted: edit });
  const gone = ch.op === 'delete' || (off && ch.op === 'add');
  return html`<div class=${'mem-li' + (off ? ' undone' : '')}>
    ${tag && html`<span class=${'mem-tag ' + tag[1]}>${tag[0]}</span>`}
    <span class="grow"><span class="mem-tx">${ch.text}</span>${ch.was && html`<small class="mem-was">was: ${ch.was}</small>`}</span>
    ${edit && !gone && !off && ch.item_id && html`<button class="mem-undo mu" onClick=${() => openNote(ch.item_id)}>Edit</button>`}
    <button class="mem-undo" onClick=${() => toggleUndo(ch.history_id, first)}>${off ? 'Redo' : 'Undo'}</button>
  </div>`;
}

// ------------------------------------------------------------ Noted line

function NotedSheet({ changes }) {
  return html`<div class="sheet-body"><h3>What I noted</h3>
    <p class="mu">From this reply. Undo anything I got wrong, or edit it.</p>
    <div class="grp mem-grp">${changes.map((ch) => html`<${ChangeRow} ch=${ch} edit key=${ch.history_id} />`)}</div>
    <div class="btns"><button class="b2" onClick=${() => { closeSheet(); go('iris/memory'); }}>See everything I remember</button></div>
  </div>`;
}

/** "Noted: …" under a reply that saved, changed or forgot something. */
export function NotedLine({ m }) {
  useFlips();
  const changes = m.meta?.noted;
  if (!isOwner() || !Array.isArray(changes) || !changes.length) return null;
  const allOff = changes.every((ch) => undos.isOff(ch.history_id));
  return html`<button class=${'noted' + (allOff ? ' undone' : '')} onClick=${() => memorySheet(() => html`<${NotedSheet} changes=${changes} />`)}>
    <${Icon} name="memory" size=${13} /><span>${notedLine(changes)}</span></button>`;
}

// ------------------------------------------------------------ morning note

/** The note after I tidied my memory overnight (server card `memory_morning`). */
export function MorningCard({ c }) {
  const [all, setAll] = useState(false);
  const preview = c.changes || [];
  const list = all ? (c.all_changes || preview) : preview;
  const more = c.more_count || Math.max(0, (c.all_changes?.length || 0) - preview.length);
  const first = c.undo_url ? (id) => post(apiPath(c.undo_url), { history_id: id }) : undefined;
  return html`<div class="card-x mem-card"><b>${c.title || 'I tidied my notes overnight'}</b>
    ${!isOwner() ? null : html`<p>From our chats yesterday. Tap Undo if I got something wrong.</p>
    ${list.map((ch) => html`<${ChangeRow} ch=${ch} first=${first} key=${ch.history_id} />`)}
    ${!all && more > 0 && html`<button class="lnk" onClick=${() => setAll(true)}>${c.more_label || `and ${more} more ›`}</button>`}
    <button class="lnk" onClick=${() => go('iris/memory')}>${c.action_label || 'See everything I remember ›'}</button>`}
  </div>`;
}

// ------------------------------------------------------------ settings

function ExportSheet() {
  const [url, setUrl] = useState(null);
  const [err, setErr] = useState(null);
  useEffect(() => {
    let live = true;
    let made = null;
    api('GET', '/memory/export', undefined, { as: 'blob' })
      .then((b) => { if (live) { made = URL.createObjectURL(b); setUrl(made); } })
      .catch((e) => live && setErr(e.message));
    return () => { live = false; if (made) URL.revokeObjectURL(made); };
  }, []);
  return html`<div class="sheet-body"><h3>Export as Markdown</h3>
    <p class="mu">A plain-text copy of everything I remember, including notes I’ve set aside. It doesn’t include our chats.</p>
    ${err && html`<div class="err-line">${err}</div>`}
    <div class="btns"><button class="b2" onClick=${closeSheet}>Close</button>
      ${url ? html`<a class="b1" href=${url} download="iris-memory.md" onClick=${() => setTimeout(closeSheet, 300)}><${Icon} name="download" size=${17} /> Download</a>`
        : html`<button class="b1" disabled>${err ? 'Download' : 'Preparing…'}</button>`}</div>
  </div>`;
}

export function MemorySettings() {
  useStore();
  const mem = S.settings?.memory || {};
  const save = async (body) => {
    try { set({ settings: await put('/settings', body) }); } catch (e) { toast(e.message); }
  };
  const forgetAll = async () => {
    const ok = await confirmSheet({
      title: 'Forget everything?',
      body: `I’ll forget every note I’ve learned about you, and their history, so this can’t be undone. Our chats and any copies you’ve downloaded stay.`,
      yes: 'Forget everything',
      danger: true,
    });
    if (!ok) return;
    try {
      const r = await post('/memory/forget-everything', { confirm: true });
      toast(r.forgotten ? 'Done. I’ve forgotten everything I’d learned.' : 'Done. There was nothing to forget.');
      memoryReset();
    } catch (e) { toast(e.message); }
  };
  return html`<${Page} title="Memory" sub="What I remember and how I learn">
    <${Group} label="Learning">
      <${Row} icon="pause" title="Pause learning" sub="I won’t save anything new. I still use what I already know."
        right=${html`<${Toggle} on=${!!mem.learning_paused} onChange=${(on) => save({ memory_learning_paused: on })} label="Pause learning" />`} />
      <${Row} icon="sparkles" title="Morning note" sub="A short note in Main chat after I tidy my notes overnight"
        right=${html`<${Toggle} on=${mem.morning_note !== false} onChange=${(on) => save({ memory_morning_note: on })} label="Morning note" />`} />
    <//>
    <${Group} label="Your notes">
      <${Row} icon="memory" title="See everything I remember" onClick=${() => go('iris/memory')} />
      <${Row} icon="download" title="Export as Markdown" sub="Download a copy of all my notes" onClick=${() => memorySheet(() => html`<${ExportSheet} />`)} chevron=${false} />
    <//>
    <${Group} note="Removes every note and its history. Chats stay.">
      <${Row} title="Forget everything" danger chevron=${false} onClick=${forgetAll} />
    <//>
  <//>`;
}
