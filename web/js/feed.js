// For you: story cards per topic, Instagram-like posts, full-screen stories,
// and the feed's sources screen. Posts come from the server's daily
// pipeline (docs/API.md, "For you pipeline").
import { html, useEffect, useRef, useState } from './h.js';
import { get, post as apiPost, patch, del, seg } from './api.js';
import { S, set, useStore, loadFeed, go, toast, isOwner } from './store.js';
import { Icon, Img, LetterTile, Empty, Spinner, Page, Group, Row, Toggle, ago, since, hostOf, safeUrl, agentName, confirmSheet } from './ui.js';
import { md, plain } from './md.js';

// The local copies Iris cached (photos[i]) stand in for the originals.
// The app is served over https, so plain-http originals are left out.
const shown = (u) => typeof u === 'string' && (u.startsWith('/api/') || u.startsWith('https://'));
const photos = (p) => (p.photos?.length ? p.photos : p.images?.length ? p.images : p.image_url ? [p.image_url] : []).filter(shown);
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
  // Stories with a photo first (stable), so each topic opens on a picture.
  const byPhoto = (a, b) => (photos(b).length > 0) - (photos(a).length > 0);
  return [...map.entries()].map(([name, stories]) => ({ name, stories: stories.sort(byPhoto) }));
}

/** Show `liked` on the post wherever it is loaded (feed, Library's Saved). */
function markLiked(p, liked) {
  const swap = (x) => (x.id === p.id ? { ...x, liked } : x);
  const patch = {};
  if (S.feed) patch.feed = { ...S.feed, posts: S.feed.posts.map(swap) };
  if (S.library) {
    const saved = S.library.saved;
    patch.library = {
      ...S.library,
      saved: saved.some((x) => x.id === p.id) ? saved.map(swap) : liked ? [{ ...p, liked }, ...saved] : saved,
    };
  }
  set(patch);
}

/** The freshest loaded copy of a post. */
export function findPost(id) {
  return S.library?.saved.find((x) => x.id === id) || S.feed?.posts.find((x) => x.id === id);
}

async function like(p) {
  const liked = !p.liked;
  markLiked(p, liked);
  try { await apiPost(`/feed/${seg(p.id)}/like`, { liked }); } catch (e) {
    toast(e.message);
    markLiked(p, !liked);
  }
}

export async function discuss(p) {
  try {
    const r = await apiPost(`/feed/${seg(p.id)}/discuss`);
    set({ story: null });
    go(`chat/${r.conversation.id}`);
  } catch (e) { toast(e.message); }
}

const visit = (p) => safeUrl(p.source_url) && window.open(p.source_url, '_blank', 'noopener,noreferrer');

function Favicon({ p, size = 30 }) {
  const [bad, setBad] = useState(0);
  const host = hostOf(p.source_url);
  // Reserved names (RFC 2606/6761) never resolve; skip the request.
  const reserved = /(^|\.)(example|invalid|test|localhost|local)$/.test(host);
  // Pipeline posts only ever show the icon Iris cached; posts the agent
  // wrote may fall back to the site's own favicon.
  const guess = !p.source_kind && !reserved && host && `https://${host}/favicon.ico`;
  const candidates = [p.favicon, guess].filter(shown);
  const src = candidates[bad];
  if (!src) return html`<${LetterTile} name=${sourceName(p)} size=${size} radius=${size / 2} />`;
  const style = `width:${size}px;height:${size}px`;
  if (src.startsWith('/api/')) return html`<span class="fav" style=${style}><${Img} src=${src} alt="" onError=${() => setBad(bad + 1)} /></span>`;
  return html`<img class="fav" style=${style} alt="" referrerpolicy="no-referrer" src=${src} onError=${() => setBad(bad + 1)} />`;
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

const money = (pr) => {
  if (!pr || !(pr.amount > 0)) return '';
  try { return new Intl.NumberFormat(undefined, { style: 'currency', currency: pr.currency || 'USD' }).format(pr.amount); } catch { return `${pr.amount} ${pr.currency || ''}`.trim(); }
};

/** One line of facts read from the page (event date, price). */
function Facts({ p }) {
  const ev = p.facts?.event;
  if (ev?.start_ms) {
    const when = new Date(ev.start_ms).toLocaleDateString(undefined, { weekday: 'short', month: 'short', day: 'numeric' });
    return html`<div class="fact"><${Icon} name="target" size=${13} /> ${[when, ev.location].filter(Boolean).join(' · ')}</div>`;
  }
  if (p.source_kind === 'price_drop' && p.facts?.price) {
    return html`<div class="fact good">Now ${money(p.facts.price)}${p.facts.was ? html` <s>${money(p.facts.was)}</s>` : ''}</div>`;
  }
  return null;
}

export function Post({ p }) {
  const [more, setMore] = useState(false);
  const preview = plain(p.body, 220);
  const long = plain(p.body).length > preview.length;
  return html`<article class="post" data-id=${p.id}>
    <header class="post-h"><${Favicon} p=${p} />
      <div><b>${sourceName(p)}</b><small>${[p.topic || hostOf(p.source_url), ago(p.created_at)].filter(Boolean).join(' · ')}</small></div>
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
      <${Facts} p=${p} />
      ${more ? html`<div class="md" dangerouslySetInnerHTML=${{ __html: md(p.body) }} />`
        : html`<p>${preview}${long && html` <button class="more" onClick=${() => setMore(true)}>more</button>`}</p>`}
      ${p.why && html`<div class="why"><${Icon} name="sparkles" size=${13} /> ${p.why}</div>`}
    </div>
    <i class="post-end" aria-hidden="true" />
  </article>`;
}

// ------------------------------------------------------------ feed screen

/** Report feed use (drives the adaptive daily size and the 2-week pause). */
const report = (event) => apiPost('/feed/activity', { event }).catch(() => {});

function useEndTracking(posts, status) {
  // The end of today's batch is its last (lowest-ranked) post; it may not
  // be loaded yet, in which case there is nothing to watch.
  const endId = status?.last_batch_end_id;
  const loaded = !!endId && posts.some((p) => p.id === endId);
  useEffect(() => {
    const key = `iris.feedEnd.${endId}`;
    if (!loaded || sessionStorage.getItem(key) || !('IntersectionObserver' in window)) return undefined;
    // The bottom edge of the batch's last post: seeing it means the user
    // scrolled through the whole post, not just its first pixels.
    const el = document.querySelector(`.post[data-id="${endId}"] .post-end`);
    if (!el) return undefined;
    const io = new IntersectionObserver((entries) => {
      if (entries.some((e) => e.isIntersecting && e.intersectionRatio > 0)) {
        sessionStorage.setItem(key, '1');
        report('reached_end');
        io.disconnect();
      }
    }, { threshold: 1 });
    io.observe(el);
    return () => io.disconnect();
  }, [endId, loaded]);
}

/** Report an open on mount and whenever the app comes back to the front. */
function useOpenTracking() {
  useEffect(() => {
    let last = 0;
    const opened = () => {
      if (document.visibilityState !== 'visible' || Date.now() - last < 30 * 60e3) return;
      last = Date.now();
      report('opened');
    };
    opened();
    document.addEventListener('visibilitychange', opened);
    return () => document.removeEventListener('visibilitychange', opened);
  }, []);
}

function PausedCard({ status }) {
  const [busy, setBusy] = useState(false);
  const resume = async () => {
    setBusy(true);
    try { await apiPost('/feed/resume'); await loadFeed(); toast("Your feed is back on. I'm finding today's posts."); } catch (e) { toast(e.message); }
    setBusy(false);
  };
  const why = status.paused_reason === 'user' ? 'You paused it, so I stopped looking for new posts.'
    : "It hadn't been opened in two weeks, so I stopped looking for posts instead of piling them up.";
  const when = status.paused_at && Date.now() - status.paused_at > 3600e3 ? `Paused ${since(status.paused_at)}. ` : '';
  return html`<div class="card feed-note"><b>Your feed is paused</b>
    <p>${when}${why}</p>
    <div class="act"><button class="b1 sm" disabled=${busy} onClick=${resume}>Resume</button></div></div>`;
}

export function FeedScreen() {
  useStore();
  const [busy, setBusy] = useState(false);
  useEffect(() => { if (!S.feed) loadFeed().catch((e) => toast(e.message)); }, []);
  useOpenTracking();
  const posts = S.feed?.posts || [];
  const st = S.feed?.status;
  useEndTracking(posts, st);
  if (!S.feed) return html`<div class="scr tabscr"><${Spinner} /></div>`;
  const tps = topics(posts);
  const refresh = async () => {
    setBusy(true);
    try {
      const r = await apiPost('/feed/refresh');
      toast(r.started ? "I'm looking for things for you. They'll show up here in a minute or two." : "I'm already on it.");
      await loadFeed();
    } catch (e) { toast(e.message); }
    setBusy(false);
  };
  const onScroll = (e) => {
    const el = e.currentTarget;
    if (S.feed.hasMore && el.scrollHeight - el.scrollTop - el.clientHeight < 600 && !S.feedLoading) {
      S.feedLoading = true;
      loadFeed(true).finally(() => { S.feedLoading = false; });
    }
  };
  const empty = () => {
    if (st?.running) {
      return html`<${Empty} icon="sparkles" title="Finding your first posts">
        I'm reading your sources and picking a few things that fit what you like. This takes a minute or two.<//>`;
    }
    if (!st?.pipeline) {
      return html`<${Empty} icon="sparkles" title="Nothing here yet">Things ${agentName()} finds for you show up here.<//>`;
    }
    if (!st.enabled) {
      return html`<${Empty} icon="sparkles" title="The feed is turned off">
        It's switched off in the server's settings (<code>[feed] enabled</code>). You can still ask me for a fresh batch.
        ${isOwner() && html`<div><button class="b1 sm" disabled=${busy} onClick=${refresh}>Find something now</button></div>`}<//>`;
    }
    return html`<${Empty} icon="sparkles" title="Nothing here yet">
      Every morning I pick about ${st.daily_size || 10} posts from places that fit what you like. Tap ♡ on what you enjoy and I'll learn.
      ${isOwner() && html`<div><button class="b1 sm" disabled=${busy} onClick=${refresh}>Find something now</button></div>`}<//>`;
  };
  return html`<div class="scr tabscr feed" onScroll=${onScroll}>
    ${st?.paused && html`<${PausedCard} status=${st} />`}
    ${st?.running && posts.length > 0 && html`<div class="feed-run"><span class="shimmer">Finding today's posts…</span></div>`}
    ${tps.length > 0 && html`<div class="rail" role="list">
      ${tps.map((t, i) => html`<button class="stile" role="listitem" onClick=${() => set({ story: { t: i, s: 0 } })}>
        <${Img} src=${photos(t.stories[0])[0]} alt="" cls="stile-im" />
        <div class="stile-src"><${Favicon} p=${t.stories[0]} size=${22} /></div>
        <small>${t.name}</small>
      </button>`)}
    </div>`}
    ${posts.map((p) => html`<${Post} p=${p} key=${p.id} />`)}
    ${!posts.length && empty()}
    ${posts.length > 0 && !S.feed.hasMore && html`<div class="end mu">You're all caught up.${st?.pipeline && !st.paused ? ' New posts arrive each morning.' : ''}</div>`}
    ${st?.pipeline && html`<div class="end"><button class="linkish" onClick=${() => go('settings/feed')}>Sources and settings</button></div>`}
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
      <b>${p.hook || p.title}</b>
      ${p.hook && html`<div class="viewer-sub">${p.title}</div>`}
      <p>${plain(p.body, 200)}</p>
      ${p.why && html`<div class="why light"><${Icon} name="sparkles" size=${13} /> ${p.why}</div>`}
      <div class="sa">
        <button aria-label=${p.liked ? 'Unlike' : 'Like'} onClick=${() => like(p)}><${Icon} name="heart" size=${18} fill=${p.liked} /></button>
        <button onClick=${() => discuss(p)}>Talk about it</button>
        ${p.source_url && html`<button onClick=${() => visit(p)}>Visit ›</button>`}
      </div>
      <div class="sw">tap for next · swipe up to talk · swipe down to close</div>
    </div>
  </div>`;
}

// --------------------------------------------------------- feed settings

const KIND_LABEL = { rss: 'Website', youtube: 'YouTube', reddit: 'Reddit', hn: 'Hacker News', bluesky: 'Bluesky', mastodon: 'Mastodon' };
const ORIGIN_LABEL = { user: 'Added by you', suggested: 'Suggested by me', google: 'From your YouTube' };

function sourceTitle(s) {
  if (s.label || s.title) return s.label || s.title;
  if (s.kind === 'reddit') return `r/${s.value}`;
  if (s.kind === 'hn') return s.value === 'front_page' ? 'Hacker News front page' : `Hacker News: ${s.value}`;
  if (s.kind === 'rss') return hostOf(s.value) || s.value;
  return s.value;
}

export function FeedSettings() {
  useStore();
  const [sources, setSources] = useState(null);
  const [input, setInput] = useState('');
  const [busy, setBusy] = useState(false);
  const load = () => get('/feed/sources').then((r) => setSources(r.sources)).catch((e) => toast(e.message));
  useEffect(() => { load(); if (!S.feed) loadFeed().catch(() => {}); }, []);
  const st = S.feed?.status;
  const add = async (e) => {
    e.preventDefault();
    if (!input.trim()) return;
    setBusy(true);
    try { await apiPost('/feed/sources', { input: input.trim() }); setInput(''); await load(); toast("Added. I'll check it with tomorrow's posts."); } catch (err) { toast(err.message); }
    setBusy(false);
  };
  const toggle = async (s, on) => {
    try { await patch(`/feed/sources/${seg(s.id)}`, { enabled: on }); await load(); } catch (e) { toast(e.message); }
  };
  const remove = async (s) => {
    if (!(await confirmSheet({ title: 'Remove this source?', body: sourceTitle(s), yes: 'Remove', danger: true }))) return;
    try { await del(`/feed/sources/${seg(s.id)}`); await load(); } catch (e) { toast(e.message); }
  };
  const pauseToggle = async (on) => {
    try { await apiPost(on ? '/feed/resume' : '/feed/pause'); await loadFeed(); } catch (e) { toast(e.message); }
  };
  const last = st?.last_run;
  return html`<${Page} title="For you" sub="Where your feed comes from and how it works.">
    ${st?.pipeline && html`<${Group} note=${`About ${st.daily_size} posts a day. It grows when you keep reaching the end and shrinks when you rarely look. Only big personal things (a price drop on something you saved, an event near you) send a notification.`}>
      <${Row} icon="sparkles" title="Daily posts" sub=${st.paused ? 'Paused' : last ? `Last batch ${ago(last.at)}: ${last.published} posts from ${last.sources_checked} sources` : 'First batch coming up'}
        right=${html`<${Toggle} on=${!st.paused} onChange=${pauseToggle} label="Daily posts" />`} />
    <//>`}
    ${st?.topics?.length > 0 && html`<div class="gl">Today's topics</div><div class="pills static">${st.topics.map((t) => html`<button disabled>${t}</button>`)}</div>`}
    <div class="gl">Add a source</div>
    <form class="add-row" onSubmit=${add}>
      <input class="field" value=${input} placeholder="Link, r/subreddit, @handle, #tag" onInput=${(e) => setInput(e.target.value)} />
      <button class="b1 sm" disabled=${busy || !input.trim()}>Add</button>
    </form>
    <div class="gnote">Websites and blogs (any link with a feed), YouTube channels, subreddits, Bluesky and Mastodon accounts, or Mastodon hashtags.</div>
    ${!sources ? html`<${Spinner} />` : html`<${Group} label="Sources">
      ${sources.length ? sources.map((s) => html`<${Row} icon=${html`<${LetterTile} name=${KIND_LABEL[s.kind] || s.kind} />`}
          title=${sourceTitle(s)}
          sub=${[KIND_LABEL[s.kind], ORIGIN_LABEL[s.origin],
            s.origin === 'suggested' && !s.enabled && !s.last_fetch_at ? 'turn on to use'
              : s.last_error && s.fail_count ? "couldn't reach it lately" : s.topic].filter(Boolean).join(' · ')}
          right=${html`<span class="src-ac"><${Toggle} on=${s.enabled} onChange=${(on) => toggle(s, on)} label="On" />
            <button class="btn" aria-label="Remove" onClick=${() => remove(s)}><${Icon} name="trash" size=${17} /></button></span>`} />`)
        : html`<div class="r"><div class="rt mu">None yet. I'll suggest some after my first look at what you like.</div></div>`}
    <//>`}
    <div class="gnote">I read feeds once a day, identify myself as Iris, and follow each site's robots.txt for pages and photos.</div>
  <//>`;
}
