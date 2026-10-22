// Working notes: the agent's own picture of each domain it looks after
// (plan, state, what it is watching). Owner only. The list, one note
// (rendered Markdown, Edit, earlier versions, Delete), and the hub row's
// subtitle.
import { html, useEffect, useRef, useState } from './h.js';
import { get, put, del, seg } from './api.js';
import { S, useStore, go, toast } from './store.js';
import { Icon, Row, Group, Page, Empty, Spinner, agentName, ago, confirmSheet, promptSheet } from './ui.js';
import { md } from './md.js';

const BY = { heartbeat: 'on a check-in', agent: 'in a chat', owner: 'by you' };
const byLabel = (n) => `${ago(n.updated_at)} ${BY[n.updated_by] || ''}`.trim();

export function NotesScreen() {
  useStore();
  const [notes, setNotes] = useState(null);
  const [err, setErr] = useState(null);
  const load = () => get('/working-notes').then((r) => setNotes(r.notes)).catch((e) => setErr(e.message));
  useEffect(() => { load(); }, [S.workingNotesRev]);
  const add = async () => {
    const name = await promptSheet({ title: 'What should I look after?', body: 'One domain per note, like “Household budget” or “Kitchen remodel”. I’ll keep the plan and where things stand in it.', placeholder: 'Household budget', yes: 'Start note' });
    if (!name?.trim()) return;
    const domain = name.trim().toLowerCase().replace(/[^a-z0-9]+/g, '-').replace(/^-+|-+$/g, '').slice(0, 60).replace(/-+$/, '');
    if (!domain) { toast('Give it a name with some letters in it.'); return; }
    try {
      // Never replace a note that already exists under this name: open it.
      const current = await get('/working-notes');
      if (current.notes.some((n) => n.domain === domain)) { go(`iris/notes/${domain}`); return; }
      await put(`/working-notes/${seg(domain)}`, { title: name.trim(), body: '' });
      go(`iris/notes/${domain}?edit=1`);
    } catch (e) { toast(e.message); }
  };
  let body;
  if (err && !notes) body = html`<${Empty} icon="file" title="I can't open my notes right now">${err}<//>`;
  else if (!notes) body = html`<${Spinner} />`;
  else if (!notes.length) body = html`<${Empty} icon="file" title="No working notes yet">
    When you hand me something to look after, I keep a note for it here: the plan, where things stand, and what I'm watching. Each check-in reads and updates them.<//>`;
  else body = html`<${Group}>${notes.map((n) => html`<${Row} key=${n.domain} icon="file" title=${n.title}
      sub=${`Updated ${byLabel(n)}`} onClick=${() => go(`iris/notes/${n.domain}`)} />`)}<//>`;
  return html`<${Page} title="Working notes" sub=${`My own notes on what I look after for you`}
    right=${html`<button class="b2 sm" onClick=${add}>Add</button>`}>
    ${body}
  <//>`;
}

export function NoteScreen({ domain }) {
  useStore();
  const [data, setData] = useState(null);
  const [err, setErr] = useState(null);
  const [editing, setEditing] = useState(!!S.route.query.get('edit'));
  const [text, setText] = useState('');
  const [busy, setBusy] = useState(false);
  const [showHistory, setShowHistory] = useState(false);
  // The version the editor started from, so a note the agent rewrote
  // meanwhile is never overwritten without asking.
  const base = useRef(null);
  const editingRef = useRef(editing);
  editingRef.current = editing;
  const load = () => get(`/working-notes/${seg(domain)}`)
    .then((r) => {
      setData(r);
      if (!editingRef.current || base.current == null) { setText(r.note.body); base.current = r.note.updated_at; }
    })
    .catch((e) => setErr(e.message));
  useEffect(() => { load(); }, [domain, S.workingNotesRev]);
  const note = data?.note;
  const dirty = note && text !== note.body;
  const changedMeanwhile = editing && note && base.current != null && note.updated_at !== base.current;
  const startEditing = () => { base.current = note.updated_at; setText(note.body); setEditing(true); };
  const stopEditing = () => { base.current = note.updated_at; setText(note.body); setEditing(false); };
  const save = async () => {
    // Not busy while the conflict sheet is up: dismissing it by tapping
    // outside never resolves, and Save must stay usable.
    let latest;
    try { latest = await get(`/working-notes/${seg(domain)}`); } catch (e) { toast(e.message); return; }
    if (base.current != null && latest.note.updated_at !== base.current) {
      setData(latest);
      const ok = await confirmSheet({
        title: `${agentName()} changed this note while you were editing`,
        body: `Updated ${byLabel(latest.note)}. Replace that version with yours? Its text stays under Earlier versions.`,
        yes: 'Replace',
      });
      if (!ok) return;
    }
    setBusy(true);
    try {
      const r = await put(`/working-notes/${seg(domain)}`, { body: text });
      setData({ ...latest, note: r.note });
      base.current = r.note.updated_at;
      setEditing(false);
      toast('Saved. I’ll read it on my next check-in.');
      load();
    } catch (e) { toast(e.message); }
    setBusy(false);
  };
  const remove = async () => {
    if (!(await confirmSheet({ title: `Stop keeping a note on ${note.title}?`, body: 'I’ll forget my plan and notes for this. Your memory and goals are not affected.', yes: 'Delete', danger: true }))) return;
    try { await del(`/working-notes/${seg(domain)}`); go('iris/notes', true); } catch (e) { toast(e.message); }
  };
  if (err && !note) return html`<${Page} title="Working note"><${Empty} icon="file" title="I can't open this note">${err}<//><//>`;
  if (!note) return html`<${Page} title="Working note"><${Spinner} /><//>`;
  const history = data.history || [];
  return html`<${Page} title=${note.title} sub=${`Updated ${byLabel(note)}`}
    right=${editing
      ? html`<button class="b1 sm" disabled=${busy || !dirty} onClick=${save}>Save</button>`
      : html`<button class="b2 sm" onClick=${startEditing}>Edit</button>`}>
    ${editing
      ? html`${changedMeanwhile && html`<div class="note">${agentName()} updated this note while you were editing. Saving will ask before replacing that version.</div>`}
        <textarea class="editor" value=${text} spellcheck=${false} onInput=${(e) => setText(e.target.value)}
          placeholder=${'Plan:\nState:\nWatching:\nOpen questions:'} aria-label=${`Edit ${note.title}`} />
        ${dirty && html`<button class="lk-m" onClick=${stopEditing}>Discard changes</button>`}`
      : note.body.trim()
        ? html`<div class="mdview" dangerouslySetInnerHTML=${{ __html: md(note.body) }} />`
        : html`<${Empty} icon="file" title="Nothing written yet">${agentName()} fills this in on the next check-in, or you can write the plan yourself.<//>`}
    ${!editing && html`<${Group}>
      ${history.length > 0 && html`<${Row} icon="repeat" title=${`Earlier versions (${history.length})`} chevron=${false}
        right=${html`<span class="mu">${showHistory ? 'Hide' : 'Show'}</span>`} onClick=${() => setShowHistory(!showHistory)} />`}
      <${Row} icon="x" title="Delete this note" danger chevron=${false} onClick=${remove} />
    <//>`}
    ${!editing && showHistory && history.map((h) => html`<div class="note-ver" key=${h.id}>
      <div class="gl">${ago(h.replaced_at)} · ${BY[h.updated_by] || h.updated_by}</div>
      <div class="mdview" dangerouslySetInnerHTML=${{ __html: md(h.body || '_(empty)_') }} />
      <button class="lk-m" onClick=${() => { base.current = note.updated_at; setText(h.body); setEditing(true); }}>Restore this version</button>
    </div>`)}
  <//>`;
}

/** Hub row subtitle: how many notes, or an invitation. */
export function notesSub(notes) {
  if (!notes) return ' ';
  if (!notes.length) return 'What I look after for you';
  return `${notes.length} domain${notes.length === 1 ? '' : 's'} I look after`;
}
