// Ideas, Goals, and Library tabs.
import { html, useEffect, useState } from './h.js';
import { get, post, patch, del, api, seg, isId, sessionGen } from './api.js';
import {
  S, set, useStore, go, toast, loadIdeas, loadGoals, loadLibrary, loadMoreSaved, loadMorePhotos, loadMoreItems, convFor, touchList,
} from './store.js';
import { Icon, Img, Empty, Spinner, ago, since, openSheet, closeSheet, confirmSheet, hostOf, useBlob } from './ui.js';
import { md } from './md.js';
import { Post, findPost } from './feed.js';
import { openPhoto } from './photo.js';

function Head({ title, sub }) {
  return html`<h1 class="ttl">${title}</h1>${sub && html`<div class="sub">${sub}</div>`}`;
}

// ------------------------------------------------------------------- ideas

export function IdeasScreen() {
  useStore();
  const [busy, setBusy] = useState(null);
  useEffect(() => { loadIdeas().catch((e) => toast(e.message)); }, []);
  if (!S.ideas) return html`<div class="scr tabscr"><${Spinner} /></div>`;
  const open = S.ideas.filter((i) => i.status === 'open');
  const started = S.ideas.filter((i) => i.status === 'started');
  const start = async (i) => {
    setBusy(i.id);
    try {
      const r = await post(`/ideas/${seg(i.id)}/start`);
      await loadIdeas();
      go(`chat/${r.conversation.id}`);
    } catch (e) { toast(e.message); }
    setBusy(null);
  };
  const later = async (i) => {
    touchList('ideas');
    set({ ideas: S.ideas.map((x) => (x.id === i.id ? { ...x, status: 'dismissed' } : x)) });
    try { await patch(`/ideas/${seg(i.id)}`, { status: 'dismissed' }); } catch (e) { toast(e.message); loadIdeas(); }
  };
  return html`<div class="scr tabscr">
    <${Head} title="Ideas" sub="Things I could do for you. Tap Do it and I'll get started." />
    ${open.map((i) => html`<div class="card" key=${i.id}>
      <b>${i.title}</b>${i.description && html`<p>${i.description}</p>`}
      <div class="act"><button disabled=${busy === i.id} onClick=${() => start(i)}>Do it</button>
        <button class="mu" onClick=${() => later(i)}>Not now</button></div>
    </div>`)}
    ${!open.length && html`<${Empty} icon="bulb" title="No new ideas right now">
      As I learn what you care about, I'll suggest things I can take off your plate.<//>`}
    ${started.length > 0 && html`<div class="gl">In progress</div>
      <div class="grp">${started.map((i) => html`<div class="r tap" onClick=${() => i.conversation_id && go(`chat/${i.conversation_id}`)}>
        <div class="ri"><${Icon} name="chat" size=${17} /></div><div class="rt">${i.title}<small>Started ${since(i.updated_at)}</small></div>
        <${Icon} name="chev" size=${16} cls="rc" /></div>`)}</div>`}
  </div>`;
}

// ------------------------------------------------------------------- goals

export function GoalsScreen() {
  useStore();
  const [title, setTitle] = useState('');
  const [showDone, setShowDone] = useState(false);
  useEffect(() => { loadGoals().catch((e) => toast(e.message)); }, []);
  if (!S.goals) return html`<div class="scr tabscr"><${Spinner} /></div>`;
  const suggested = S.goals.filter((g) => g.suggested && !g.done);
  const active = S.goals.filter((g) => !g.suggested && !g.done);
  const done = S.goals.filter((g) => g.done);
  const update = async (g, body) => {
    touchList('goals');
    set({ goals: S.goals.map((x) => (x.id === g.id ? { ...x, ...body } : x)) });
    try { await patch(`/goals/${seg(g.id)}`, body); } catch (e) { toast(e.message); }
    loadGoals().catch(() => {});
  };
  const remove = async (g) => {
    touchList('goals');
    set({ goals: S.goals.filter((x) => x.id !== g.id) });
    try { await del(`/goals/${seg(g.id)}`); } catch (e) { toast(e.message); loadGoals(); }
  };
  const add = async (e) => {
    e.preventDefault();
    if (!title.trim()) return;
    try { await post('/goals', { title: title.trim() }); setTitle(''); await loadGoals(); } catch (err) { toast(err.message); }
  };
  const menu = (g) => openSheet(() => html`<div class="sheet-body">
    <h3>${g.title}</h3>${g.status_text && html`<p class="mu">${g.status_text}</p>`}
    <div class="grp">
      <div class="r tap" onClick=${() => { closeSheet(); update(g, { done: !g.done }); }}><div class="rt">${g.done ? 'Mark as not done' : 'Mark as done'}</div></div>
      <div class="r tap danger" onClick=${async () => { closeSheet(); if (await confirmSheet({ title: 'Delete this goal?', yes: 'Delete', danger: true })) remove(g); }}><div class="rt">Delete goal</div></div>
    </div></div>`);
  const GoalRow = ({ g }) => html`<div class="goal">
    <button class=${'chk' + (g.done ? ' on' : '')} aria-label=${g.done ? 'Mark not done' : 'Mark done'} onClick=${() => update(g, { done: !g.done })}>
      ${g.done && html`<${Icon} name="check" size=${14} />`}</button>
    <div class="grow" onClick=${() => menu(g)}><b>${g.title}</b>${g.status_text && html`<p>${g.status_text}</p>`}</div>
  </div>`;
  return html`<div class="scr tabscr">
    <${Head} title="Goals" sub="What you're working toward. I keep the status up to date." />
    ${suggested.map((g) => html`<div class="card suggest" key=${g.id}><span class="tag">I suggest</span>
      <b>${g.title}</b>${g.status_text && html`<p>${g.status_text}</p>`}
      <div class="act"><button onClick=${() => update(g, { suggested: false })}>Add goal</button>
      <button class="mu" onClick=${() => remove(g)}>No thanks</button></div></div>`)}
    ${active.length > 0 && html`<div class="grp goals">${active.map((g) => html`<${GoalRow} g=${g} key=${g.id} />`)}</div>`}
    ${!active.length && !suggested.length && html`<${Empty} icon="target" title="No goals yet">
      Tell me what you're working toward, or add one below. I'll help you keep track.<//>`}
    <form class="add-row" onSubmit=${add}>
      <input class="field" value=${title} placeholder="Add a goal…" onInput=${(e) => setTitle(e.target.value)} />
      <button class="b1 sm" disabled=${!title.trim()}>Add</button>
    </form>
    ${done.length > 0 && html`<button class="gl tap-gl" onClick=${() => setShowDone(!showDone)}>Done (${done.length}) ${showDone ? '▾' : '›'}</button>
      ${showDone && html`<div class="grp goals">${done.map((g) => html`<${GoalRow} g=${g} key=${g.id} />`)}</div>`}`}
  </div>`;
}

// ----------------------------------------------------------------- library

const KIND_ICON = (mime) => (mime.startsWith('image/') ? 'image' : mime.includes('html') ? 'globe' : 'file');

function Download({ src, name }) {
  const [url, failed] = useBlob(src);
  if (failed) return html`<button class="b1" disabled>Download</button>`;
  return html`<a class="b1" href=${url || undefined} download=${name} aria-disabled=${url ? undefined : 'true'}>Download</a>`;
}

/** `cid` if it is a chat this user can open (not a background job's), else null. */
async function openableChat(cid) {
  if (!isId(cid)) return null;
  if (convFor(cid)) return cid;
  try {
    const { conversation: c } = await get(`/conversations/${seg(cid)}`);
    return c && c.kind !== 'job' ? cid : null;
  } catch {
    return null;
  }
}

export async function openItem(it) {
  openSheet(() => html`<div class="sheet-body"><${Spinner} /></div>`);
  const gen = sessionGen();
  const chat = openableChat(it.conversation_id);
  try {
    const path = `/library/${seg(it.id)}/content`;
    let view;
    if (it.mime.startsWith('image/')) view = html`<${Img} src=${`/api${path}`} cls="lib-full" alt=${it.title} />`;
    else if (it.mime.startsWith('text/') || it.mime === 'application/json') {
      const text = await api('GET', path, undefined, { as: 'text' });
      view = it.mime === 'text/markdown' ? html`<div class="md doc" dangerouslySetInnerHTML=${{ __html: md(text) }} />`
        : html`<pre class="doc">${text}</pre>`;
    } else view = html`<p class="mu">I can't show this kind of file here, but you can download it.</p>`;
    const cid = await chat;
    // Signed out or re-paired meanwhile: this belongs to the previous user.
    if (gen !== sessionGen()) return;
    openSheet(() => html`<div class="sheet-body tall">
      <h3>${it.title}</h3>${it.description && html`<p class="mu">${it.description}</p>`}
      ${view}
      <div class="btns">
        ${cid && html`<button class="b2" onClick=${() => { closeSheet(); go(`chat/${cid}`); }}>Open chat</button>`}
        <${Download} src=${`/api${path}`} name=${it.title} />
      </div></div>`);
  } catch (e) { closeSheet(); toast(e.message); }
}

function MadeByIris({ items, limit }) {
  const media = items.filter((i) => i.mime.startsWith('image/'));
  const docs = items.filter((i) => !i.mime.startsWith('image/'));
  const shown = limit ? docs.slice(0, limit) : docs;
  return html`${media.length > 0 && html`<div class="grid">${(limit ? media.slice(0, 6) : media).map((it) => html`
      <button class="gi" onClick=${() => openPhoto(`/api/library/${seg(it.id)}/content`, it.title, { action: { label: 'Details', onClick: () => openItem(it) } })}><${Img} src=${`/api/library/${seg(it.id)}/content`} alt=${it.title} /></button>`)}</div>`}
    ${shown.length > 0 && html`<div class="grp">${shown.map((it) => html`<div class="r tap" onClick=${() => openItem(it)}>
      <div class="ri"><${Icon} name=${KIND_ICON(it.mime)} size=${17} /></div>
      <div class="rt">${it.title}<small>${it.description || it.mime} · ${ago(it.created_at)}</small></div>
      <${Icon} name="chev" size=${16} cls="rc" /></div>`)}</div>`}`;
}

function PhotoGrid({ photos, limit }) {
  const list = limit ? photos.slice(0, limit) : photos;
  const options = (p) => ({ action: p.conversation_id ? { label: 'Open chat', onClick: () => go(`chat/${p.conversation_id}`) } : null });
  return html`<div class="grid">${list.map((p) => html`<button class="gi" onClick=${() => openPhoto(`/api/attachments/${seg(p.id)}`, 'Photo you sent', options(p))}>
    <${Img} src=${`/api/attachments/${seg(p.id)}`} alt="Photo you sent" photoOptions=${options(p)} /></button>`)}</div>`;
}

function Saved({ posts, limit }) {
  const list = limit ? posts.slice(0, limit) : posts;
  const show = (p) => {
    // Follow the post's live state (like/unlike) while the sheet is open.
    let cur = p;
    openSheet(() => {
      cur = findPost(p.id) || cur;
      return html`<div class="sheet-body tall flush"><${Post} p=${cur} /></div>`;
    });
  };
  return html`<div class="grp">${list.map((p) => html`<div class="r tap" onClick=${() => show(p)}>
    <div class="ri thumb">${(p.images?.[0] || p.image_url) ? html`<${Img} src=${p.images?.[0] || p.image_url} alt=${p.title} />` : html`<${Icon} name="heart" size=${16} />`}</div>
    <div class="rt">${p.title}<small>${p.source_name || hostOf(p.source_url) || p.topic || 'Saved post'}</small></div>
    <${Icon} name="chev" size=${16} cls="rc" /></div>`)}</div>`;
}

function ShowMore({ load }) {
  const [busy, setBusy] = useState(false);
  const more = async () => {
    setBusy(true);
    try { await load(); } catch (e) { toast(e.message); }
    setBusy(false);
  };
  return html`<button class="b2 wide" disabled=${busy} onClick=${more}>${busy ? 'Loading…' : 'Show more'}</button>`;
}

export function LibraryScreen() {
  useStore();
  const [tab, setTab] = useState('all');
  useEffect(() => { loadLibrary().catch((e) => toast(e.message)); }, []);
  const lib = S.library;
  // Unliked posts stay loaded (so an open post and the paging cursor keep
  // working) but leave the Saved views at once.
  const saved = (lib?.saved || []).filter((p) => p.liked !== false);
  const tabs = [['all', 'All'], ['made', 'Made by me'], ['photos', 'Photos'], ['saved', 'Saved']];
  let body;
  if (!lib) body = html`<${Spinner} />`;
  else if (tab === 'made') body = lib.items.length ? html`<${MadeByIris} items=${lib.items} />${lib.itemsMore && html`<${ShowMore} load=${loadMoreItems} />`}` : html`<${Empty} icon="file" title="Nothing yet">Reports, drafts, and images I make for you land here.<//>`;
  else if (tab === 'photos') body = lib.photos.length ? html`<${PhotoGrid} photos=${lib.photos} />${lib.photosMore && html`<${ShowMore} load=${loadMorePhotos} />`}` : html`<${Empty} icon="image" title="No photos yet">Photos you send me show up here.<//>`;
  else if (tab === 'saved') body = saved.length || lib.savedMore ? html`<${Saved} posts=${saved} />${lib.savedMore && html`<${ShowMore} load=${loadMoreSaved} />`}` : html`<${Empty} icon="heart" title="Nothing saved">Tap ♡ on a post in For you to keep it here.<//>`;
  else {
    const any = lib.items.length || lib.photos.length || saved.length;
    body = any ? html`
      ${lib.items.length > 0 && html`<div class="gl">Made by me</div><${MadeByIris} items=${lib.items} limit=${4} />`}
      ${lib.photos.length > 0 && html`<div class="gl">Photos you sent</div><${PhotoGrid} photos=${lib.photos} limit=${6} />`}
      ${saved.length > 0 && html`<div class="gl">Saved posts</div><${Saved} posts=${saved} limit=${4} />`}`
      : html`<${Empty} icon="library" title="Your library is empty">Things I make for you, photos you send, and posts you save all collect here.<//>`;
  }
  return html`<div class="scr tabscr">
    <${Head} title="Library" sub="What I've made, photos you sent, and posts you saved." />
    <div class="pills">${tabs.map(([k, l]) => html`<button class=${tab === k ? 'on' : ''} onClick=${() => setTab(k)}>${l}</button>`)}</div>
    ${body}
  </div>`;
}
