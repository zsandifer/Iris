// Immediate, local feedback. The server validates again before saving.
export function googlePasteError(paste) {
  const id = /^[A-Za-z0-9_-]{1,200}\.apps\.googleusercontent\.com$/;
  const secret = /^GOCSPX-[A-Za-z0-9_-]{1,200}$/;
  if (new TextEncoder().encode(paste).length > 65536) return 'I only need the downloaded client JSON or the client ID and secret. This paste is too long.';
  const text = paste.trim().replace(/^\uFEFF/, '');
  let ids, secrets;
  if (text.startsWith('{')) {
    let v;
    try { v = JSON.parse(text); } catch { return 'I couldn’t read that JSON. Copy the whole downloaded file, including the braces.'; }
    const c = v.web ?? v.installed ?? v;
    ids = [typeof c?.client_id === 'string' ? c.client_id.trim() : ''];
    secrets = [typeof c?.client_secret === 'string' ? c.client_secret.trim() : ''];
  } else {
    const tokens = text.split(/[^A-Za-z0-9_.-]+/);
    ids = [...new Set(tokens.filter((t) => id.test(t)))];
    secrets = [...new Set(tokens.filter((t) => secret.test(t)))];
  }
  if (ids.length > 1 || secrets.length > 1) return 'I found more than one login key. Paste just one client’s ID and secret together.';
  if (!id.test(ids[0] || '')) return 'I need the client ID ending in .apps.googleusercontent.com. Paste it with the secret, or paste the downloaded JSON.';
  if (!secret.test(secrets[0] || '')) return 'I found the client ID, but I also need the secret starting with GOCSPX-. Copy both from the same login key.';
  return null;
}
