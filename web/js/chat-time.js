// Pure chat timeline helpers: day separators, message times, and the
// new-message count on the jump-to-latest button. No DOM, so node tests
// import this directly. Times are in the device's time zone and locale.

/** A run of messages from one sender gets a new time after this long. */
export const RUN_GAP = 10 * 60 * 1000;
/** Show the jump-to-latest button this far (px) from the bottom. */
export const JUMP_AWAY = 300;
/** Within this distance (px) of the bottom the chat follows new messages. */
export const NEAR_BOTTOM = 90;

const DAY = 86400000;

/** Local calendar day as a whole number (daylight-saving safe). */
export function dayNumber(ms) {
  const d = new Date(ms);
  return Math.round(Date.UTC(d.getFullYear(), d.getMonth(), d.getDate()) / DAY);
}

/** "Today", "Yesterday", a weekday within the last week, else a date. */
export function dayLabel(ms, now = Date.now(), locale = undefined) {
  const ago = dayNumber(now) - dayNumber(ms);
  if (ago === 0) return 'Today';
  if (ago === 1) return 'Yesterday';
  const d = new Date(ms);
  if (ago > 1 && ago < 7) return d.toLocaleDateString(locale, { weekday: 'long' });
  const opts = { weekday: 'short', month: 'short', day: 'numeric' };
  if (d.getFullYear() !== new Date(now).getFullYear()) opts.year = 'numeric';
  return d.toLocaleDateString(locale, opts);
}

/** "3:42 PM" (or "15:42", per the locale). */
export function timeLabel(ms, locale = undefined) {
  return new Date(ms).toLocaleTimeString(locale, { hour: 'numeric', minute: '2-digit' });
}

/** Full date and time, for the tooltip on a message time. */
export function fullLabel(ms, locale = undefined) {
  return new Date(ms).toLocaleString(locale, { dateStyle: 'full', timeStyle: 'short' });
}

/**
 * Which separators and times to show. `items` are the messages in display
 * order as `{ at, who }`: `at` is created_at (ms), `who` identifies the
 * sender (null for lines with no time of their own, like system notes,
 * which never split a run). Returns `{ day, time }` per item: `day` is the
 * separator label to put before it (or null), and `time` says whether it
 * ends a run and shows its time. A run ends at a change of sender or day,
 * or when that sender's next message comes more than RUN_GAP later.
 */
export function timeline(items, now = Date.now(), locale = undefined) {
  const next = new Array(items.length).fill(null);
  for (let i = items.length - 2, after = null; i >= 0; i--) {
    if (items[i + 1].who != null) after = items[i + 1];
    next[i] = after;
  }
  let lastDay = null;
  return items.map((it, i) => {
    const day = dayNumber(it.at);
    const label = day !== lastDay ? dayLabel(it.at, now, locale) : null;
    lastDay = day;
    if (it.who == null) return { day: label, time: false };
    const n = next[i];
    const time = !n || n.who !== it.who || n.at - it.at > RUN_GAP || dayNumber(n.at) !== day;
    return { day: label, time };
  });
}

/**
 * How many messages after `seenId` (the newest message the reader had in
 * view) are not their own. 0 when `seenId` is no longer in the list.
 */
export function newSince(list, seenId, meId) {
  const i = list.findIndex((m) => m.id === seenId);
  if (i < 0) return 0;
  let n = 0;
  for (const m of list.slice(i + 1)) if (!(m.role === 'user' && m.author_user_id === meId)) n++;
  return n;
}

/** Badge text for a new-message count. */
export const badge = (n) => (n > 9 ? '9+' : String(n));
