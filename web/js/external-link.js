import { html } from './h.js';
import { externalUrl, openExternal } from './external.js';
import { Icon } from './ui.js';
import { toast } from './store.js';

export function ExternalLink({ url, children = 'Open', cls = 'pill' }) {
  const href = externalUrl(url);
  if (!href) return null;
  const copy = async () => {
    try { await navigator.clipboard.writeText(url); toast('Link copied'); }
    catch { toast('I couldn’t copy it. Press and hold Open to copy the link.'); }
  };
  return html`<div class="external-links">
    <a class=${cls} href=${href} target="_blank" rel="noopener noreferrer" onClick=${(e) => { e.preventDefault(); openExternal(url); }}>${children} <${Icon} name="external" size=${14} /></a>
    <button class="lk-m" type="button" onClick=${copy}>Copy link</button>
  </div>`;
}
