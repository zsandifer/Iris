// login-text.js: the plain logic behind saving a login (no UI), so it can
// be tested in node.

/** The host of a website the owner typed or Iris filled in ('' if none). */
export function siteHost(site) {
  const s = String(site || '').trim();
  if (!s) return '';
  try {
    return new URL(s.includes('://') ? s : `https://${s}`).hostname.toLowerCase();
  } catch {
    return '';
  }
}

/** A bare domain the owner typed, such as example.com (or www.example.com):
 * they most likely mean the whole site, its sign-in subdomains included. A
 * full address (with https:// or a path) means just that host. */
export function bareDomain(site) {
  const s = String(site || '').trim();
  if (/[/:@?#]/.test(s)) return false;
  const host = siteHost(s).replace(/^www\./, '');
  if (!host || /^[\d.]+$/.test(host) || host.includes(':')) return false;
  return host.split('.').length === 2;
}

/** The hosts a save_login card would let a login be used on. */
export function cardHosts(card) {
  const out = [];
  for (const s of [card.site, ...(card.also_sites || [])]) {
    const h = siteHost(s);
    if (h && !out.includes(h)) out.push(h);
  }
  return out;
}

/** The Logins & wallet route that adds the login an agent's save_login
 * card asks for, with what Iris knows filled in; it comes back to the chat. */
export function saveLoginPath(card, message) {
  const q = new URLSearchParams({ card: message.id });
  if (card.site) q.set('site', card.site);
  q.set('label', card.label || card.host || '');
  if (card.username) q.set('username', card.username);
  for (const s of card.also_sites || []) q.append('also', s);
  q.set('return', `chat/${message.conversation_id}`);
  return `iris/logins/new?${q}`;
}

/** What the add-login form starts with, from that route's query. */
export function loginDraft(query) {
  const site = query.get('site') || '';
  return {
    card: query.get('card') || null,
    label: query.get('label') || '',
    site,
    also_sites: query.getAll('also').filter(Boolean),
    username: query.get('username') || '',
    allow_subdomains: bareDomain(site),
  };
}

/** Extra sites typed one per line (or separated by commas or spaces). */
export function parseSites(text) {
  const out = [];
  for (const s of String(text || '').split(/[\s,]+/)) {
    if (s && !out.some((o) => o.toLowerCase() === s.toLowerCase())) out.push(s);
  }
  return out;
}
