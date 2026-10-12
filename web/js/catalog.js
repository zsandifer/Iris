// The connector catalog's display logic, kept free of UI so it can be
// tested in node: brand tiles, which rows a group shows, what a row's
// button says and opens, and how a permission choice becomes a rule.

// Catalog services Iris sets up itself: name, tile color, mark (a simple
// monogram in the brand's color, drawn locally; no logos are fetched).
export const SERVICES = {
  google: { name: 'Google', bg: '#4285f4', mark: 'G' },
  notion: { name: 'Notion', bg: '#191919', mark: 'N' },
  todoist: { name: 'Todoist', bg: '#e44332', mark: 'T' },
  airtable: { name: 'Airtable', bg: '#18bfff', mark: 'A' },
  monday: { name: 'monday.com', bg: '#ff3d57', mark: 'm' },
  miro: { name: 'Miro', bg: '#ffd02f', fg: '#050038', mark: 'M' },
  canva: { name: 'Canva', bg: '#00a3a8', mark: 'C' },
  calcom: { name: 'Cal.com', bg: '#111111', mark: 'Cal' },
  granola: { name: 'Granola', bg: '#2f4f2f', mark: 'G' },
  fireflies: { name: 'Fireflies', bg: '#e5357e', mark: 'F' },
  zapier: { name: 'Zapier', bg: '#ff4f00', mark: 'Z' },
  github: { name: 'GitHub', bg: '#24292f', mark: 'GH' },
  linear: { name: 'Linear', bg: '#5e6ad2', mark: 'L' },
  sentry: { name: 'Sentry', bg: '#362d59', mark: 'S' },
  cloudflare: { name: 'Cloudflare', bg: '#f38020', mark: 'CF' },
  supabase: { name: 'Supabase', bg: '#3ecf8e', fg: '#1c1c1c', mark: 'S' },
  netlify: { name: 'Netlify', bg: '#05bdba', mark: 'N' },
  neon: { name: 'Neon', bg: '#00e599', fg: '#0c0d0d', mark: 'N' },
  huggingface: { name: 'Hugging Face', bg: '#ffd21e', fg: '#1f2937', mark: 'HF' },
  stripe: { name: 'Stripe', bg: '#635bff', mark: 'S' },
  mercury: { name: 'Mercury', bg: '#1e1e2a', mark: 'M' },
  readwise: { name: 'Readwise', bg: '#111827', mark: 'R' },
};

// Google's services, each shown as its own row behind one connection.
export const GOOGLE_PARTS = {
  gmail: { name: 'Gmail', bg: '#ea4335', mark: 'M' },
  calendar: { name: 'Google Calendar', bg: '#4285f4', mark: '31' },
  drive: { name: 'Google Drive', bg: '#fbbc04', fg: '#202124', mark: 'D' },
  docs: { name: 'Google Docs', bg: '#4285f4', mark: 'D' },
  sheets: { name: 'Google Sheets', bg: '#0f9d58', mark: 'S' },
  slides: { name: 'Google Slides', bg: '#f29900', mark: 'S' },
  forms: { name: 'Google Forms', bg: '#7248b9', mark: 'F' },
  tasks: { name: 'Google Tasks', bg: '#1a73e8', mark: 'T' },
  contacts: { name: 'Google Contacts', bg: '#1a73e8', mark: 'C' },
  youtube: { name: 'YouTube', bg: '#ff0000', mark: '▶' },
};

export const CATALOG_IDS = [...Object.keys(SERVICES), 'custom'];

/** Tile colors and mark for a connection, catalog row, or Google part. */
export function brandOf({ service, part, id } = {}) {
  if (part && GOOGLE_PARTS[part]) return GOOGLE_PARTS[part];
  return SERVICES[service || id] || null;
}

/** How many rows a group shows before "See all". */
export const TOP = 5;

export function topRows(group, n = TOP) {
  const rows = group?.rows || [];
  return { rows: rows.slice(0, n), more: rows.length > n };
}

/** A row's button: View once its connection exists and is signed in. */
export function rowAction(row) {
  return row.connector_id && row.state !== 'needs_sign_in' ? 'View' : 'Connect';
}

/** Small status under a row's name, when it isn't simply working. */
export function rowStatus(row) {
  return { off: 'Off', paused: 'Paused', needs_sign_in: row.connector_id ? 'Needs sign-in' : null }[row.state] || null;
}

/** Where tapping a row goes (an app route). */
export function rowTarget(row) {
  if (!row.connector_id) return `iris/connections/add/${row.service}`;
  if (row.part) return `iris/connections/${row.connector_id}/${row.part}`;
  return `iris/connections/${row.connector_id}`;
}

export const CHOICE_LABEL = { allow: 'Allow', ask: 'Ask', off: 'Off' };

/**
 * The rule to save for a picker choice. Choosing what the rules already do
 * removes the override (null), so "ask unless trusted" defaults keep
 * working; otherwise "off" is stored as "block".
 */
export function ruleFor(choice, def) {
  if (choice === def) return null;
  return choice === 'off' ? 'block' : choice;
}
