// Plain-language wording for memory: topics, "learned last night" marks,
// history lines, change tags and the Noted line, plus the Undo/Redo
// bookkeeping. No imports, so the tests can load it without a browser.

export const TOPICS = [
  ['you', 'You'],
  ['people', 'People'],
  ['projects', 'Projects'],
  ['routines', 'Routines'],
];

/** Heading for a topic's group (and the detail sheet title). */
export const TOPIC_TITLE = { you: 'About you', people: 'People', projects: 'Projects', routines: 'Routines' };

const DAY = 86400000;
const RECENT = 2 * DAY;

const sameDay = (a, b) => new Date(a).toDateString() === new Date(b).toDateString();

/** "last night", "today", "yesterday", or null when it was longer ago. */
export function whenWord(ms, now = Date.now()) {
  const hour = new Date(ms).getHours();
  if (sameDay(ms, now)) return hour < 5 ? 'last night' : 'today';
  if (sameDay(ms, now - DAY)) return hour >= 21 ? 'last night' : 'yesterday';
  return null;
}

const MONTH = { month: 'long' };
const MONTH_YEAR = { month: 'long', year: 'numeric' };

export function shortDate(ms, now = Date.now()) {
  const d = new Date(ms);
  const opts = { month: 'short', day: 'numeric' };
  if (d.getFullYear() !== new Date(now).getFullYear()) opts.year = 'numeric';
  return d.toLocaleDateString(undefined, opts);
}

/**
 * The small line under a note in the list: what changed lately, or how
 * long it has been true. `recent` marks it as a change worth noticing.
 */
export function noteMark(item, now = Date.now()) {
  const learned = item.created_at && now - item.created_at < RECENT && whenWord(item.created_at, now);
  if (learned) return { text: `Learned ${learned}`, recent: true };
  const updated = item.updated_at && now - item.updated_at < RECENT && whenWord(item.updated_at, now);
  if (updated) return { text: `Updated ${updated}`, recent: true };
  const since = item.observed_at || item.created_at;
  if (!since) return { text: '', recent: false };
  const d = new Date(since);
  const opts = d.getFullYear() === new Date(now).getFullYear() ? MONTH : MONTH_YEAR;
  return { text: `Since ${d.toLocaleDateString(undefined, opts)}`, recent: false };
}

/** Topic groups for the list. `topic` null = All: Pinned, then each topic. */
export function groupNotes(items, topic = null) {
  const shown = topic ? items.filter((i) => i.topic === topic) : items;
  const pinned = shown.filter((i) => i.pinned);
  const rest = shown.filter((i) => !i.pinned);
  const groups = [];
  if (pinned.length) groups.push({ key: 'pinned', label: 'Pinned', items: pinned });
  for (const [k] of TOPICS) {
    if (topic && k !== topic) continue;
    const list = rest.filter((i) => i.topic === k);
    if (list.length) groups.push({ key: k, label: k === 'you' ? 'You' : TOPIC_TITLE[k], items: list });
  }
  // A topic the app does not know yet still shows.
  const other = rest.filter((i) => !TOPIC_TITLE[i.topic]);
  if (other.length) groups.push({ key: 'other', label: 'Other', items: other });
  return groups;
}

export function countLabel(n, more = false) {
  if (more) return `${n}+ things`;
  return n === 1 ? '1 thing' : `${n} things`;
}

const quote = (s, n = 80) => {
  const t = String(s || '').replace(/\s+/g, ' ').trim();
  return `“${t.length > n ? t.slice(0, n - 1).trimEnd() + '…' : t}”`;
};

/**
 * History, newest first, in plain words. A correction is stored as the new
 * note's addition plus the old note being replaced; it shows as one line.
 * Only the newest change can be undone (the server checks that too).
 */
export function historyLines(history = []) {
  const corrections = new Set(history.filter((h) => h.op === 'supersede' && h.after?.superseded_by).map((h) => `${h.batch_id}:${h.after.superseded_by}`));
  const lines = [];
  for (const h of history) {
    if (h.op === 'add' && corrections.has(`${h.batch_id}:${h.item_id}`)) continue;
    lines.push({ id: h.id, at: h.at, text: historyText(h) });
  }
  if (lines.length) lines[0].undo = lines[0].id === history[0]?.id;
  return lines;
}

const BY = { owner: 'by you', agent: 'in a chat', dream: 'overnight', import: '' };

export function historyText(h) {
  const by = BY[h.actor] ?? '';
  const was = h.before?.text;
  const now = h.after?.text;
  switch (h.op) {
    case 'add':
      if (h.actor === 'owner') return 'You added it';
      if (h.actor === 'import') return 'Brought over from my older notes';
      return `First learned ${by}`.trim();
    case 'update':
      if (was && now && was !== now) return `Updated ${by} from ${quote(was)}`.replace('  ', ' ');
      if (h.before && h.after && h.before.pinned !== h.after.pinned) return h.after.pinned ? `Pinned ${by}`.trim() : `Unpinned ${by}`.trim();
      return `Details changed ${by}`.trim();
    case 'supersede':
      if (h.after?.superseded_by) return `Corrected ${by} from ${quote(was)}`.replace('  ', ' ');
      return h.actor === 'dream' ? 'Marked as done overnight' : `No longer true ${by}`.trim();
    case 'archive':
      return h.actor === 'dream' ? 'Set aside as no longer current' : `Set aside ${by}`.trim();
    case 'restore':
      return h.actor === 'owner' ? 'You undid a change' : 'Brought back';
    case 'delete':
      return h.actor === 'owner' ? 'You asked me to forget it' : `Forgotten ${by}`.trim();
    default:
      return 'Changed';
  }
}

/**
 * Tag on a change: [label, class]. In the morning note a replaced note is
 * one that is over ("Done"); in a chat it was corrected ("Updated").
 * Morning notes from before tags existed have no op, so no tag.
 */
export function changeTag(op, { noted = false } = {}) {
  if (op === 'add') return ['New', 'new'];
  if (op === 'update') return ['Updated', 'chg'];
  if (op === 'supersede') return noted ? ['Updated', 'chg'] : ['Done', 'old'];
  if (op === 'archive') return ['Done', 'old'];
  if (op === 'delete') return ['Forgot', 'old'];
  if (op === 'restore') return ['Back', 'new'];
  return null;
}

const VERB = { add: 'Noted', update: 'Updated', supersede: 'Updated', delete: 'Forgot', archive: 'Set aside', restore: 'Brought back' };

/** The line under a reply that saved, changed, or forgot something. */
export function notedLine(changes = []) {
  if (!changes.length) return '';
  const first = changes[0];
  const text = String(first.text || '').replace(/\s+/g, ' ').trim();
  const more = changes.length > 1 ? ` +${changes.length - 1} more` : '';
  return `${VERB[first.op] || 'Noted'}: ${text}${more}`;
}

export const verbFor = (op) => VERB[op] || 'Noted';

/**
 * Undo/Redo of changes shown in Noted sheets and morning notes. Each undo
 * (and redo) is itself a new change, and only a note's newest change can be
 * undone, so the next tap always goes to the newest one. `first(id)` makes
 * the first undo (the morning note has its own route); `again(id)` the rest.
 */
export function undoTracker() {
  const state = new Map(); // original change id -> {off, latest}
  return {
    isOff: (id) => !!state.get(id)?.off,
    async toggle(id, first, again) {
      const s = state.get(id);
      const r = s ? await again(s.latest) : await first(id);
      state.set(id, { off: !s || !s.off, latest: r.history_id });
    },
    clear: () => state.clear(),
  };
}

/** Shared by every Noted sheet and morning note, so they agree. */
export const undos = undoTracker();

/** A cached message without what Forget everything removed on the server. */
export function withoutMemory(m) {
  if (m.card?.type === 'memory_morning') return { ...m, kind: 'text', card: null, content: 'I cleared my notes.' };
  if (m.meta && 'noted' in m.meta) {
    const { noted, ...meta } = m.meta;
    return { ...m, meta };
  }
  return m;
}

/**
 * Where a note came from: {text, cid?} where cid is a chat to open.
 * Conversation and overnight sources link the chat; the rest are words.
 */
export function sourceOf(item, now = Date.now()) {
  const s = item?.source || {};
  const at = item?.mentioned_at || item?.created_at;
  const date = at ? ` · ${shortDate(at, now)}` : '';
  if (s.type === 'conversation' && s.conversation_id) return { text: `In a chat${date}`, cid: s.conversation_id };
  if (s.type === 'dream') {
    const cid = s.messages?.find((m) => m?.conversation_id)?.conversation_id;
    return { text: `From our chats${date}`, cid };
  }
  if (s.type === 'import') return { text: 'From my older notes' };
  if (s.type === 'owner') return { text: `You told me here${date}` };
  return { text: `I've known this for a while${date}` };
}
