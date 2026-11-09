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
        : await api('/v1/quick-add', { method: 'POST', body: { media_id: it.media_id } });
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
    h('div', { class: 'what' }, h('h3', { title: it.title }, it.title),
      meta(it) && h('p', { class: 'meta' }, meta(it)),
      h('p', { class: 'problem' }, ic('alert'), h('span', null, it.problem)), msg), btn);
}
function arrivedRow(it) {
  return h('li', { class: 'item arrived' }, tile(it.title, it.media_id),
    h('div', { class: 'what' }, h('span', { class: 'tag' }, ic('check'), 'In Plex'),
      h('h3', { title: it.title }, it.title), h('p', { class: 'meta' }, joinDot(it.detail, ago(it.at)))),
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
    h('div', { class: 'what' }, h('h3', { title: it.title }, it.title),
      h('p', { class: 'meta status' }, it.paused && ic('pause'), joinDot(meta(it), status)),
      h('ol', { class: 'journey' + (it.paused ? ' paused' : ''), 'aria-label': 'Progress' }, fill,
        STAGES.map((s, i) => h('li', { class: i <= stage ? 'on' : '', 'aria-current': i === stage ? 'step' : null }, s)))));
}
let pendingFills = [];
function lookRow(it) {
  const until = it.until ? parseDate(it.until) : null;
  return h('li', { class: 'item' }, until ? chip(until, 'month') : tile(it.title, it.media_id),
    h('div', { class: 'what' }, h('h3', { title: it.title }, it.title),
      h('p', { class: 'meta' }, joinDot(meta(it), until ? `Out ${fmtDay(until)}. Arr will get it then.` : 'Arr is looking for a copy'))));
}
function soonRow(it) {
  return h('li', { class: 'item' }, chip(new Date(it.at), 'day'),
    h('div', { class: 'what' }, h('h3', { title: it.title }, it.title), h('p', { class: 'meta' }, it.detail || 'New episode')));
}
function ideaItem(x) {
  return { id: x.media_id, title: x.title, base: x.in_library ? 'added' : 'add' };
}
function ideaCard(x) {
  const [btn] = addControl(ideaItem(x), false);
  return h('li', { class: 'idea' }, tile(x.title, x.media_id), h('h3', { title: x.title }, x.title),
    h('p', { class: 'meta' }, x.network || x.year || ''), btn);
}
function ideaRow(x) {
  const [btn, msg] = addControl(ideaItem(x));
  return h('li', { class: 'item top-align' }, tile(x.title, x.media_id),
    h('div', { class: 'what' }, h('h3', { title: x.title }, x.title),
      h('p', { class: 'meta' }, joinDot(x.year, x.network)), x.summary && h('p', { class: 'clamp' }, x.summary), msg), btn);
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
  const base = x.has_file ? 'inplex' : x.in_catalog && x.monitored ? 'added' : 'add';
  const [btn, msg] = addControl({ id: x.media_id, title: x.title, base });
  return h('li', { class: 'item top-align' }, tile(x.title, x.media_id),
    h('div', { class: 'what' }, h('h3', { title: x.title }, x.title), h('p', { class: 'meta' }, joinDot(x.year, label)), msg), btn);
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
    return h('li', { class: 'item' }, tile(title, mid), h('div', { class: 'what' }, h('h3', { title }, title),
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
function parseHash() {
  const [path, qs] = (location.hash.slice(1) || '/').split('?');
  return { path, q: new URLSearchParams(qs || '').get('q') || '' };
}
const TABS = [['/', 'Home', 'home'], ['/ideas', 'Ideas', 'ideas'], ['/library', 'Library', 'library']];
function route() {
  if (S.out) return;
  const t = ++token, { path, q } = parseHash();
  clearTimeout(retryT); clearTimeout(pollT); clearTimeout(debounceT);
  for (const a of tabs.children) { if (a.getAttribute('href') === '#' + path) a.setAttribute('aria-current', 'page'); else a.removeAttribute('aria-current'); }
  searchAbort?.abort();
  const name = path === '/search' ? 'Search' : path === '/ideas' ? 'Ideas' : path === '/library' ? 'Library' : 'Home';
  document.title = name + ' · Arr';
  input.value = path === '/search' ? q : '';
  if (path === '/search') renderSearch(q, t);
  else if (path === '/ideas') renderIdeas(t);
  else if (path === '/library') renderLibrary(t);
  else renderHome(t);
}

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
  document.addEventListener('visibilitychange', () => { if (!document.hidden && onHome()) loadFeed(); else clearTimeout(pollT); });
  window.addEventListener('hashchange', route);
  try {
    const r = await api('/me');
    if (r.ok) { $('#who').textContent = r.data.email ? 'Signed in as ' + r.data.email : 'Signed in'; $('#foot').hidden = false; }
  } catch {
    if (S.out) return;
    mount(hint('Your list will show up here as soon as Arr is back.'));
    setTimeout(boot2, 5000);
    return;
  }
  route();
}
function go(q) {
  const url = '#/search?q=' + encodeURIComponent(q);
  if (parseHash().path === '/search') { history.replaceState(null, '', url); token++; renderSearch(q, token); }
  else location.hash = url;
}
async function boot2() {
  try {
    const r = await api('/me');
    if (r.ok) { $('#who').textContent = r.data.email ? 'Signed in as ' + r.data.email : 'Signed in'; $('#foot').hidden = false; route(); }
  } catch { if (!S.out) setTimeout(boot2, 5000); }
}
boot();
