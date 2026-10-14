// `github.events` — the public GitHub events timeline (unauthenticated: 60 req/h). Mirrors adapters/github.rs.
import { Attrs, Emitter, Poller, RateWindow, SeenSet, getJson, js, nowSec, paramF64, paramSecs } from './util.js';

export const URL = 'https://api.github.com/events?per_page=100';

export function intensityFor(ty) {
  switch (ty) {
    case 'PushEvent': return 0.4;
    case 'PullRequestEvent': return 0.7;
    case 'PullRequestReviewEvent': case 'PullRequestReviewCommentEvent': return 0.5;
    case 'CreateEvent': return 0.5;
    case 'DeleteEvent': return 0.45;
    case 'WatchEvent': return 0.3;
    case 'ForkEvent': return 0.45;
    case 'IssuesEvent': return 0.6;
    case 'IssueCommentEvent': return 0.35;
    case 'ReleaseEvent': return 0.9;
    case 'PublicEvent': return 0.8;
    case 'MemberEvent': return 0.5;
    case 'GollumEvent': return 0.3;
    case 'CommitCommentEvent': return 0.35;
    default: return 0.4;
  }
}
export const eventAttrs = (e) => new Attrs().text('type', e.ty).text('repo', e.repo).text('actor', e.actor).text('action', e.action).text('id', e.id).intoMap();

/** Parse the events array (newest first). */
export function parseEvents(v) {
  if (!Array.isArray(v)) return [];
  const out = [];
  for (const e of v) {
    const id = js(e, 'id');
    if (id === null) continue;
    out.push({ id, ty: js(e, 'type') ?? '', repo: js(e?.repo, 'name') ?? '', actor: js(e?.actor, 'login') ?? '', action: js(e?.payload, 'action') ?? '' });
  }
  return out;
}

export default class Adapter {
  constructor(spec, emit, log) {
    this.interval = paramSecs(spec, 'interval', 60, 60);
    const initial = paramF64(spec, 'initial', 10);
    if (!(initial >= 0 && initial <= 100)) throw new Error(`github.events: param \`initial\`: expected a number in 0..=100, got ${initial}`);
    this.initial = Math.trunc(initial);
    this.em = new Emitter(emit, log);
    this.seen = new SeenSet(10000);
    this.first = true;
    this.window = new RateWindow();
  }
  start() { this.poller = new Poller(this.em, this.interval); this.poller.run((s) => this.pollOnce(s)); }
  stop() { this.poller?.stop(); this.em.close(); }
  async pollOnce(signal) {
    const v = await getJson(URL, { signal, headers: { Accept: 'application/vnd.github+json' } });
    if (v && !Array.isArray(v) && typeof v.message === 'string') throw `GitHub: ${v.message}`;
    const events = parseEvents(v).reverse(); // oldest first
    let fresh = events.filter((e) => this.seen.insert(e.id));
    if (this.first) { fresh = fresh.slice(fresh.length - Math.min(this.initial, fresh.length)); this.first = false; }
    const now = nowSec();
    for (const e of fresh) { this.window.push(now); this.em.event('event', intensityFor(e.ty), eventAttrs(e)); }
    this.em.reading([['rate', this.window.rate(now, 300)], ['new_events', fresh.length]]);
    return fresh.length ? 'live' : 'idle';
  }
}
