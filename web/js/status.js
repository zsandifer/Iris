// Turn harness activity (tool use reported by the CLI) into short, specific,
// plain-language status phrases: "Checking your calendar…" while running,
// "Checked your calendar" in the receipt afterwards.

// Shorten a query at a word boundary (no ellipsis: phrases end in one).
const cut = (s, n = 34) => {
  s = String(s || '').replace(/\s+/g, ' ').trim();
  if (s.length <= n) return s;
  const head = s.slice(0, n);
  return (head.includes(' ') ? head.slice(0, head.lastIndexOf(' ')) : head).replace(/[\s,.;:]+$/, '');
};

// CLI plumbing that means nothing to a person (tool discovery and the like).
const INTERNAL = /^(toolsearch|listmcpresourcestool|readmcpresourcetool|exitplanmode|enterplanmode|killshell|bashoutput)$/;
export const visible = (a) => !INTERNAL.test(String(a.name || '').toLowerCase());

function host(s) {
  const m = String(s || '').match(/https?:\/\/([^/\s?#]+)/i);
  if (m) return m[1].replace(/^www\./, '');
  const d = String(s || '').match(/\b([a-z0-9-]+\.)+[a-z]{2,}\b/i);
  return d ? d[0].replace(/^www\./, '') : '';
}

const title = (s) => String(s || '').replace(/[-_]+/g, ' ').replace(/\b\w/g, (c) => c.toUpperCase());

const IRIS_TOOLS = {
  create_goal: ['Adding a goal', 'Added a goal'],
  update_goal: ['Updating your goals', 'Updated your goals'],
  list_goals: ['Checking your goals', 'Checked your goals'],
  create_idea: ['Jotting down an idea', 'Saved an idea for you'],
  list_ideas: ['Looking at your ideas', 'Looked at your ideas'],
  create_feed_post: ['Adding to your feed', 'Added a post to your feed'],
  list_feed_posts: ['Looking at your feed', 'Looked at your feed'],
  save_to_library: ['Saving it to your library', 'Saved to your library'],
  list_library: ['Looking through your library', 'Looked through your library'],
  create_card: ['Putting a card together', 'Made a card'],
  list_connectors: ['Checking your connections', 'Checked your connections'],
};

const SERVICES = [
  [/mail|inbox|imap/, (t) => (/send/.test(t) ? ['Sending an email', 'Sent an email']
    : /draft|reply|compose/.test(t) ? ['Getting an email ready', 'Got an email ready'] : ['Reading your email', 'Read your email'])],
  [/calendar|gcal|event/, (t) => (/create|add|insert|update/.test(t) ? ['Updating your calendar', 'Updated your calendar'] : ['Checking your calendar', 'Checked your calendar'])],
  [/drive|docs|sheets|notion|dropbox|files?\b/, () => ['Looking through your documents', 'Looked through your documents']],
  [/browser|playwright|puppeteer|chrome|navigate/, () => ['Browsing the web', 'Browsed the web']],
  [/spotify|music/, () => ['Checking Spotify', 'Checked Spotify']],
  [/github|gitlab/, () => ['Checking GitHub', 'Checked GitHub']],
  [/slack/, () => ['Checking Slack', 'Checked Slack']],
  [/maps|places|resy|opentable|yelp|restaurant|tables?\b|booking|reserv/, (t) => (/book|reserve|confirm/.test(t) ? ['Making the booking', 'Made the booking'] : ['Looking up places', 'Looked up places'])],
  [/weather/, () => ['Checking the weather', 'Checked the weather']],
  [/contacts|people/, () => ['Checking your contacts', 'Checked your contacts']],
  [/bank|plaid|finance|money/, () => ['Checking your accounts', 'Checked your accounts']],
];

/** [running phrase, past-tense phrase] for one activity. */
export function describe(a) {
  const name = String(a.name || '');
  const low = name.toLowerCase();
  const sum = a.summary || '';
  const memory = /memory|memories|notes?\b/i.test(sum);

  if (a.kind === 'web_search' || /^(websearch|web_search|google_web_search|search_web)$/.test(low)) {
    return sum ? [`Searching the web for “${cut(sum, 28)}”`, `Searched the web for “${cut(sum, 28)}”`] : ['Searching the web', 'Searched the web'];
  }
  if (/^(webfetch|web_fetch|fetch|read_url)$/.test(low)) {
    const h = host(sum);
    return h ? [`Reading ${h}`, `Read ${h}`] : ['Reading a page', 'Read a page'];
  }
  if (a.kind === 'mcp' || name.includes('/')) {
    const [server, tool = ''] = name.split('/');
    if (server === 'iris') return IRIS_TOOLS[tool] || ['Updating the app', 'Updated the app'];
    const key = `${server} ${tool}`.toLowerCase();
    for (const [re, f] of SERVICES) if (re.test(key)) return f(tool.toLowerCase());
    const t = tool.toLowerCase();
    const svc = title(server);
    if (/delete|remove|trash/.test(t)) return [`Deleting something in ${svc}`, `Deleted something in ${svc}`];
    if (/buy|pay|order|checkout|purchase/.test(t)) return [`Placing an order with ${svc}`, `Placed an order with ${svc}`];
    if (/send|reply|forward|post|publish|^message|sms|invite|share/.test(t)) return [`Sending a message with ${svc}`, `Sent a message with ${svc}`];
    if (/create|update|add|edit|set|move|write/.test(t)) return [`Updating ${svc}`, `Updated ${svc}`];
    return [`Checking ${svc}`, `Checked ${svc}`];
  }
  if (a.kind === 'file_change' || /^(edit|write|multiedit|notebookedit|write_file|replace|apply_patch)$/.test(low)) {
    return memory ? ['Writing that down', 'Wrote it down in my notes'] : ['Writing a file', 'Wrote a file'];
  }
  if (/^(read|read_file|read_many_files|glob|grep|ls|list_directory|search_file_content)$/.test(low)) {
    return memory ? ['Checking my notes', 'Checked my notes'] : ['Looking through files', 'Looked through files'];
  }
  if (/^(task|agent)$/.test(low)) return ['Thinking it through', 'Thought it through'];
  if (/todo|plan/.test(low)) return ['Making a plan', 'Made a plan'];
  if (a.kind === 'command' || /^(bash|shell|run_shell_command|exec|command)$/.test(low)) {
    if (/\b(curl|wget)\b/.test(sum)) {
      const h = host(sum);
      return h ? [`Fetching ${h}`, `Fetched ${h}`] : ['Fetching a page', 'Fetched a page'];
    }
    if (/\b(python3?|node|deno|ruby)\b/.test(sum)) return ['Running a quick script', 'Ran a quick script'];
    if (/\bgit\b/.test(sum)) return ['Checking the code', 'Checked the code'];
    return ['Working on my computer', 'Worked on my computer'];
  }
  return ['Working on it', 'Did a step'];
}

/** The shimmering line shown while a turn runs. */
export function statusPhrase(live) {
  if (!live) return 'Thinking…';
  if (live.queued) return 'Waiting for a free moment…';
  const acts = (live.acts || []).filter(visible);
  const running = [...acts].reverse().find((a) => a.status === 'started');
  if (running) return describe(running)[0] + '…';
  const last = acts.at(-1);
  if (!live.text && last && /^(Search|Read|Check|Look|Brows|Fetch)/.test(describe(last)[0])) return 'Thinking about what I found…';
  return 'Thinking…';
}

/** Receipt for a finished turn: [summary, steps[]] or null. */
export function receipt(meta) {
  const acts = (meta?.activity || []).filter(visible);
  if (!acts.length) return null;
  const steps = [];
  for (const a of acts) {
    const p = describe(a)[1] + (a.status === 'failed' ? ' (didn’t work)' : '');
    if (steps.at(-1) !== p) steps.push(p);
  }
  const summary = steps.length === 1 ? steps[0] : `${steps[0]} + ${steps.length - 1} more`;
  return [summary, steps];
}

export function seconds(ms) {
  if (!ms && ms !== 0) return '';
  const s = Math.max(1, Math.round(ms / 1000));
  return s < 90 ? `${s}s` : `${Math.round(s / 60)} min`;
}
