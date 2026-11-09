// Arr web app. No framework, no build. All text goes through textContent / DOM APIs.
const $ = (s) => document.querySelector(s);
const view = $('#view'), input = $('#q'), banner = $('#banner'), live = $('#live'), tabs = $('#tabs'), top = $('#top');
const S = { feed: null, ideas: null, ideasAt: 0, out: false, all: false, kind: 'movies', filter: '' };
const A = new Map(); // add / retry state by id, kept for the session
const reg = new Map(); // id -> painters that must repaint when A changes
const fills = new Map();
let token = 0, pollT = 0, retryT = 0, feedBusy = false, lastKey = '', searchSeq = 0, searchAbort = null, debounceT = 0;

const ICONS = {
  search: 'M11 4a7 7 0 1 0 0 14a7 7 0 0 0 0-14zM20 20l-4-4',
  check: 'M5 12.5l4.5 4.5L19 7.5',
  retry: 'M20 12a8 8 0 1 1-2.5-5.8M20 4v4.5h-4.5',
  ext: 'M14 4h6v6M20 4l-9 9M18 14v5a1 1 0 0 1-1 1H5a1 1 0 0 1-1-1V7a1 1 0 0 1 1-1h5',
  home: 'M4 11l8-7l8 7v8a1 1 0 0 1-1 1h-4v-6h-6v6H5a1 1 0 0 1-1-1z',
  ideas: 'M12 3l2.2 5.8L20 11l-5.8 2.2L12 19l-2.2-5.8L4 11l5.8-2.2z',
  library: 'M4 5h4v14H4zM10 5h4v14h-4zM16.5 5.5l3.7 1l-3.2 13l-3.7-1z',
  plus: 'M12 5v14M5 12h14',
  pause: 'M9 5v14M15 5v14',
  alert: 'M12 4a8 8 0 1 0 0 16a8 8 0 0 0 0-16zM12 8v5M12 16.5v.01',
  close: 'M6 6l12 12M18 6L6 18',
  down: 'M6 9l6 6l6-6',
  right: 'M9 6l6 6l-6 6',
};
const NS = 'http://www.w3.org/2000/svg';
function ic(name) {
  const s = document.createElementNS(NS, 'svg'), p = document.createElementNS(NS, 'path');
  s.setAttribute('viewBox', '0 0 24 24'); s.setAttribute('class', 'ic'); s.setAttribute('aria-hidden', 'true');
  p.setAttribute('d', ICONS[name]); s.append(p);
  return s;
}
function h(tag, props, ...kids) {
  const e = document.createElement(tag);
  for (const [k, v] of Object.entries(props || {})) {
    if (v == null || v === false) continue;
    if (k === 'class') e.className = v;
    else if (k.startsWith('on')) e.addEventListener(k.slice(2), v);
    else e.setAttribute(k, v === true ? '' : v);
  }
  for (const k of kids.flat()) if (k != null && k !== false) e.append(k.nodeType ? k : String(k));
  return e;
}
const say = (m) => { live.textContent = ''; setTimeout(() => { live.textContent = m; }, 50); };

// ---------- network ----------
async function api(path, { method = 'GET', body, signal } = {}) {
  let r;
  try {
    r = await fetch('/app/api' + path, {
      method, signal, credentials: 'same-origin',
      headers: { 'X-Arr-App': '1', ...(body ? { 'Content-Type': 'application/json' } : {}) },
      body: body ? JSON.stringify(body) : undefined,
    });
  } catch (e) {
    if (e.name !== 'AbortError') banner.textContent = "Can't reach Arr right now. Trying again…";
    throw e;
  }
  let data = null;
  try { data = await r.json(); } catch { /* empty body */ }
  if (r.status === 401) { showSignin(data); throw new Error('signed out'); }
  const code = data && data.error && data.error.code;
  if (code === 'catalog_not_imported') { banner.textContent = "Arr is still being set up. Check back soon."; throw new Error(code); }
  // Only a reply that isn't Arr's own error (a proxy or tunnel failing) means Arr is out of reach.
  if (r.status >= 500 && !code) { banner.textContent = "Can't reach Arr right now. Trying again…"; throw new Error('unreachable'); }
  banner.textContent = '';
  return { ok: r.ok, status: r.status, data };
}

// ---------- small helpers ----------
const plexUrl = (t) => 'https://app.plex.tv/desktop/#!/search?query=' + encodeURIComponent(t);
// The title tile is the placeholder; a poster, when there is one, fades in over it.
function tile(title, mediaId) {
  const words = String(title).split(/\s+/).filter((w) => /[\p{L}\p{N}]/u.test(w));
  const sig = words.filter((w) => !/^(the|a|an)$/i.test(w));
  const use = sig.length ? sig : words;
  const ch = (w) => [...(w || '').replace(/[^\p{L}\p{N}]/gu, '')];
  const s = use.length > 1 ? (ch(use[0])[0] || '') + (ch(use[1])[0] || '') : ch(use[0]).slice(0, 2).join('');
  let n = 0;
  for (const c of String(title)) n = (n * 31 + c.codePointAt(0)) >>> 0;
  const t = h('span', { class: 'tile t' + (n % 8), 'aria-hidden': 'true' }, (s || '?').toUpperCase());
  if (mediaId) {
    const img = h('img', { src: '/app/art/' + encodeURIComponent(mediaId), alt: '', loading: 'lazy', decoding: 'async' });
    img.addEventListener('load', () => img.classList.add('on'));
    img.addEventListener('error', () => img.remove());
    t.append(img);
  }
  return t;
}
function parseDate(s) {
  if (/^\d{4}-\d\d-\d\d$/.test(s)) { const [y, m, d] = s.split('-').map(Number); return new Date(y, m - 1, d); }
  return new Date(s);
}
const fmtDay = (d) => d.toLocaleDateString(undefined, { day: 'numeric', month: 'short' });
function chip(d, mode) {
  const top = mode === 'month' ? d.toLocaleDateString(undefined, { month: 'short' }) : d.toLocaleDateString(undefined, { weekday: 'short' });
  return h('time', { class: 'date', datetime: d.toISOString().slice(0, 10) }, top, h('b', null, d.getDate()));
}
function ago(iso) {
  const days = Math.floor((new Date().setHours(0, 0, 0, 0) - new Date(iso).setHours(0, 0, 0, 0)) / 864e5);
  return days <= 0 ? 'today' : days === 1 ? 'yesterday' : days + ' days ago';
}
const meta = (it) => it.detail || (it.year ? String(it.year) : '');
const joinDot = (...p) => p.filter(Boolean).join(' · ');
const plexLink = (title) => h('a', { class: 'btn', href: plexUrl(title), target: '_blank', rel: 'noopener noreferrer', 'aria-label': `Open ${title} in Plex` }, ic('ext'), 'Watch');
const section = (id, title, body, extra) => h('section', { 'aria-labelledby': 'h-' + id }, h('h2', { id: 'h-' + id }, title, extra), body);
const list = (rows) => h('ul', { class: 'list' }, rows);
const hint = (t, role) => h('p', { class: 'hint', role }, t);

function mount(node) {
  const fk = view.contains(document.activeElement) && document.activeElement.dataset.fk;
  view.replaceChildren(node);
  if (fk) view.querySelector(`[data-fk="${CSS.escape(fk)}"]`)?.focus({ preventScroll: true });
}
function track(id, el, fn) {
  let set = reg.get(id);
  if (!set) reg.set(id, (set = new Set()));
  set.add({ el, fn });
  fn();
}
function repaint(id) {
  for (const t of reg.get(id) || []) {
    if (t.el.isConnected) t.fn(); else reg.get(id).delete(t);
  }
}

// ---------- add / retry controls ----------
function addControl(it, withMsg = true) {
  const btn = h('button', { type: 'button', class: 'btn', 'data-fk': 'add:' + it.id });
  const msg = withMsg ? h('p', { class: 'msg' }) : null;
  const cur = () => { const st = A.get(it.id) || {}; return st.busy ? 'busy' : st.state || it.base; };
  track(it.id, btn, () => {
    const st = A.get(it.id) || {}, s = cur();
    const L = { add: ['plus', 'Add'], busy: [null, 'Adding…'], added: ['check', 'Added'], following: ['check', 'Following'], inplex: ['check', 'In Plex'] }[s];
    btn.dataset.s = s;
    btn.setAttribute('aria-disabled', s === 'add' ? 'false' : 'true');
    btn.setAttribute('aria-label', s === 'add' ? `Add ${it.title}` : `${L[1]}, ${it.title}`);
    btn.replaceChildren(...(L[0] ? [ic(L[0])] : []), L[1]);
    if (msg) { msg.textContent = st.msg || ''; if (st.err) msg.dataset.err = ''; else delete msg.dataset.err; }
  });
  btn.addEventListener('click', async () => {
    if (cur() !== 'add') return;
    A.set(it.id, { busy: true }); repaint(it.id);
    let next;
    try {
      const r = await api('/v1/quick-add', { method: 'POST', body: { media_id: it.id } });
      if (r.ok && r.data) {
        const o = r.data.outcome;
        next = { msg: r.data.message || 'Added.', state: o === 'in_plex' ? 'inplex' : o === 'following' ? 'following' : o === 'movies_off' ? undefined : 'added', err: o === 'movies_off' };
      } else next = { msg: "Couldn't add this one. Try again in a moment.", err: true };
    } catch { next = { msg: "Couldn't reach Arr. Try again in a moment.", err: true }; }
    A.set(it.id, next); repaint(it.id); say(next.msg);
    if (!next.err) loadFeed();
  });
  return [btn, msg];
}
function retryControl(it) {
  // Keyed by when it went wrong too, so a later failure of the same item can be retried again.
  const id = 'r:' + it.key + '|' + it.at;
  const btn = h('button', { type: 'button', class: 'btn coral', 'data-fk': id });
  const msg = h('p', { class: 'msg' });
  track(id, btn, () => {
    const st = A.get(id) || {}, off = st.busy || st.state;
    btn.setAttribute('aria-disabled', off ? 'true' : 'false');
    btn.replaceChildren(ic('retry'), h('span', { class: 'lbl' }, st.busy ? 'Trying…' : st.state ? 'Trying again' : 'Try again'));
    btn.setAttribute('aria-label', `Try again, ${it.title}`);
    msg.textContent = st.msg || ''; if (st.err) msg.dataset.err = ''; else delete msg.dataset.err;
  });
  btn.addEventListener('click', async () => {
    const st = A.get(id) || {};
    if (st.busy || st.state) return;
    A.set(id, { busy: true }); repaint(id);
    let next;
    try {
      const r = it.grab_id
        ? await api(`/v1/queue/${encodeURIComponent(it.grab_id)}/retry`, { method: 'POST' })
        : await api('/v1/quick-add', { method: 'POST', body: { media_id: it.target || it.media_id } });
      next = r.ok ? { state: 'again', msg: 'Trying again. Check back in a minute.' } : { msg: "Couldn't try again just now. Give it a moment.", err: true };
    } catch { next = { msg: "Couldn't reach Arr. Try again in a moment.", err: true }; }
    A.set(id, next); repaint(id); say(next.msg);
    if (!next.err) loadFeed();
  });
  return [btn, msg];
}

// ---------- rows ----------
function needRow(it) {
  const [btn, msg] = retryControl(it);
  return h('li', { class: 'item need' }, tile(it.title, it.media_id),
    h('div', { class: 'what' }, h('h3', { title: it.title }, stretch(it.title, it.media_id)),
      meta(it) && h('p', { class: 'meta' }, meta(it)),
      h('p', { class: 'problem' }, ic('alert'), h('span', null, it.problem)), msg), btn);
}
function arrivedRow(it) {
  return h('li', { class: 'item arrived' }, tile(it.title, it.media_id),
    h('div', { class: 'what' }, h('span', { class: 'tag' }, ic('check'), 'In Plex'),
      h('h3', { title: it.title }, stretch(it.title, it.media_id)), h('p', { class: 'meta' }, joinDot(it.detail, ago(it.at)))),
    plexLink(it.title));
}
const STAGES = ['Found', 'Downloading', 'Checking', 'In Plex'];
function wayRow(it) {
  const stage = Math.max(0, ['found', 'downloading', 'checking'].indexOf(it.stage));
  const p = typeof it.progress === 'number' ? Math.min(1, Math.max(0, it.progress)) : null;
  const target = stage === 1 ? (1 + (p ?? 0)) / 3 : stage / 3;
  const fill = h('i', { class: 'fill' });
  fill.style.setProperty('--f', fills.get(it.grab_id) ?? 0);
  pendingFills.push([fill, target, it.grab_id]);
  const status = it.paused ? 'Paused' : stage === 0 ? 'Found, getting ready' : stage === 1 ? (p == null ? 'Downloading' : `${Math.round(p * 100)}% downloaded`) : 'Checking the file';
  return h('li', { class: 'item' }, tile(it.title, it.media_id),
    h('div', { class: 'what' }, h('h3', { title: it.title }, stretch(it.title, it.media_id)),
      h('p', { class: 'meta status' }, it.paused && ic('pause'), joinDot(meta(it), status)),
      h('ol', { class: 'journey' + (it.paused ? ' paused' : ''), 'aria-label': 'Progress' }, fill,
        STAGES.map((s, i) => h('li', { class: i <= stage ? 'on' : '', 'aria-current': i === stage ? 'step' : null }, s)))));
}
let pendingFills = [];
function lookRow(it) {
  const until = it.until ? parseDate(it.until) : null;
  return h('li', { class: 'item' }, until ? chip(until, 'month') : tile(it.title, it.media_id),
    h('div', { class: 'what' }, h('h3', { title: it.title }, stretch(it.title, it.media_id)),
      h('p', { class: 'meta' }, joinDot(meta(it), until ? `Out ${fmtDay(until)}. Arr will get it then.` : 'Arr is looking for a copy'))));
}
function soonRow(it) {
  return h('li', { class: 'item' }, chip(new Date(it.at), 'day'),
    h('div', { class: 'what' }, h('h3', { title: it.title }, stretch(it.title, it.media_id)), h('p', { class: 'meta' }, it.detail || 'New episode')));
}
// Ideas are shows: the button opens the show page, where the person picks what to get.
function chooseLink(x) {
  return h('a', { class: 'btn' + (x.in_library ? ' ghost' : ''), href: titleHref(x.media_id), 'aria-label': `${x.in_library ? 'Open' : 'Choose'} ${x.title}` }, x.in_library ? 'Open' : 'Choose', ic('right'));
}
function ideaCard(x) {
  return h('li', { class: 'idea' }, tile(x.title, x.media_id), h('h3', { title: x.title }, stretch(x.title, x.media_id)),
    h('p', { class: 'meta' }, x.network || x.year || ''), chooseLink(x));
}
function ideaRow(x) {
  return h('li', { class: 'item top-align' }, tile(x.title, x.media_id),
    h('div', { class: 'what' }, h('h3', { title: x.title }, stretch(x.title, x.media_id)),
      h('p', { class: 'meta' }, joinDot(x.year, x.network)), x.summary && h('p', { class: 'clamp' }, x.summary)), chooseLink(x));
}

// ---------- home ----------
function homeNode() {
  const f = S.feed, root = h('div', { class: 'stack' });
  pendingFills = [];
  const n = (k) => (f[k] || []);
  const secs = [];
  if (n('needs_you').length) secs.push(section('need', 'Needs you', list(n('needs_you').map(needRow))));
  if (n('arrived').length) secs.push(section('arrived', 'Arrived', list(n('arrived').map(arrivedRow))));
  if (n('on_its_way').length) {
    const all = n('on_its_way'), shown = S.all ? all : all.slice(0, 3);
    const more = all.length > 3 && h('button', { type: 'button', class: 'link', 'aria-expanded': String(S.all), 'data-fk': 'all', onclick: () => { S.all = !S.all; paintHome(true); } }, S.all ? 'Show fewer' : `See all ${all.length}`);
    secs.push(section('way', 'On its way', list(shown.map(wayRow)), more));
  }
  if (n('looking').length) secs.push(section('look', 'Looking', list(n('looking').map(lookRow))));
  if (n('coming_up').length) secs.push(section('soon', 'Coming up', list(n('coming_up').map(soonRow))));
  const ideas = ((S.ideas && S.ideas.items) || []).filter((x) => !x.in_library).slice(0, 6);
  if (!secs.length) {
    secs.push(h('section', { class: 'empty', 'aria-labelledby': 'h-empty' }, h('img', { src: '/app/mascot.svg', alt: '' }),
      h('h2', { id: 'h-empty' }, 'Find something to watch'), h('p', null, ideas.length ? 'Search above for a movie or show, or start with one of these.' : 'Search above for a movie or show.')));
  }
  if (ideas.length) secs.push(section('ideas', 'Ideas for you', h('ul', { class: 'ideas-row' }, ideas.map(ideaCard)), h('a', { href: '#/ideas' }, 'See all')));
  root.append(...secs);
  return root;
}
function paintHome(force) {
  if (!S.feed) return;
  const { generated_at, ...rest } = S.feed;
  const key = JSON.stringify([rest, S.ideas && S.ideas.items, S.all]);
  if (!force && key === lastKey) return;
  lastKey = key;
  const node = homeNode();
  mount(node);
  const fl = pendingFills;
  requestAnimationFrame(() => requestAnimationFrame(() => {
    for (const [el, t, id] of fl) { el.style.setProperty('--f', t); if (id) fills.set(id, t); }
  }));
}
const onHome = () => parseHash().path === '/';
async function loadFeed() {
  if (S.out || feedBusy) return;
  feedBusy = true;
  try {
    const r = await api('/v1/feed');
    if (r.ok) { S.feed = r.data; $('#tmdb-credit').hidden = !r.data.tmdb_backup; if (onHome()) paintHome(); }
    return r.ok;
  } catch { return false; } finally {
    feedBusy = false;
    clearTimeout(pollT);
    if (!document.hidden && onHome() && !S.out) pollT = setTimeout(loadFeed, 5000);
  }
}
async function loadIdeas() {
  if (S.ideas && Date.now() - S.ideasAt < 6e5) return S.ideas;
  try { const r = await api('/v1/ideas'); if (r.ok) { S.ideas = r.data; S.ideasAt = Date.now(); } } catch { /* optional */ }
  return S.ideas;
}
function failed(t) {
  mount(hint('Your list will show up here as soon as Arr is back.'));
  clearTimeout(retryT);
  retryT = setTimeout(() => { if (t === token) route(); }, 5000);
}
async function renderHome(t) {
  lastKey = '';
  if (S.feed) paintHome(); else mount(hint('Loading…'));
  const ok = await loadFeed();
  if (t !== token) return;
  if (!ok && !S.feed) return failed(t);
  const had = S.ideas;
  await loadIdeas();
  if (t === token && S.ideas !== had) paintHome();
}

// ---------- ideas ----------
async function renderIdeas(t) {
  mount(hint('Loading…'));
  const d = await loadIdeas();
  if (t !== token) return;
  if (!d) return failed(t);
  const note = d.status === 'disabled' ? "Ideas aren't switched on for this Arr." : d.status === 'unavailable' && !d.items.length ? "Ideas aren't available right now. Check back soon." : !d.items.length ? 'No ideas yet. Check back soon.' : null;
  mount(section('ideas', 'Popular this week', note ? hint(note) : list(d.items.map(ideaRow))));
}

// ---------- search ----------
async function renderSearch(q, t) {
  q = q.trim();
  if (q.length < 2) return mount(hint('Type at least two letters to search.'));
  const seq = ++searchSeq;
  searchAbort?.abort();
  const ac = (searchAbort = new AbortController());
  const prev = view.querySelector('[data-results]');
  if (prev) prev.classList.add('busy'); else mount(hint('Searching…', 'status'));
  const get = (kind) => api(`/v1/discover?kind=${kind}&q=${encodeURIComponent(q)}`, { signal: ac.signal });
  const [m, s] = await Promise.allSettled([get('movie'), get('series')]);
  if (seq !== searchSeq || t !== token) return;
  const ok = (r) => r.status === 'fulfilled' && r.value.ok;
  if (!ok(m) && !ok(s)) { mount(hint('Search will work again as soon as Arr is back.', 'status')); return; }
  const movies = ok(m) ? m.value.data.results || [] : [];
  const shows = ok(s) ? s.value.data.results || [] : [];
  const moviesOff = (S.feed && S.feed.movies_enabled === false) || (ok(m) && (m.value.data.providers || []).some((p) => p.name === 'plex' && p.status === 'not_configured'));
  const rows = [];
  for (let i = 0; i < Math.max(movies.length, shows.length); i++) {
    if (movies[i]) rows.push(searchRow(movies[i], 'Movie'));
    if (shows[i]) rows.push(searchRow(shows[i], 'Show'));
  }
  const notes = [];
  if (moviesOff) notes.push(hint("Movie search isn't set up yet."));
  else if (!ok(m)) notes.push(hint("Movie results aren't available right now."));
  if (!ok(s)) notes.push(hint("Show results aren't available right now."));
  const wrap = h('div', { 'data-results': '' }, section('res', 'Results', rows.length ? list(rows) : hint(`Nothing found for “${q}”. Check the spelling or try fewer words.`)), notes);
  mount(wrap);
}
function searchRow(x, label) {
  // A movie is one tap; a show opens its page to pick what to get.
  if (label === 'Show') {
    return h('li', { class: 'item top-align' }, tile(x.title, x.media_id),
      h('div', { class: 'what' }, h('h3', { title: x.title }, stretch(x.title, x.media_id)), h('p', { class: 'meta' }, joinDot(x.year, label))),
      chooseLink({ media_id: x.media_id, title: x.title, in_library: x.in_catalog && x.monitored }));
  }
  const base = x.has_file ? 'inplex' : x.in_catalog && x.monitored ? 'added' : 'add';
  const [btn, msg] = addControl({ id: x.media_id, title: x.title, base });
  return h('li', { class: 'item top-align' }, tile(x.title, x.media_id),
    h('div', { class: 'what' }, h('h3', { title: x.title }, stretch(x.title, x.media_id)), h('p', { class: 'meta' }, joinDot(x.year, label)), msg), btn);
}

// ---------- library ----------
function renderLibrary(t) {
  const listEl = h('ul', { class: 'list' }), more = h('div', { class: 'more' }), status = h('p', { class: 'hint', role: 'status' });
  let items = [], total = 0, seq = 0;
  const filter = h('input', { type: 'search', class: 'filter', placeholder: 'Filter your library', 'aria-label': 'Filter your library', autocomplete: 'off', value: S.filter });
  const seg = h('div', { class: 'seg', role: 'group', 'aria-label': 'What to show' });
  const draw = () => {
    seg.replaceChildren(...[['movies', 'Movies'], ['series', 'Shows']].map(([k, l]) =>
      h('button', { type: 'button', 'aria-pressed': String(S.kind === k), onclick: () => { S.kind = k; draw(); load(true); } }, l)));
  };
  const rowFor = (x) => {
    const mv = S.kind === 'movies', title = x.title || 'Untitled';
    const mid = mv ? (x.tmdb_id && `movie:tmdb:${x.tmdb_id}`) : x.tvdb_id ? `series:tvdb:${x.tvdb_id}` : x.tvmaze_id ? `series:tvmaze:${x.tvmaze_id}` : null;
    return h('li', { class: 'item' }, tile(title, mid), h('div', { class: 'what' }, h('h3', { title }, stretch(title, mid)),
      h('p', { class: 'meta' }, joinDot(x.year, mv ? 'Movie' : 'Show', mv && x.has_file === false && 'Not in Plex yet'))),
      !(mv && x.has_file === false) && plexLink(title));
  };
  async function load(reset) {
    const my = ++seq;
    if (reset) { items = []; total = 0; listEl.replaceChildren(); more.replaceChildren(); status.textContent = 'Loading…'; }
    const qs = `limit=50&offset=${items.length}` + (S.filter.trim() ? '&q=' + encodeURIComponent(S.filter.trim()) : '');
    try {
      const r = await api(`/v1/library/${S.kind}?${qs}`);
      if (my !== seq || t !== token) return;
      if (!r.ok) { status.textContent = "Couldn't load the library just now."; return; }
      items = items.concat(r.data.items || []); total = r.data.total || 0;
      listEl.append(...(r.data.items || []).map(rowFor));
      status.textContent = items.length ? '' : S.filter.trim() ? `No matches for “${S.filter.trim()}”.` : 'Nothing here yet. Add something from the search above.';
      more.replaceChildren(...(items.length < total ? [h('button', { type: 'button', class: 'btn ghost', onclick: () => load(false) }, 'Show more'), h('span', null, `Showing ${items.length} of ${total}`)] : []));
    } catch { if (my === seq && t === token && !S.out) { status.textContent = ''; if (!items.length) failed(t); } }
  }
  let ft = 0;
  filter.addEventListener('input', () => { clearTimeout(ft); ft = setTimeout(() => { S.filter = filter.value; load(true); }, 250); });
  draw();
  mount(section('lib', 'Library', h('div', null, h('div', { class: 'bar' }, seg, filter), status, listEl, more)));
  load(true);
}

// ---------- title pages (a show or a movie, as a sheet over the current screen) ----------
const sheet = $('#sheet'), panel = $('#panel');
const P = { id: '', kind: '', token: 0, seq: 0, data: null, err: '', seasons: new Map(), open: new Set(), ask: '', busy: new Set(), msg: new Map(), more: false, followBusy: false, pollT: 0, askT: 0, opener: null, scrollY: 0, canBack: false, opt: null };
let bgHash = '', sheetFills = [];
const sheetOpen = () => !sheet.hidden;
// Everything behind the sheet: out of reach of the keyboard and of assistive tech while it is open.
const behind = () => [$('.skip'), top, view, $('#foot')];
const enc = encodeURIComponent;
const pct = (p) => Math.round(p * 100);
const fmtDate = (s) => {
  const d = parseDate(s);
  return d.toLocaleDateString(undefined, { day: 'numeric', month: 'short', ...(d.getFullYear() !== new Date().getFullYear() ? { year: 'numeric' } : {}) });
};
const fmtRuntime = (m) => (m >= 60 ? `${Math.floor(m / 60)}h${m % 60 ? ` ${m % 60}m` : ''}` : `${m}m`);
const plural = (n, one, many) => `${n} ${n === 1 ? one : many}`;

function titleRoute(path) {
  const m = /^\/(show|movie)\/(.+)$/.exec(path);
  if (!m) return null;
  let id;
  try { id = decodeURIComponent(m[2]); } catch { return null; }
  const ok = m[1] === 'show' ? /^series:(tvdb|tvmaze):\d+$/.test(id) : /^movie:tmdb:\d+$/.test(id);
  return ok ? { kind: m[1], id } : null;
}
// The page a media id of any kind belongs to: a movie's, or the show of a season or episode.
function titleHref(id) {
  if (/^movie:tmdb:\d+$/.test(id || '')) return '#/movie/' + id;
  const m = /^(?:series|season|episode):(tvdb|tvmaze):(\d+)/.exec(id || '');
  return m ? `#/show/series:${m[1]}:${m[2]}` : null;
}
// A title that opens its page; the link covers the whole row (see .stretch).
function stretch(text, id) {
  const href = titleHref(id);
  return href ? h('a', { class: 'stretch', href, 'data-fk': 'open:' + href }, text) : text;
}
document.addEventListener('click', (e) => {
  const a = e.target.closest && e.target.closest('a[href^="#/show/"],a[href^="#/movie/"]');
  if (a) P.opener = a;
}, true);

function openTitle(t) {
  if (sheetOpen() && P.id === t.id) return;
  const first = !sheetOpen();
  if (first) {
    P.canBack = !!bgHash; // came from a screen of the app: closing goes back to it
    if (!bgHash) renderPath('/', '', '#/'); // a link straight to a title: home sits underneath
    P.scrollY = window.scrollY;
    sheet.hidden = false;
    document.body.classList.add('sheet-open');
    for (const el of behind()) el.inert = true;
  }
  clearTimeout(P.pollT); clearTimeout(P.askT);
  Object.assign(P, { id: t.id, kind: t.kind, token: P.token + 1, data: null, err: '', seasons: new Map(), open: new Set(), ask: '', busy: new Set(), msg: new Map(), more: false, followBusy: false, opt: freshOpt() });
  panel.scrollTop = 0;
  paintSheet();
  if (first) panel.querySelector('.close')?.focus({ preventScroll: true });
  loadTitle(false);
}
function closeSheet() {
  if (!sheetOpen()) return;
  clearTimeout(P.pollT); clearTimeout(P.askT);
  P.token++;
  sheet.hidden = true;
  document.body.classList.remove('sheet-open');
  for (const el of behind()) el.inert = false;
  panel.replaceChildren();
  document.title = pageName(parseHash(bgHash).path) + ' · Arr';
  window.scrollTo(0, P.scrollY);
  const o = P.opener, fk = o && o.dataset.fk;
  const back = (o && o.isConnected && o) || (fk && view.querySelector(`[data-fk="${CSS.escape(fk)}"]`)) || view;
  back.focus({ preventScroll: true });
}
// The person closed it: go back to where they were, or home if the link was the way in.
function dismiss() {
  if (P.canBack) history.back();
  else { history.replaceState(null, '', bgHash || '#/'); route(); }
}
document.addEventListener('keydown', (e) => {
  if (e.key !== 'Escape' || !sheetOpen()) return;
  e.preventDefault();
  if (P.ask) { P.ask = ''; paintSheet(); } else dismiss();
});
sheet.addEventListener('click', (e) => { if (e.target === sheet) dismiss(); });
// Tab and Shift-Tab stay inside the sheet.
document.addEventListener('keydown', (e) => {
  if (e.key !== 'Tab' || !sheetOpen()) return;
  const f = [...panel.querySelectorAll('a[href],button:not([disabled]),input,select,textarea,[tabindex]:not([tabindex="-1"])')].filter((x) => !x.hidden && x.getClientRects().length);
  const a = document.activeElement;
  if (!f.length) { e.preventDefault(); panel.focus(); return; }
  if (!panel.contains(a)) { e.preventDefault(); f[0].focus(); }
  else if (e.shiftKey && a === f[0]) { e.preventDefault(); f[f.length - 1].focus(); }
  else if (!e.shiftKey && a === f[f.length - 1]) { e.preventDefault(); f[0].focus(); }
});

function errText(r) {
  if (P.kind === 'movie') return r && r.status === 404 ? (S.feed && S.feed.movies_enabled === false ? "Movie details need movie search, which isn't set up yet." : "Arr doesn't have details for this movie right now.") : "Couldn't load this movie just now.";
  return r && r.status === 404 ? "Arr doesn't have an episode list for this show right now." : "Couldn't load this show just now.";
}
// Reads are numbered: a newer read, or the start of an action, makes an older answer stale.
async function loadTitle(quiet) {
  const tk = P.token, sq = ++P.seq, stale = () => tk !== P.token || sq !== P.seq;
  // Expanded seasons reload on their own, so a failed title read can't leave one loading forever.
  const seasons = P.kind === 'show' ? Promise.all([...P.open].map((n) => loadSeason(n, tk, sq))) : null;
  try {
    const r = await api(`/v1/${P.kind}/${enc(P.id)}`);
    if (stale()) return;
    if (!r.ok) { if (!P.data) P.err = errText(r); await seasons; if (!stale()) paintSheet(); return; }
    P.data = r.data; P.err = '';
    document.title = `${r.data.title} · Arr`;
  } catch {
    if (stale()) return;
    if (!P.data) P.err = "Couldn't reach Arr just now.";
  }
  await seasons;
  if (stale()) return;
  paintSheet();
  schedulePoll();
}
async function loadSeason(n, tk, sq) {
  const cur = P.seasons.get(n) || {};
  try {
    const r = await api(`/v1/show/${enc(P.id)}/season/${n}`);
    if (tk !== P.token || sq !== P.seq) return;
    P.seasons.set(n, r.ok ? { eps: r.data.episodes } : { ...cur, loading: false, err: "Couldn't load the episodes." });
  } catch {
    if (tk === P.token && sq === P.seq) P.seasons.set(n, { ...cur, loading: false, err: "Couldn't load the episodes." });
  }
}
function schedulePoll() {
  clearTimeout(P.pollT);
  if (!sheetOpen() || document.hidden || !P.data) return;
  const moving = P.kind === 'movie' ? P.data.state === 'on_its_way' : P.data.seasons.some((s) => s.state === 'on_its_way');
  if (!moving) return;
  const tk = P.token;
  P.pollT = setTimeout(() => {
    if (tk !== P.token) return;
    if (P.busy.size || P.followBusy) schedulePoll(); else loadTitle(true);
  }, 5000);
}

function paintSheet() {
  if (!sheetOpen()) return;
  const fk = panel.contains(document.activeElement) && document.activeElement.dataset.fk;
  const scroll = panel.scrollTop;
  sheetFills = [];
  const body = P.data ? (P.kind === 'show' ? showBody() : movieBody()) : P.err ? errorBody() : skeleton();
  const close = h('button', { type: 'button', class: 'btn ghost close', 'aria-label': 'Close', 'data-fk': 'close', onclick: dismiss }, ic('close'));
  panel.replaceChildren(h('div', { class: 'sheet-bar' }, close), h('div', { class: 'sheet-body' }, body));
  panel.scrollTop = scroll;
  if (fk) panel.querySelector(`[data-fk="${CSS.escape(fk)}"]`)?.focus({ preventScroll: true });
  const fl = sheetFills, sum = panel.querySelector('.sum.cl'), more = panel.querySelector('.more-sum');
  requestAnimationFrame(() => {
    if (sum && more && sum.scrollHeight > sum.clientHeight + 1) more.hidden = false;
    requestAnimationFrame(() => { for (const [el, t, id] of fl) { el.style.setProperty('--f', t); fills.set(id, t); } });
  });
}
function skeleton() {
  const bar = (w) => h('div', { class: 'sk', style: `width:${w}` });
  return h('div', { class: 'stack', 'aria-busy': 'true' },
    h('span', { id: 'sheet-title', class: 'sr' }, 'Loading'),
    h('div', { class: 'hero' }, h('div', { class: 'tile big sk-tile' }), h('div', { class: 'hero-t' }, bar('80%'), bar('50%'))),
    bar('100%'), bar('70%'),
    h('ul', { class: 'list' }, [1, 2, 3].map(() => h('li', { class: 'item' }, h('div', { class: 'what' }, bar('40%'), h('div', { class: 'sk', style: 'width:65%;margin-top:8px' }))))));
}
function errorBody() {
  return h('div', { class: 'stack' }, h('span', { id: 'sheet-title', class: 'sr' }, 'Something went wrong'),
    hint(P.err, 'status'),
    h('button', { type: 'button', class: 'btn ghost', 'data-fk': 'retry', onclick: () => { P.err = ''; paintSheet(); loadTitle(false); } }, ic('retry'), 'Try again'));
}
function hero(title, line) {
  const t = tile(title, P.data.media_id);
  t.classList.add('big');
  return h('header', { class: 'hero' }, t, h('div', { class: 'hero-t' }, h('h1', { id: 'sheet-title' }, title), line && h('p', { class: 'meta' }, line)));
}
function summary(text) {
  if (!text) return null;
  return h('div', null,
    h('p', { class: 'sum' + (P.more ? '' : ' cl') }, text),
    h('button', { type: 'button', class: 'link more-sum', 'aria-expanded': String(P.more), 'data-fk': 'more', hidden: !P.more, onclick: () => { P.more = !P.more; paintSheet(); } }, P.more ? 'Less' : 'More'));
}
function msgFor(key) {
  const m = P.msg.get(key);
  const p = h('p', { class: 'msg' }, m ? m.text : '');
  if (m && m.err) p.dataset.err = '';
  return p;
}

// One action: Get, or Stop (which asks first). `key` identifies it on the page.
function actionNode(key, kind, label, noun, scope) {
  const busy = P.busy.has(key);
  if (kind === 'stop' && P.ask === key && !busy) {
    return h('span', { class: 'ask' }, h('span', { class: 'ask-t' }, 'Stop getting it?'),
      h('button', { type: 'button', class: 'btn', 'data-fk': 'act:' + key, 'aria-label': `Confirm: stop getting ${noun}`, onclick: () => act(key, 'stop', scope) }, 'Confirm'),
      h('button', { type: 'button', class: 'btn ghost', 'aria-label': `Keep getting ${noun}`, onclick: () => { P.ask = ''; paintSheet(); } }, 'Keep'));
  }
  const text = busy ? (kind === 'get' ? 'Getting…' : 'Stopping…') : label;
  return h('button', {
    type: 'button', class: 'btn' + (kind === 'stop' ? ' ghost' : ''), 'data-fk': 'act:' + key, 'data-s': busy ? 'busy' : null,
    'aria-disabled': busy ? 'true' : 'false', 'aria-label': `${label} ${noun}`,
    onclick: () => {
      if (P.busy.has(key)) return;
      if (kind === 'get') return act(key, 'get', scope);
      P.ask = key; paintSheet();
      clearTimeout(P.askT);
      P.askT = setTimeout(() => { if (P.ask === key) { P.ask = ''; paintSheet(); } }, 6000);
    },
  }, !busy && kind === 'get' && ic('plus'), text);
}
async function act(key, kind, scope) {
  const tk = P.token;
  P.seq++; // an older read must not overwrite what this changes
  P.busy.add(key); P.ask = ''; P.msg.delete(key); paintSheet();
  let res;
  try {
    const body = { media_id: scope };
    if (kind === 'get' && pickedLevel()) body.level = pickedLevel();
    const r = await api(kind === 'get' ? '/v1/quick-add' : '/v1/stop', { method: 'POST', body });
    if (!r.ok) res = { text: "Couldn't do that just now. Try again in a moment.", err: true };
    else if (kind === 'get') res = { text: r.data.message || 'Done.', err: r.data.outcome === 'movies_off' };
    else if (r.data.cancelled > 0) res = { text: 'Stopped. Arr cancelled the download and will not look for it.' };
    else if (r.data.finishing > 0) res = { text: 'Stopped looking. A download that is nearly done will still finish.' };
    else res = { text: 'Stopped. Arr will not look for it.' };
  } catch { res = { text: "Couldn't reach Arr. Try again in a moment.", err: true }; }
  if (tk !== P.token) return;
  P.busy.delete(key); P.msg.set(key, res); say(res.text);
  // Reads the action made stale are issued again (also after a failure), so nothing stays loading.
  paintSheet();
  await loadTitle(true);
}
async function toggleFollow() {
  if (P.followBusy) return;
  const want = !P.data.following, tk = P.token;
  P.seq++;
  P.followBusy = true; P.data.following = want; P.msg.delete('follow'); paintSheet();
  let bad = false;
  try {
    const r = await api('/v1/follow', { method: 'POST', body: { media_id: P.data.media_id, on: want, ...(want && pickedLevel() ? { level: pickedLevel() } : {}) } });
    bad = !r.ok;
  } catch { bad = true; }
  if (tk !== P.token) return;
  P.followBusy = false;
  if (bad) {
    P.data.following = !want;
    P.msg.set('follow', { text: "Couldn't change that. Try again in a moment.", err: true });
    say("Couldn't change that.");
    paintSheet();
    return loadTitle(true);
  }
  say(want ? 'Arr will get new episodes as they air.' : 'Arr will not look for new episodes.');
  await loadTitle(true);
}

// ----- download options: a quality level, and a list of copies to pick from
const freshOpt = () => ({ open: false, level: null, saving: false, copiesOpen: false, copies: { st: 'idle' }, season: null, picking: '' });
const LEVEL_WORDS = { smaller: 'Smaller', standard: 'Standard', best: 'Best' };
function fmtSize(bytes) {
  if (!(bytes > 0)) return '';
  const gb = bytes / 1024 ** 3;
  return gb >= 1 ? `${gb >= 10 ? Math.round(gb) : gb.toFixed(1)} GB` : `${Math.max(1, Math.round(bytes / 1024 ** 2 / 10) * 10)} MB`;
}
// The level shown: the one picked on this page, else the title's own, else Standard.
const shownLevel = () => P.opt.level || P.data.level || 'standard';
// A level picked for a title Arr does not have yet travels with the Get.
const pickedLevel = () => (P.data && P.data.levels.length && !P.data.tracked && P.opt.level) || null;
async function setLevel(level) {
  const o = P.opt, d = P.data;
  if (o.saving || level === shownLevel()) return;
  o.level = level; o.gen = (o.gen || 0) + 1; P.msg.delete('opt'); o.copies = { st: 'idle' };
  if (!d.tracked) {
    // Nothing to save yet: Arr uses it when the person taps Get.
    say(`${LEVEL_WORDS[level]} quality. Arr will use it when you tap Get.`);
    paintSheet();
    if (o.copiesOpen) loadCopies();
    return;
  }
  o.saving = true; paintSheet();
  const tk = P.token;
  let res;
  try {
    const r = await api('/v1/quality', { method: 'POST', body: { media_id: d.media_id, level } });
    res = r.ok ? { text: `Saved. Arr will look for ${LEVEL_WORDS[level].toLowerCase()} quality from now on. Nothing already in Plex changes.` }
      : { text: "Couldn't change that just now. Try again in a moment.", err: true };
  } catch { res = { text: "Couldn't reach Arr. Try again in a moment.", err: true }; }
  if (tk !== P.token) return;
  o.saving = false;
  if (res.err) o.level = null;
  P.msg.set('opt', res); say(res.text); paintSheet();
  P.seq++;
  await loadTitle(true);
  if (tk !== P.token) return;
  if (!res.err) { o.level = null; paintSheet(); }
  // An open list is looked up again for the level now in force.
  if (o.copiesOpen) loadCopies();
}
const copyScope = () => (P.kind === 'movie' ? P.data.media_id : P.data.media_id.replace(/^series:/, 'season:') + ':' + (P.opt.season ?? defaultSeason()));
function defaultSeason() {
  const aired = P.data.seasons.filter((s) => s.aired_count > 0);
  return (aired.length ? aired[aired.length - 1] : P.data.seasons[P.data.seasons.length - 1] || { number: 1 }).number;
}
async function loadCopies() {
  const o = P.opt, tk = P.token, scope = copyScope(), gen = (o.gen = (o.gen || 0) + 1);
  o.copies = { st: 'loading', scope };
  paintSheet();
  const lv = pickedLevel();
  let c;
  try {
    const r = await api(`/v1/copies/${enc(scope)}` + (lv ? `?level=${lv}` : ''));
    c = r.ok ? { st: 'ready', scope, data: r.data } : { st: 'error', scope };
  } catch { c = { st: 'error', scope }; }
  // Only the newest lookup counts: an older answer is for a level or season no longer shown.
  if (tk !== P.token || o.gen !== gen) return;
  o.copies = c; paintSheet();
  if (c.st === 'ready') say(c.data.copies.length ? plural(c.data.copies.length, 'copy', 'copies') + ' found.' : "Arr couldn't find any copies right now.");
}
function toggleCopies() {
  const o = P.opt;
  o.copiesOpen = !o.copiesOpen;
  if (o.copiesOpen && o.copies.st !== 'ready' && o.copies.st !== 'loading') loadCopies(); else paintSheet();
}
const PICK_ERRORS = {
  already_queued: 'Arr is already getting that one.',
  blocklisted: "That copy failed before, so Arr won't try it again.",
  release_not_found: 'That list is out of date. Look again and pick one.',
  quality_changed: 'That copy no longer matches the quality you chose. Look again and pick one.',
  not_managed: 'Arr is not getting this right now. Tap Get to start again.',
  not_adopted: 'Arr is not getting this right now. Tap Get to start again.',
};
async function pickCopy(c) {
  const o = P.opt;
  if (o.picking) return;
  const tk = P.token;
  o.picking = c.id; P.msg.delete('copy'); P.seq++; paintSheet();
  const scope = o.copies.data.media_id;
  let res;
  try {
    const r = await api(`/v1/copies/${enc(scope)}/get`, { method: 'POST', body: { id: c.id, ...(pickedLevel() ? { level: pickedLevel() } : {}) } });
    if (r.ok) res = { text: r.data.message || 'Arr is getting that copy now.' };
    else res = { text: PICK_ERRORS[r.data && r.data.error && r.data.error.code] || "Couldn't start that one just now. Try again in a moment.", err: true };
  } catch { res = { text: "Couldn't reach Arr. Try again in a moment.", err: true }; }
  if (tk !== P.token) return;
  o.picking = '';
  P.msg.set('copy', res); say(res.text);
  if (!res.err) { o.copiesOpen = false; o.copies = { st: 'idle' }; }
  paintSheet();
  await loadTitle(true);
}
function levelChoice(l) {
  const on = shownLevel() === l.level, d = P.data;
  const size = l.size_bytes ? fmtSize(l.size_bytes) : '';
  return h('label', { class: 'lv-opt' },
    h('input', { type: 'radio', name: 'level', value: l.level, checked: on, disabled: P.opt.saving, 'data-fk': 'lv:' + l.level, onchange: () => setLevel(l.level) }),
    h('span', { class: 'lv-t' }, l.label), h('span', { class: 'lv-r' }, l.resolution), size && h('span', { class: 'lv-s' }, size));
}
function copyRow(c) {
  const line = [c.resolution, c.source].filter(Boolean).join(' · ') || 'Unknown quality';
  const speed = { fast: 'Fast', ok: 'Medium speed', slow: 'Slow' }[c.speed];
  const busy = P.opt.picking === c.id;
  const label = `${line}${c.size_bytes ? ', ' + fmtSize(c.size_bytes) : ''}`;
  return h('li', { class: 'item copy' + (c.recommended ? ' rec' : '') },
    h('div', { class: 'what' },
      h('h3', null, line),
      h('p', { class: 'meta' }, joinDot(fmtSize(c.size_bytes), speed)),
      c.recommended && h('span', { class: 'tag' }, ic('check'), 'Recommended')),
    h('button', { type: 'button', class: 'btn' + (c.recommended ? '' : ' ghost'), 'data-fk': 'copy:' + c.id, 'data-s': busy ? 'busy' : null, 'aria-disabled': P.opt.picking ? 'true' : 'false', 'aria-label': `Get this copy: ${label}${c.recommended ? ', recommended' : ''}`, onclick: () => pickCopy(c) }, busy ? 'Getting…' : 'Get this one'));
}
function copiesNode() {
  const c = P.opt.copies;
  if (c.st === 'loading' || (c.st === 'idle' && P.opt.saving)) {
    return h('div', { class: 'copies', 'aria-busy': 'true' }, h('span', { class: 'sr', role: 'status' }, 'Looking for copies'),
      h('ul', { class: 'list' }, [1, 2, 3].map(() => h('li', { class: 'item' }, h('div', { class: 'what' }, h('div', { class: 'sk', style: 'width:45%' }), h('div', { class: 'sk', style: 'width:30%;margin-top:8px' })), h('div', { class: 'sk', style: 'width:92px;height:44px;border-radius:12px' })))));
  }
  const retry = h('button', { type: 'button', class: 'btn ghost', 'data-fk': 'copies-retry', onclick: loadCopies }, ic('retry'), 'Try again');
  if (c.st === 'error') return h('div', { class: 'copies' }, hint("Couldn't look for copies just now.", 'status'), retry);
  if (c.st !== 'ready') return null;
  const n = c.data.skipped.reduce((a, x) => a + x.count, 0);
  const skipped = n ? h('p', { class: 'hint skipped' }, `Skipped ${plural(n, 'copy', 'copies')}: ` + c.data.skipped.map((x) => `${x.count} ${x.reason}`).join(', ') + '.') : null;
  if (!c.data.copies.length) return h('div', { class: 'copies' }, hint("Arr couldn't find any copies right now.", 'status'), skipped, retry);
  return h('div', { class: 'copies' }, h('ul', { class: 'list' }, c.data.copies.map(copyRow)), skipped);
}
function optionsPanel() {
  const o = P.opt, d = P.data, lv = d.levels.length > 0;
  const cur = lv ? d.levels.find((l) => l.level === shownLevel()) : null;
  const sub = cur ? `Quality: ${cur.label} (${cur.resolution})` : 'Choose a copy yourself';
  const season = P.kind === 'show' && o.copiesOpen && d.seasons.length > 1
    ? h('label', { class: 'pick-season' }, h('span', null, 'Season'),
      h('select', { 'data-fk': 'season-pick', onchange: (e) => { o.season = Number(e.target.value); o.copies = { st: 'idle' }; loadCopies(); } },
        d.seasons.map((s) => h('option', { value: s.number, selected: s.number === (o.season ?? defaultSeason()) }, 'Season ' + s.number)))) : null;
  const fs = lv && h('fieldset', { class: 'lv', disabled: o.saving, 'aria-busy': o.saving ? 'true' : null },
    h('legend', null, 'Quality'),
    h('p', { class: 'lv-note' }, P.kind === 'movie' ? 'Estimated size of this movie at each level.' : 'Estimated size of each episode at each level.'),
    h('div', { class: 'lv-row' }, d.levels.map(levelChoice)),
    h('p', { class: 'lv-note' }, shownLevel() === 'best' ? 'Best takes 1080p when there is no 4K copy. ' : '', 'This never removes or replaces anything already in Plex. It only changes what Arr looks for next.'),
    !d.tracked && o.level ? h('p', { class: 'lv-note' }, 'Arr will use this when you tap Get.') : null,
    msgFor('opt'));
  return h('section', { class: 'opts', 'aria-label': 'Download options' },
    h('button', { type: 'button', class: 'opts-head', 'aria-expanded': String(o.open), 'aria-controls': 'opts-body', 'data-fk': 'opts', onclick: () => { o.open = !o.open; paintSheet(); } },
      h('span', { class: 'what' }, h('span', { class: 'sw-t' }, 'Download options'), h('span', { class: 'sw-m' }, sub)), ic('down')),
    o.open && h('div', { id: 'opts-body', class: 'opts-body' },
      fs,
      h('div', { class: 'copies-box' },
        h('button', { type: 'button', class: 'link', 'aria-expanded': String(o.copiesOpen), 'aria-controls': 'copies', 'data-fk': 'copies-open', onclick: toggleCopies }, 'Choose a copy yourself', ic('right')),
        h('div', { id: 'copies' }, o.copiesOpen && [season, copiesNode()]),
        msgFor('copy'))));
}

// ----- the show page
function seasonFact(s) {
  const left = s.episode_count;
  switch (s.state) {
    case 'in_plex': return `${plural(left, 'episode', 'episodes')} · all in Plex`;
    case 'partly': return `${s.in_plex} of ${left} in Plex` + (s.wanted > 0 ? ' · Arr is looking for the rest' : s.next_air_date ? ` · next ${fmtDate(s.next_air_date)}` : '');
    case 'on_its_way': return 'On its way' + (typeof s.progress === 'number' ? ` · ${pct(s.progress)}%` : '');
    case 'wanted': return 'Arr is looking for a copy';
    case 'upcoming': return s.next_air_date ? `Airs ${fmtDate(s.next_air_date)}` : 'Not aired yet';
    default: return plural(left, 'episode', 'episodes');
  }
}
function seasonAction(s) {
  if (s.state === 'none') return ['get', 'Get'];
  if (s.state === 'partly') return s.wanted > 0 ? ['stop', 'Stop'] : s.missing > 0 ? ['get', 'Get the rest'] : null;
  if (s.state === 'wanted' || s.state === 'on_its_way') return ['stop', 'Stop'];
  return null;
}
function toggleSeason(n) {
  if (P.open.has(n)) { P.open.delete(n); return paintSheet(); }
  P.open.add(n);
  if (!(P.seasons.get(n) || {}).eps) {
    P.seasons.set(n, { loading: true });
    const tk = P.token;
    loadSeason(n, tk, P.seq).then(() => { if (tk === P.token) paintSheet(); });
  }
  paintSheet();
}
function episodesNode(n) {
  const st = P.seasons.get(n) || {};
  if (st.eps) {
    if (!st.eps.length) return hint('No episodes are listed for this season yet.');
    return h('ul', { class: 'eps' }, st.eps.map(episodeRow));
  }
  if (st.err) {
    return h('div', { class: 'eps-err' }, hint(st.err, 'status'),
      h('button', { type: 'button', class: 'btn ghost', 'data-fk': 'eretry:' + n, onclick: () => { P.seasons.set(n, { loading: true }); paintSheet(); const tk = P.token; loadSeason(n, tk, P.seq).then(() => { if (tk === P.token) paintSheet(); }); } }, ic('retry'), 'Try again'));
  }
  return h('div', { class: 'eps', 'aria-busy': 'true' }, [1, 2, 3].map(() => h('div', { class: 'ep' }, h('div', { class: 'sk', style: 'width:28px' }), h('div', { class: 'what' }, h('div', { class: 'sk', style: 'width:55%' })))));
}
function episodeRow(e) {
  const key = 'e:' + e.media_id, title = e.title || `Episode ${e.number}`;
  const kind = e.state === 'none' ? 'get' : e.state === 'wanted' || e.state === 'on_its_way' ? 'stop' : null;
  const fact = e.state === 'in_plex' ? h('span', { class: 'tag' }, ic('check'), 'In Plex')
    : e.state === 'on_its_way' ? 'On its way' + (typeof e.progress === 'number' ? ` · ${pct(e.progress)}%` : '')
      : e.state === 'wanted' ? 'Arr is looking for it'
        : e.state === 'upcoming' ? (e.air_date ? `Airs ${fmtDate(e.air_date)}` : 'Not aired yet')
          : e.air_date ? `Aired ${fmtDate(e.air_date)}` : '';
  return h('li', { class: 'ep' }, h('span', { class: 'num', 'aria-hidden': 'true' }, e.number),
    h('div', { class: 'what' }, h('h4', { title }, title), fact && h('div', { class: 'meta' }, fact), msgFor(key)),
    kind && h('div', { class: 'act' }, actionNode(key, kind, kind === 'get' ? 'Get' : 'Stop', `episode ${e.number}, ${title}`, e.media_id)));
}
function seasonItem(s) {
  const n = s.number, key = 's:' + n, open = P.open.has(n), a = seasonAction(s);
  const scope = P.data.media_id.replace(/^series:/, 'season:') + ':' + n;
  return h('li', { class: 'item sea' },
    h('div', { class: 'sea-head' },
      h('button', { type: 'button', class: 'sbtn', 'aria-expanded': String(open), 'aria-controls': 'eps-' + n, 'data-fk': 'season:' + n, onclick: () => toggleSeason(n) },
        h('span', { class: 'what' }, h('span', { class: 't' }, 'Season ' + n), h('span', { class: 'meta' }, seasonFact(s))), ic('down')),
      a && h('div', { class: 'act' }, actionNode(key, a[0], a[1], 'Season ' + n, scope))),
    msgFor(key),
    open && h('div', { id: 'eps-' + n }, episodesNode(n)));
}
function showBody() {
  const d = P.data, on = d.following;
  return h('div', { class: 'stack' },
    hero(d.title, joinDot(d.year, d.network, d.status)),
    summary(d.summary),
    h('div', null, h('button', { type: 'button', role: 'switch', class: 'switch', 'aria-checked': String(on), 'aria-labelledby': 'sw-t', 'data-fk': 'follow', onclick: toggleFollow },
      h('span', { class: 'what' }, h('span', { class: 'sw-t', id: 'sw-t' }, 'Get new episodes as they air'),
        h('span', { class: 'sw-m' }, on ? 'Arr gets each new episode when it airs.' : 'Off. Arr is not looking for new episodes.')),
      h('span', { class: 'track', 'aria-hidden': 'true' }, h('i'))), msgFor('follow')),
    optionsPanel(),
    section('seasons', 'Seasons', d.seasons.length ? h('ul', { class: 'list' }, d.seasons.map(seasonItem)) : hint('No seasons are listed for this show yet. Check back soon.')));
}

// ----- the movie page
function journeyEl(stage, p, key) {
  const target = stage === 1 ? (1 + (p ?? 0)) / 3 : stage / 3;
  const fill = h('i', { class: 'fill' });
  fill.style.setProperty('--f', fills.get(key) ?? 0);
  sheetFills.push([fill, target, key]);
  return h('ol', { class: 'journey', 'aria-label': 'Progress' }, fill,
    STAGES.map((s, i) => h('li', { class: i <= stage ? 'on' : '', 'aria-current': i === stage ? 'step' : null }, s)));
}
function movieBody() {
  const d = P.data, st = d.state, key = 'movie';
  const p = typeof d.progress === 'number' ? d.progress : null;
  const line = st === 'in_plex' ? 'In Plex' : st === 'on_its_way' ? 'On its way' + (p == null ? '' : ` · ${pct(p)}%`)
    : st === 'waiting' ? (d.until ? `Out ${fmtDate(d.until)}. Arr will get it then.` : 'Not out yet. Arr will get it then.')
      : st === 'wanted' ? 'Arr is looking for a copy' : 'Not in Plex yet';
  let control = null;
  if (st === 'in_plex') control = plexLink(d.title);
  else if (st === 'none') control = actionNode(key, 'get', 'Get', d.title, d.media_id);
  else control = actionNode(key, 'stop', 'Stop', d.title, d.media_id);
  return h('div', { class: 'stack' },
    hero(d.title, joinDot(d.year, d.runtime_minutes && fmtRuntime(d.runtime_minutes))),
    summary(d.summary),
    h('section', { class: 'item state' + (st === 'in_plex' ? ' arrived' : ''), 'aria-label': 'Status' },
      h('div', { class: 'what' },
        st === 'in_plex' ? h('span', { class: 'tag' }, ic('check'), 'In Plex') : null,
        h('p', { class: 'st' }, st === 'in_plex' ? 'Ready to watch' : line),
        st === 'on_its_way' ? journeyEl(p == null ? 0 : 1, p, 'movie:' + d.media_id) : null,
        msgFor(key)),
      h('div', { class: 'act' }, control)),
    optionsPanel());
}

// ---------- sign in ----------
function showSignin(data) {
  S.out = true; token++;
  clearTimeout(pollT); clearTimeout(retryT);
  top.hidden = false; $('#search').hidden = true; tabs.hidden = true; $('#foot').hidden = true; banner.textContent = '';
  document.title = 'Sign in · Arr';
  const google = data && data.signin && data.signin.google;
  mount(h('section', { class: 'signin' }, h('img', { src: '/app/mascot.svg', alt: '' }), h('h1', null, 'Sign in to Arr'),
    google ? [h('p', null, 'Use your Google account to see what is coming to Plex.'), h('a', { class: 'btn', href: '/app/signin' }, 'Continue with Google')]
      : h('p', null, 'Ask the person who runs Arr for a sign-in link.')));
}

// ---------- routing ----------
function parseHash(hash = location.hash) {
  const [path, qs] = (hash.slice(1) || '/').split('?');
  return { path, q: new URLSearchParams(qs || '').get('q') || '' };
}
const pageName = (path) => (path === '/search' ? 'Search' : path === '/ideas' ? 'Ideas' : path === '/library' ? 'Library' : 'Home');
const TABS = [['/', 'Home', 'home'], ['/ideas', 'Ideas', 'ideas'], ['/library', 'Library', 'library']];
function route() {
  if (S.out) return;
  const { path, q } = parseHash();
  const tr = titleRoute(path);
  if (tr) return openTitle(tr);
  const hash = location.hash || '#/';
  if (sheetOpen()) {
    closeSheet();
    // Back to the screen the sheet was opened over: it is still there as it was.
    if (hash === bgHash) { if (onHome()) loadFeed(); return; }
  }
  renderPath(path, q, hash);
}
function renderPath(path, q, hash) {
  bgHash = hash;
  const t = ++token;
  clearTimeout(retryT); clearTimeout(pollT); clearTimeout(debounceT);
  for (const a of tabs.children) { if (a.getAttribute('href') === '#' + path) a.setAttribute('aria-current', 'page'); else a.removeAttribute('aria-current'); }
  searchAbort?.abort();
  document.title = pageName(path) + ' · Arr';
  input.value = path === '/search' ? q : '';
  if (path === '/search') renderSearch(q, t);
  else if (path === '/ideas') renderIdeas(t);
  else if (path === '/library') renderLibrary(t);
  else renderHome(t);
}

// ---- Use Arr on your phone: the address, a QR code and how to put Arr on the home screen
const REMOTE_DOCS = 'https://github.com/newtonharry/Arr/blob/main/docs/REMOTE.md';
let publicUrl = null;

function showFoot(me) {
  $('#who').textContent = me.email ? 'Signed in as ' + me.email : 'Signed in';
  publicUrl = me.public_url || null;
  $('#foot').hidden = false;
}

// The code is always dark on white, whatever the theme: phones scan it that way.
function qrSvg(text) {
  const q = qrcodegen.QrCode.encodeText(text, qrcodegen.QrCode.Ecc.MEDIUM), pad = 3;
  const s = document.createElementNS(NS, 'svg'), bg = document.createElementNS(NS, 'rect'), p = document.createElementNS(NS, 'path');
  let d = '';
  for (let y = 0; y < q.size; y++) for (let x = 0; x < q.size; x++) if (q.getModule(x, y)) d += `M${x + pad},${y + pad}h1v1h-1z`;
  s.setAttribute('viewBox', `0 0 ${q.size + pad * 2} ${q.size + pad * 2}`);
  s.setAttribute('class', 'qr'); s.setAttribute('role', 'img'); s.setAttribute('aria-label', 'QR code for ' + text);
  s.setAttribute('shape-rendering', 'crispEdges');
  bg.setAttribute('width', '100%'); bg.setAttribute('height', '100%'); bg.setAttribute('fill', '#fff');
  p.setAttribute('d', d); p.setAttribute('fill', '#000');
  s.append(bg, p);
  return s;
}

async function copyText(node, text) {
  try { await navigator.clipboard.writeText(text); return true; } catch { /* not allowed here: select it instead */ }
  const r = document.createRange(); r.selectNodeContents(node);
  const sel = getSelection(); sel.removeAllRanges(); sel.addRange(r);
  try { return document.execCommand('copy'); } catch { return false; }
}

function paintPhone() {
  const box = $('#phone');
  const code = h('code', null, publicUrl);
  const msg = h('span', { class: 'msg', role: 'status' });
  const copy = h('button', { type: 'button', class: 'btn ghost' }, 'Copy');
  copy.addEventListener('click', async () => { msg.textContent = (await copyText(code, publicUrl)) ? 'Copied' : 'Press Ctrl+C to copy'; });
  const how = h('div', { class: 'how' },
    h('p', null, h('b', null, 'iPhone: '), 'open the address in Safari, tap Share, then Add to Home Screen.'),
    h('p', null, h('b', null, 'Android: '), 'open the address in Chrome, tap the menu (three dots), then Add to Home screen or Install app.'));
  box.replaceChildren(publicUrl
    ? h('div', { class: 'stack' },
      h('p', null, 'Open this address on your phone and sign in with Google:'),
      h('div', { class: 'addr' }, code, copy, msg),
      qrSvg(publicUrl),
      how)
    : h('div', { class: 'stack' },
      h('p', null, 'Arr works on this computer for now. To use it on your phone, turn on remote access: run the installer again with ',
        h('code', null, '--remote'), ' (', h('a', { href: REMOTE_DOCS, target: '_blank', rel: 'noopener noreferrer' }, 'how it works'), ').'),
      h('p', null, 'Once that is done, this page shows an address and a QR code to scan.')));
}

$('#phone-btn').addEventListener('click', () => {
  const btn = $('#phone-btn'), open = $('#phone').hidden;
  if (open) paintPhone();
  $('#phone').hidden = !open;
  btn.setAttribute('aria-expanded', String(open));
  if (open) $('#phone').scrollIntoView({ block: 'nearest', behavior: 'smooth' });
});

async function boot() {
  $('#search-ico').append(ic('search'));
  tabs.append(...TABS.map(([p, l, i]) => h('a', { href: '#' + p }, ic(i), l)));
  $('#search').addEventListener('submit', (e) => { e.preventDefault(); const q = input.value.trim(); if (q.length >= 2) { clearTimeout(debounceT); go(q); } });
  input.addEventListener('input', () => {
    clearTimeout(debounceT);
    const q = input.value.trim();
    debounceT = setTimeout(() => {
      if (q.length >= 2) go(q);
      else if (!q && parseHash().path === '/search') { history.replaceState(null, '', '#/'); route(); }
    }, 300);
  });
  $('#signout').addEventListener('click', async () => {
    try { await api('/signout', { method: 'POST' }); } catch { /* signed out anyway */ }
    location.hash = ''; location.reload();
  });
  document.addEventListener('visibilitychange', () => {
    if (!document.hidden && onHome()) loadFeed(); else clearTimeout(pollT);
    if (!document.hidden && sheetOpen()) loadTitle(true);
  });
  window.addEventListener('hashchange', route);
  try {
    const r = await api('/me');
    if (r.ok) showFoot(r.data);
  } catch {
    if (S.out) return;
    mount(hint('Your list will show up here as soon as Arr is back.'));
    setTimeout(boot2, 5000);
    return;
  }
  route();
  // The feed also says whether TMDB is used, which the footer credit needs on every screen.
  if (!onHome()) loadFeed();
}
function go(q) {
  const url = '#/search?q=' + encodeURIComponent(q);
  if (parseHash().path === '/search') { history.replaceState(null, '', url); bgHash = url; token++; renderSearch(q, token); }
  else location.hash = url;
}
async function boot2() {
  try {
    const r = await api('/me');
    if (r.ok) { showFoot(r.data); route(); if (!onHome()) loadFeed(); }
  } catch { if (!S.out) setTimeout(boot2, 5000); }
}
boot();
