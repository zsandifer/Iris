// For you: story cards per topic, Instagram-like posts, full-screen stories.
import { html, useEffect, useRef, useState } from './h.js';
import { post as apiPost } from './api.js';
import { S, set, useStore, loadFeed, go, toast, isOwner } from './store.js';
import { Icon, Img, LetterTile, Empty, Spinner, ago, hostOf, safeUrl } from './ui.js';
import { md, plain } from './md.js';

const photos = (p) => (p.images?.length ? p.images : p.image_url ? [p.image_url] : []);
const sourceName = (p) => p.source_name || hostOf(p.source_url) || 'For you';
const STORY_MS = 7000;

/** Topics with 1-5 stories each, in order of their newest post. */
export function topics(posts) {
  const map = new Map();
  for (const p of posts) {
    const t = (p.topic || '').trim();
    if (!t) continue;
    if (!map.has(t)) map.set(t, []);
    if (map.get(t).length < 5) map.get(t).push(p);
  }
  return [...map.entries()].map(([name, stories]) => ({ name, stories }));
}

async function like(p) {
  const liked = !p.liked;
  const swap = (x) => (x.id === p.id ? { ...x, liked } : x);
  set({ feed: { ...S.feed, posts: S.feed.posts.map(swap) } });
  try { await apiPost(`/feed/${p.id}/like`, { liked }); } catch (e) {
    toast(e.message);
    set({ feed: { ...S.feed, posts: S.feed.posts.map((x) => (x.id === p.id ? { ...x, liked: !liked } : x)) } });
  }
}

export async function discuss(p) {
  try {
    const r = await apiPost(`/feed/${p.id}/discuss`);
    set({ story: null });
    go(`chat/${r.conversation.id}`);
  } catch (e) { toast(e.message); }
}

const visit = (p) => safeUrl(p.source_url) && window.open(p.source_url, '_blank', 'noopener,noreferrer');

function Favicon({ p, size = 30 }) {
  const [bad, setBad] = useState(false);
  const host = hostOf(p.source_url);
  // Reserved names (RFC 2606/6761) never resolve; skip the request.
  const reserved = /(^|\.)(example|invalid|test|localhost|local)$/.test(host);
  if (!host || bad || reserved) return html`<${LetterTile} name=${sourceName(p)} size=${size} radius=${size / 2} />`;
  return html`<img class="fav" style=${`width:${size}px;height:${size}px`} alt="" referrerpolicy="no-referrer"
    src=${`https://${host}/favicon.ico`} onError=${() => setBad(true)} />`;
}

function Carousel({ urls, alt }) {
  const [i, setI] = useState(0);
  const onScroll = (e) => setI(Math.round(e.currentTarget.scrollLeft / e.currentTarget.clientWidth));
  if (!urls.length) return null;
  return html`<div class="car" onScroll=${onScroll}>
      ${urls.map((u) => html`<div class="car-i"><${Img} src=${u} alt=${alt} /></div>`)}
    </div>
    ${urls.length > 1 && html`<div class="dots">${urls.map((_, j) => html`<i class=${j === i ? 'on' : ''} />`)}</div>`}`;
}

export function Post({ p }) {
  const [more, setMore] = useState(false);
  const preview = plain(p.body, 220);
  const long = plain(p.body).length > preview.length;
  return html`<article class="post">
    <header class="post-h"><${Favicon} p=${p} />
      <div><b>${sourceName(p)}</b><small>${[hostOf(p.source_url) || p.topic, ago(p.created_at)].filter(Boolean).join(' · ')}</small></div>
    </header>
    <${Carousel} urls=${photos(p)} alt=${p.title} />
    <div class="post-b">
      <div class="post-ac">
        <button class=${'heart' + (p.liked ? ' on' : '')} aria-label=${p.liked ? 'Unlike' : 'Like'} aria-pressed=${p.liked ? 'true' : 'false'} onClick=${() => like(p)}>
          <${Icon} name="heart" size=${22} fill=${p.liked} /></button>
        <button onClick=${() => discuss(p)}><${Icon} name="chat" size=${20} /> Talk about it</button>
        ${p.source_url && html`<button class="visit" onClick=${() => visit(p)}>Visit ›</button>`}
      </div>
      <b class="post-t">${p.title}</b>
      ${more ? html`<div class="md" dangerouslySetInnerHTML=${{ __html: md(p.body) }} />`
        : html`<p>${preview}${long && html` <button class="more" onClick=${() => setMore(true)}>more</button>`}</p>`}
      ${p.why && html`<div class="why"><${Icon} name="sparkles" size=${13} /> ${p.why}</div>`}
    </div>
  </article>`;
}

export function FeedScreen() {
  useStore();
  const [busy, setBusy] = useState(false);
  useEffect(() => { if (!S.feed) loadFeed().catch((e) => toast(e.message)); }, []);
  if (!S.feed) return html`<div class="scr tabscr"><${Spinner} /></div>`;
  const posts = S.feed.posts;
  const tps = topics(posts);
  const refresh = async () => {
    setBusy(true);
    try { await apiPost('/feed/refresh'); toast("I'm finding a few things for you. They'll show up here."); } catch (e) { toast(e.message); }
    setBusy(false);
  };
  const onScroll = (e) => {
    const el = e.currentTarget;
    if (S.feed.hasMore && el.scrollHeight - el.scrollTop - el.clientHeight < 600 && !S.feedLoading) {
      S.feedLoading = true;
      loadFeed(true).finally(() => { S.feedLoading = false; });
    }
  };
  return html`<div class="scr tabscr feed" onScroll=${onScroll}>
    ${tps.length > 0 && html`<div class="rail" role="list">
      ${tps.map((t, i) => html`<button class="stile" role="listitem" onClick=${() => set({ story: { t: i, s: 0 } })}>
        <${Img} src=${photos(t.stories[0])[0]} alt="" cls="stile-im" />
        <div class="stile-src"><${Favicon} p=${t.stories[0]} size=${22} /></div>
        <small>${t.name}</small>
      </button>`)}
    </div>`}
    ${posts.map((p) => html`<${Post} p=${p} key=${p.id} />`)}
    ${!posts.length && html`<${Empty} icon="sparkles" title="Your feed is warming up">
      I'll add a few things a day that fit what you like. Tap ♡ on what you enjoy and I'll learn.
      ${isOwner() && html`<div><button class="b1 sm" disabled=${busy} onClick=${refresh}>Find something now</button></div>`}
    <//>`}
    ${posts.length > 0 && !S.feed.hasMore && html`<div class="end mu">You're all caught up.</div>`}
  </div>`;
}

// ------------------------------------------------------------ story viewer

export function StoryViewer() {
  useStore();
  const st = S.story;
  const tps = S.feed ? topics(S.feed.posts) : [];
  const touch = useRef(null);
  const [paused, setPaused] = useState(false);
  const topic = st && tps[st.t];
  const p = topic?.stories[st.s];

  const close = () => set({ story: null });
  const step = (d) => {
    if (!topic) return close();
    const s = st.s + d;
    if (s >= topic.stories.length) return st.t + 1 < tps.length ? set({ story: { t: st.t + 1, s: 0 } }) : close();
    if (s < 0) return st.t > 0 ? set({ story: { t: st.t - 1, s: tps[st.t - 1].stories.length - 1 } }) : set({ story: { ...st, s: 0 } });
    set({ story: { ...st, s } });
  };
  const topicStep = (d) => {
    const t = st.t + d;
    if (t < 0 || t >= tps.length) return d > 0 ? close() : null;
    set({ story: { t, s: 0 } });
  };

  useEffect(() => {
    if (!p || paused) return undefined;
    const id = setTimeout(() => step(1), STORY_MS);
    return () => clearTimeout(id);
  }, [st?.t, st?.s, paused, !!p]);

  useEffect(() => {
    if (!st) return undefined;
    const key = (e) => {
      if (e.key === 'Escape') close();
      if (e.key === 'ArrowRight') step(1);
      if (e.key === 'ArrowLeft') step(-1);
    };
    addEventListener('keydown', key);
    return () => removeEventListener('keydown', key);
  });

  if (!st || !p) return null;

  const down = (e) => { touch.current = { x: e.clientX, y: e.clientY, t: Date.now() }; setPaused(true); };
  const up = (e) => {
    setPaused(false);
    const t0 = touch.current;
    touch.current = null;
    if (!t0 || e.target.closest('button')) return;
    const dx = e.clientX - t0.x;
    const dy = e.clientY - t0.y;
    if (Math.abs(dy) > 70 && Math.abs(dy) > Math.abs(dx)) return dy < 0 ? discuss(p) : close();
    if (Math.abs(dx) > 60) return topicStep(dx < 0 ? 1 : -1);
    if (Date.now() - t0.t > 500) return undefined;
    const w = e.currentTarget.clientWidth;
    step(e.clientX - e.currentTarget.getBoundingClientRect().left < w * 0.3 ? -1 : 1);
  };

  const img = photos(p)[0];
  return html`<div class="viewer" onPointerDown=${down} onPointerUp=${up} onPointerCancel=${() => setPaused(false)}>
    ${img ? html`<${Img} src=${img} alt="" cls="viewer-im" />` : html`<div class="viewer-im viewer-blank" />`}
    <div class="viewer-shade" />
    <div class="bars">${topic.stories.map((_, i) => html`<i class=${i < st.s ? 'f' : ''}>
      ${i === st.s && html`<b class=${'run' + (paused ? ' paused' : '')} style=${`animation-duration:${STORY_MS}ms`} key=${`${st.t}-${st.s}`} />`}</i>`)}</div>
    <div class="viewer-top"><${Favicon} p=${p} size=${28} /><div><b>${topic.name}</b><small>${sourceName(p)} · ${ago(p.created_at)}</small></div>
      <button class="x" aria-label="Close" onClick=${close}><${Icon} name="x" size=${24} /></button></div>
    <div class="viewer-tx">
      <b>${p.title}</b>
      <p>${plain(p.body, 200)}</p>
      ${p.why && html`<div class="why light"><${Icon} name="sparkles" size=${13} /> ${p.why}</div>`}
      <div class="sa">
        <button aria-label="Like" onClick=${() => like(p)}><${Icon} name="heart" size=${18} fill=${p.liked} /></button>
        <button onClick=${() => discuss(p)}>Talk about it</button>
        ${p.source_url && html`<button onClick=${() => visit(p)}>Visit ›</button>`}
      </div>
      <div class="sw">tap for next · swipe up to talk · swipe down to close</div>
    </div>
  </div>`;
}
