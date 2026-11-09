// Arr web app. Preact (vendored under /app/vendor, no build step, no JSX). Server text only ever
// reaches the page as Preact children or attributes, never as markup.
import { h, render, Fragment } from '/app/vendor/preact.mjs';
import { useState, useEffect, useLayoutEffect, useMemo, useRef } from '/app/vendor/hooks.mjs';

const $ = (s) => document.querySelector(s);
// Module state, read while rendering. Whatever changes it calls bump() to draw it again.
const S = { feed: null, ideas: null, ideasAt: 0, out: false, all: false, kind: 'movies', filter: '', me: null, tmdb: false, banner: '', live: '' };
let V = { t: 'hint', text: '' }; // what the main area shows
const R = { path: null }; // the screen under the sheet, for the active tab
const A = new Map(); // add / retry state by id, kept for the session
const fills = new Map();
let token = 0, pollT = 0, retryT = 0, feedBusy = false, searchSeq = 0, searchAbort = null, debounceT = 0;

// ---------- drawing ----------
let rerender = () => {}, pendingFk = null;
const later = [];
// Focus that a redraw takes away (a button swapped for another) goes to the element with the same data-fk.
function focusKey() {
  const a = document.activeElement, fk = a && a.dataset && a.dataset.fk;
  if (!fk) return null;
  const p = $('#panel');
  return { fk, panel: !!(p && p.contains(a)) };
}
function bump() {
  if (!pendingFk) pendingFk = focusKey() || { fk: '' };
  rerender();
}
// Runs once the page has been redrawn.
function after(fn) { later.push(fn); bump(); }
function setView(v) { V = v; bump(); }
function setBanner(t) { if (S.banner !== t) { S.banner = t; bump(); } }
const say = (m) => { S.live = ''; bump(); setTimeout(() => { S.live = m; bump(); }, 50); };
const hint = (text, role) => ({ t: 'hint', text, role });

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
  star: 'M12 3.5l2.6 5.4l5.9.8l-4.3 4.1l1 5.9L12 16.9l-5.2 2.8l1-5.9l-4.3-4.1l5.9-.8z',
};
const ic = (name) => h('svg', { viewBox: '0 0 24 24', class: 'ic', 'aria-hidden': 'true' }, h('path', { d: ICONS[name] }));

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
    if (e.name !== 'AbortError') setBanner("Can't reach Arr right now. Trying again…");
    throw e;
  }
  let data = null;
  try { data = await r.json(); } catch { /* empty body */ }
  if (r.status === 401) { showSignin(data); throw new Error('signed out'); }
  const code = data && data.error && data.error.code;
  if (code === 'catalog_not_imported') { setBanner('Arr is still being set up. Check back soon.'); throw new Error(code); }
  // Only a reply that isn't Arr's own error (a proxy or tunnel failing) means Arr is out of reach.
  if (r.status >= 500 && !code) { setBanner("Can't reach Arr right now. Trying again…"); throw new Error('unreachable'); }
  setBanner('');
  return { ok: r.ok, status: r.status, data };
}

// ---------- small helpers ----------
const plexUrl = (t) => 'https://app.plex.tv/desktop/#!/search?query=' + encodeURIComponent(t);
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
// Rows need keys that stay put between redraws, even if the server repeats an id.
function keyed(items, id, fn) {
  const seen = new Map();
  return items.map((it, i) => {
    const k = String(id(it, i)), n = seen.get(k) || 0;
    seen.set(k, n + 1);
    return fn(it, n ? `${k}#${n}` : k);
  });
}
function Section({ id, title, extra, children }) {
  return h('section', { 'aria-labelledby': 'h-' + id }, h('h2', { id: 'h-' + id }, title, extra), children);
}
const List = ({ children }) => h('ul', { class: 'list' }, children);
const Hint = ({ text, role }) => h('p', { class: 'hint', role }, text);

// The title tile is the placeholder; a poster, when there is one, fades in over it.
function Poster({ id }) {
  const [s, set] = useState('');
  if (s === 'err') return null;
  return h('img', { src: '/app/art/' + encodeURIComponent(id), alt: '', loading: 'lazy', decoding: 'async', class: s === 'on' ? 'on' : null, onLoad: () => set('on'), onError: () => set('err') });
}
function Tile({ title, mediaId, big }) {
  const words = String(title).split(/\s+/).filter((w) => /[\p{L}\p{N}]/u.test(w));
  const sig = words.filter((w) => !/^(the|a|an)$/i.test(w));
  const use = sig.length ? sig : words;
  const ch = (w) => [...(w || '').replace(/[^\p{L}\p{N}]/gu, '')];
  const s = use.length > 1 ? (ch(use[0])[0] || '') + (ch(use[1])[0] || '') : ch(use[0]).slice(0, 2).join('');
  let n = 0;
  for (const c of String(title)) n = (n * 31 + c.codePointAt(0)) >>> 0;
  return h('span', { class: 'tile t' + (n % 8) + (big ? ' big' : ''), 'aria-hidden': 'true' }, (s || '?').toUpperCase(),
    mediaId ? h(Poster, { key: mediaId, id: mediaId }) : null);
}

// ---------- add / retry controls ----------
function addBtn(it) {
  const st = A.get(it.id) || {}, s = st.busy ? 'busy' : st.state || it.base;
  const L = { add: ['plus', 'Add'], busy: [null, 'Adding…'], added: ['check', 'Added'], following: ['check', 'Following'], inplex: ['check', 'In Plex'] }[s];
  return h('button', {
    type: 'button', class: 'btn', 'data-fk': 'add:' + it.id, 'data-s': s, 'aria-disabled': s === 'add' ? 'false' : 'true',
    'aria-label': s === 'add' ? `Add ${it.title}` : `${L[1]}, ${it.title}`, onClick: () => addOne(it),
  }, L[0] ? ic(L[0]) : null, L[1]);
}
const Msg = ({ st }) => h('p', { class: 'msg', 'data-err': st.err ? '' : null }, st.msg || null);
const addMsg = (it) => h(Msg, { st: A.get(it.id) || {} });
async function addOne(it) {
  const st0 = A.get(it.id) || {};
  if ((st0.busy ? 'busy' : st0.state || it.base) !== 'add') return;
  A.set(it.id, { busy: true }); bump();
  let next;
  try {
    const r = await api('/v1/quick-add', { method: 'POST', body: { media_id: it.id } });
    if (r.ok && r.data) {
      const o = r.data.outcome;
      next = { msg: r.data.message || 'Added.', state: o === 'in_plex' ? 'inplex' : o === 'following' ? 'following' : o === 'movies_off' ? undefined : 'added', err: o === 'movies_off' };
    } else next = { msg: "Couldn't add this one. Try again in a moment.", err: true };
  } catch { next = { msg: "Couldn't reach Arr. Try again in a moment.", err: true }; }
  A.set(it.id, next); bump(); say(next.msg);
  if (!next.err) loadFeed();
}
// Keyed by when it went wrong too, so a later failure of the same item can be retried again.
const retryId = (it) => 'r:' + it.key + '|' + it.at;
async function retryOne(it) {
  const id = retryId(it), st = A.get(id) || {};
  if (st.busy || st.state) return;
  A.set(id, { busy: true }); bump();
  let next;
  try {
    const r = it.grab_id
      ? await api(`/v1/queue/${encodeURIComponent(it.grab_id)}/retry`, { method: 'POST' })
      : await api('/v1/quick-add', { method: 'POST', body: { media_id: it.target || it.media_id } });
    next = r.ok ? { state: 'again', msg: 'Trying again. Check back in a minute.' } : { msg: "Couldn't try again just now. Give it a moment.", err: true };
  } catch { next = { msg: "Couldn't reach Arr. Try again in a moment.", err: true }; }
  A.set(id, next); bump(); say(next.msg);
  if (!next.err) loadFeed();
}

// ---------- links to a title's page ----------
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
const Title = ({ title, id }) => h('h3', { title }, stretch(title, id));

// ---------- rows ----------
function NeedRow({ it }) {
  const id = retryId(it), st = A.get(id) || {}, off = st.busy || st.state;
  return h('li', { class: 'item need' }, h(Tile, { title: it.title, mediaId: it.media_id }),
    h('div', { class: 'what' }, h(Title, { title: it.title, id: it.media_id }),
      meta(it) && h('p', { class: 'meta' }, meta(it)),
      h('p', { class: 'problem' }, ic('alert'), h('span', null, it.problem)), h(Msg, { st })),
    h('button', { type: 'button', class: 'btn coral', 'data-fk': id, 'aria-disabled': off ? 'true' : 'false', 'aria-label': `Try again, ${it.title}`, onClick: () => retryOne(it) },
      ic('retry'), h('span', { class: 'lbl' }, st.busy ? 'Trying…' : st.state ? 'Trying again' : 'Try again')));
}
function ArrivedRow({ it }) {
  return h('li', { class: 'item arrived' }, h(Tile, { title: it.title, mediaId: it.media_id }),
    h('div', { class: 'what' }, h('span', { class: 'tag' }, ic('check'), 'In Plex'),
      h(Title, { title: it.title, id: it.media_id }), h('p', { class: 'meta' }, joinDot(it.detail, ago(it.at)))),
    plexLink(it.title));
}
const STAGES = ['Found', 'Downloading', 'Checking', 'In Plex'];
// The bar slides from where it last was to where it is now.
function Journey({ stage, p, fkey, paused }) {
  const el = useRef(null);
  const target = stage === 1 ? (1 + (p ?? 0)) / 3 : stage / 3;
  useLayoutEffect(() => {
    el.current.style.setProperty('--f', fills.get(fkey) ?? 0);
    let b;
    const a = requestAnimationFrame(() => {
      b = requestAnimationFrame(() => { if (el.current) el.current.style.setProperty('--f', target); fills.set(fkey, target); });
    });
    return () => { cancelAnimationFrame(a); cancelAnimationFrame(b); };
  }, [fkey, target]);
  return h('ol', { class: 'journey' + (paused ? ' paused' : ''), 'aria-label': 'Progress' }, h('i', { class: 'fill', ref: el }),
    STAGES.map((s, i) => h('li', { key: s, class: i <= stage ? 'on' : '', 'aria-current': i === stage ? 'step' : null }, s)));
}
function WayRow({ it }) {
  const stage = Math.max(0, ['found', 'downloading', 'checking'].indexOf(it.stage));
  const p = typeof it.progress === 'number' ? Math.min(1, Math.max(0, it.progress)) : null;
  const status = it.paused ? 'Paused' : stage === 0 ? 'Found, getting ready' : stage === 1 ? (p == null ? 'Downloading' : `${Math.round(p * 100)}% downloaded`) : 'Checking the file';
  return h('li', { class: 'item' }, h(Tile, { title: it.title, mediaId: it.media_id }),
    h('div', { class: 'what' }, h(Title, { title: it.title, id: it.media_id }),
      h('p', { class: 'meta status' }, it.paused && ic('pause'), joinDot(meta(it), status)),
      h(Journey, { stage, p, fkey: it.grab_id, paused: it.paused })));
}
function LookRow({ it }) {
  const until = it.until ? parseDate(it.until) : null;
  return h('li', { class: 'item' }, until ? chip(until, 'month') : h(Tile, { title: it.title, mediaId: it.media_id }),
    h('div', { class: 'what' }, h(Title, { title: it.title, id: it.media_id }),
      h('p', { class: 'meta' }, joinDot(meta(it), until ? `Out ${fmtDay(until)}. Arr will get it then.` : 'Arr is looking for a copy'))));
}
function SoonRow({ it }) {
  return h('li', { class: 'item' }, chip(new Date(it.at), 'day'),
    h('div', { class: 'what' }, h(Title, { title: it.title, id: it.media_id }), h('p', { class: 'meta' }, it.detail || 'New episode')));
}
// Ideas are shows: the button opens the show page, where the person picks what to get.
function chooseLink(x) {
  return h('a', { class: 'btn' + (x.in_library ? ' ghost' : ''), href: titleHref(x.media_id), 'aria-label': `${x.in_library ? 'Open' : 'Choose'} ${x.title}` }, x.in_library ? 'Open' : 'Choose', ic('right'));
}
// TVmaze viewers' average out of 10: a star and the number, read out in words.
function rating(x) {
  const r = Number(x.rating);
  if (!(r > 0 && r <= 10)) return null;
  const n = r.toFixed(1);
  return h('span', { class: 'rating', title: `Rated ${n} out of 10 by TVmaze viewers` },
    ic('star'), h('span', { 'aria-hidden': 'true' }, n), h('span', { class: 'sr' }, `Rated ${n} out of 10`));
}
function ideaMeta(...facts) {
  const r = rating(facts.shift()), text = joinDot(...facts);
  return h('p', { class: 'meta' }, r, r && text ? ' · ' : null, text);
}
function IdeaCard({ x }) {
  return h('li', { class: 'idea' }, h(Tile, { title: x.title, mediaId: x.media_id }), h(Title, { title: x.title, id: x.media_id }),
    ideaMeta(x, x.network || x.year || ''), chooseLink(x));
}
function IdeaRow({ x }) {
  return h('li', { class: 'item top-align' }, h(Tile, { title: x.title, mediaId: x.media_id }),
    h('div', { class: 'what' }, h(Title, { title: x.title, id: x.media_id }),
      ideaMeta(x, x.year, x.network), x.summary && h('p', { class: 'clamp' }, x.summary)), chooseLink(x));
}

// ---------- home ----------
function Home() {
  const f = S.feed, n = (k) => f[k] || [];
  const secs = [];
  if (n('needs_you').length) secs.push(h(Section, { key: 'need', id: 'need', title: 'Needs you' }, h(List, null, keyed(n('needs_you'), (it) => it.key + '|' + it.at, (it, key) => h(NeedRow, { key, it })))));
  if (n('arrived').length) secs.push(h(Section, { key: 'arrived', id: 'arrived', title: 'Arrived' }, h(List, null, keyed(n('arrived'), (it) => it.media_id + '|' + it.at, (it, key) => h(ArrivedRow, { key, it })))));
  if (n('on_its_way').length) {
    const all = n('on_its_way'), shown = S.all ? all : all.slice(0, 3);
    const more = all.length > 3 && h('button', { type: 'button', class: 'link', 'aria-expanded': String(S.all), 'data-fk': 'all', onClick: () => { S.all = !S.all; bump(); } }, S.all ? 'Show fewer' : `See all ${all.length}`);
    secs.push(h(Section, { key: 'way', id: 'way', title: 'On its way', extra: more }, h(List, null, keyed(shown, (it) => it.grab_id, (it, key) => h(WayRow, { key, it })))));
  }
  if (n('looking').length) secs.push(h(Section, { key: 'look', id: 'look', title: 'Looking' }, h(List, null, keyed(n('looking'), (it) => it.media_id, (it, key) => h(LookRow, { key, it })))));
  if (n('coming_up').length) secs.push(h(Section, { key: 'soon', id: 'soon', title: 'Coming up' }, h(List, null, keyed(n('coming_up'), (it) => it.media_id + '|' + it.at, (it, key) => h(SoonRow, { key, it })))));
  const ideas = ((S.ideas && S.ideas.items) || []).filter((x) => !x.in_library).slice(0, 6);
  if (!secs.length) {
    secs.push(h('section', { key: 'empty', class: 'empty', 'aria-labelledby': 'h-empty' }, h('img', { src: '/app/mascot.svg', alt: '' }),
      h('h2', { id: 'h-empty' }, 'Find something to watch'), h('p', null, ideas.length ? 'Search above for a movie or show, or start with one of these.' : 'Search above for a movie or show.')));
  }
  if (ideas.length) secs.push(h(Section, { key: 'ideas', id: 'ideas', title: 'Ideas for you', extra: h('a', { href: '#/ideas' }, 'See all') }, h('ul', { class: 'ideas-row' }, keyed(ideas, (x) => x.media_id, (x, key) => h(IdeaCard, { key, x })))));
  return h('div', { class: 'stack' }, secs);
}
function paintHome() { if (S.feed) setView({ t: 'home' }); }
const onHome = () => parseHash().path === '/';
async function loadFeed() {
  if (S.out || feedBusy) return;
  feedBusy = true;
  try {
    const r = await api('/v1/feed');
    if (r.ok) { S.feed = r.data; S.tmdb = !!r.data.tmdb_backup; if (onHome()) paintHome(); bump(); }
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
  setView(hint('Your list will show up here as soon as Arr is back.'));
  clearTimeout(retryT);
  retryT = setTimeout(() => { if (t === token) route(); }, 5000);
}
async function renderHome(t) {
  if (S.feed) paintHome(); else setView(hint('Loading…'));
  const ok = await loadFeed();
  if (t !== token) return;
  if (!ok && !S.feed) return failed(t);
  const had = S.ideas;
  await loadIdeas();
  if (t === token && S.ideas !== had) paintHome();
}

// ---------- ideas ----------
function IdeasPage() {
  const d = V.d;
  const note = d.status === 'disabled' ? "Ideas aren't switched on for this Arr." : d.status === 'unavailable' && !d.items.length ? "Ideas aren't available right now. Check back soon." : !d.items.length ? 'No ideas yet. Check back soon.' : null;
  return h(Section, { id: 'ideas', title: 'Popular this week' }, note ? h(Hint, { text: note }) : h(List, null, keyed(d.items, (x) => x.media_id, (x, key) => h(IdeaRow, { key, x }))));
}
async function renderIdeas(t) {
  setView(hint('Loading…'));
  const d = await loadIdeas();
  if (t !== token) return;
  if (!d) return failed(t);
  setView({ t: 'ideas', d });
}

// ---------- search ----------
async function renderSearch(q, t) {
  q = q.trim();
  if (q.length < 2) return setView(hint('Type at least two letters to search.'));
  const seq = ++searchSeq;
  searchAbort?.abort();
  const ac = (searchAbort = new AbortController());
  if (V.t === 'search') { V.busy = true; bump(); } else setView(hint('Searching…', 'status'));
  const get = (kind) => api(`/v1/discover?kind=${kind}&q=${encodeURIComponent(q)}`, { signal: ac.signal });
  const [m, s] = await Promise.allSettled([get('movie'), get('series')]);
  if (seq !== searchSeq || t !== token) return;
  const ok = (r) => r.status === 'fulfilled' && r.value.ok;
  if (!ok(m) && !ok(s)) { setView(hint('Search will work again as soon as Arr is back.', 'status')); return; }
  const movies = ok(m) ? m.value.data.results || [] : [];
  const shows = ok(s) ? s.value.data.results || [] : [];
  const moviesOff = (S.feed && S.feed.movies_enabled === false) || (ok(m) && (m.value.data.providers || []).some((p) => p.name === 'plex' && p.status === 'not_configured'));
  const rows = [];
  for (let i = 0; i < Math.max(movies.length, shows.length); i++) {
    if (movies[i]) rows.push({ x: movies[i], label: 'Movie' });
    if (shows[i]) rows.push({ x: shows[i], label: 'Show' });
  }
  const notes = [];
  if (moviesOff) notes.push("Movie search isn't set up yet.");
  else if (!ok(m)) notes.push("Movie results aren't available right now.");
  if (!ok(s)) notes.push("Show results aren't available right now.");
  setView({ t: 'search', q, rows, notes, busy: false });
}
function SearchRow({ x, label }) {
  // A movie is one tap; a show opens its page to pick what to get.
  if (label === 'Show') {
    return h('li', { class: 'item top-align' }, h(Tile, { title: x.title, mediaId: x.media_id }),
      h('div', { class: 'what' }, h(Title, { title: x.title, id: x.media_id }), h('p', { class: 'meta' }, joinDot(x.year, label))),
      chooseLink({ media_id: x.media_id, title: x.title, in_library: x.in_catalog && x.monitored }));
  }
  const it = { id: x.media_id, title: x.title, base: x.has_file ? 'inplex' : x.in_catalog && x.monitored ? 'added' : 'add' };
  return h('li', { class: 'item top-align' }, h(Tile, { title: x.title, mediaId: x.media_id }),
    h('div', { class: 'what' }, h(Title, { title: x.title, id: x.media_id }), h('p', { class: 'meta' }, joinDot(x.year, label)), addMsg(it)), addBtn(it));
}
function SearchResults() {
  const { q, rows, notes, busy } = V;
  return h('div', { 'data-results': '', class: busy ? 'busy' : null },
    h(Section, { id: 'res', title: 'Results' }, rows.length ? h(List, null, keyed(rows, (r) => r.label + r.x.media_id, (r, key) => h(SearchRow, { key, x: r.x, label: r.label })))
      : h(Hint, { text: `Nothing found for “${q}”. Check the spelling or try fewer words.` })),
    notes.map((t) => h(Hint, { key: t, text: t })));
}

// ---------- library ----------
function libraryRow(x, mv) {
  const title = x.title || 'Untitled';
  const mid = mv ? (x.tmdb_id && `movie:tmdb:${x.tmdb_id}`) : x.tvdb_id ? `series:tvdb:${x.tvdb_id}` : x.tvmaze_id ? `series:tvmaze:${x.tvmaze_id}` : null;
  return { key: mid || title, el: (key) => h('li', { key, class: 'item' }, h(Tile, { title, mediaId: mid }), h('div', { class: 'what' }, h(Title, { title, id: mid }),
    h('p', { class: 'meta' }, joinDot(x.year, mv ? 'Movie' : 'Show', mv && x.has_file === false && 'Not in Plex yet'))),
    !(mv && x.has_file === false) && plexLink(title)) };
}
function Library({ t }) {
  const [kind, setKind] = useState(S.kind);
  const [fv, setFv] = useState(S.filter);
  const [st, setSt] = useState({ items: [], total: 0, status: 'Loading…' });
  const m = useRef({ seq: 0, items: [], ft: 0 }).current;
  async function load(reset) {
    const my = ++m.seq;
    if (reset) { m.items = []; setSt({ items: [], total: 0, status: 'Loading…' }); }
    const qs = `limit=50&offset=${m.items.length}` + (S.filter.trim() ? '&q=' + encodeURIComponent(S.filter.trim()) : '');
    try {
      const r = await api(`/v1/library/${S.kind}?${qs}`);
      if (my !== m.seq || t !== token) return;
      if (!r.ok) { setSt((s) => ({ ...s, status: "Couldn't load the library just now." })); return; }
      m.items = m.items.concat(r.data.items || []);
      setSt({ items: m.items, total: r.data.total || 0, status: m.items.length ? '' : S.filter.trim() ? `No matches for “${S.filter.trim()}”.` : 'Nothing here yet. Add something from the search above.' });
    } catch { if (my === m.seq && t === token && !S.out) { setSt((s) => ({ ...s, status: '' })); if (!m.items.length) failed(t); } }
  }
  useEffect(() => { load(true); return () => clearTimeout(m.ft); }, []);
  const mv = kind === 'movies';
  return h(Section, { id: 'lib', title: 'Library' }, h('div', null,
    h('div', { class: 'bar' },
      h('div', { class: 'seg', role: 'group', 'aria-label': 'What to show' },
        [['movies', 'Movies'], ['series', 'Shows']].map(([k, l]) =>
          h('button', { key: k, type: 'button', 'aria-pressed': String(kind === k), onClick: () => { S.kind = k; setKind(k); load(true); } }, l))),
      h('input', {
        type: 'search', class: 'filter', placeholder: 'Filter your library', 'aria-label': 'Filter your library', autocomplete: 'off', value: fv,
        onInput: (e) => { const v = e.target.value; setFv(v); clearTimeout(m.ft); m.ft = setTimeout(() => { S.filter = v; load(true); }, 250); },
      })),
    h('p', { class: 'hint', role: 'status' }, st.status || null),
    h('ul', { class: 'list' }, keyed(st.items.map((x) => libraryRow(x, mv)), (r) => r.key, (r, key) => r.el(key))),
    h('div', { class: 'more' }, st.items.length < st.total ? [
      h('button', { key: 'b', type: 'button', class: 'btn ghost', onClick: () => load(false) }, 'Show more'),
      h('span', { key: 's' }, `Showing ${st.items.length} of ${st.total}`)] : null)));
}

// ---------- title pages (a show or a movie, as a sheet over the current screen) ----------
const P = { shown: false, id: '', kind: '', token: 0, seq: 0, data: null, err: '', seasons: new Map(), open: new Set(), ask: '', busy: new Set(), msg: new Map(), more: false, followBusy: false, pollT: 0, askT: 0, opener: null, scrollY: 0, canBack: false, opt: null };
let bgHash = '';
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
document.addEventListener('click', (e) => {
  const a = e.target.closest && e.target.closest('a[href^="#/show/"],a[href^="#/movie/"]');
  if (a) P.opener = a;
}, true);

function openTitle(t) {
  if (P.shown && P.id === t.id) return;
  const first = !P.shown;
  if (first) {
    P.canBack = !!bgHash; // came from a screen of the app: closing goes back to it
    if (!bgHash) renderPath('/', '', '#/'); // a link straight to a title: home sits underneath
    P.scrollY = window.scrollY;
    P.shown = true; // the page behind goes inert with the same redraw
  }
  clearTimeout(P.pollT); clearTimeout(P.askT);
  Object.assign(P, { id: t.id, kind: t.kind, token: P.token + 1, data: null, err: '', seasons: new Map(), open: new Set(), ask: '', busy: new Set(), msg: new Map(), more: false, followBusy: false, opt: freshOpt() });
  after(() => {
    const panel = $('#panel');
    panel.scrollTop = 0;
    if (first) panel.querySelector('.close')?.focus({ preventScroll: true });
  });
  loadTitle(false);
}
function closeSheet() {
  if (!P.shown) return;
  clearTimeout(P.pollT); clearTimeout(P.askT);
  P.token++;
  P.shown = false;
  document.title = pageName(parseHash(bgHash).path) + ' · Arr';
  after(() => {
    window.scrollTo(0, P.scrollY);
    const o = P.opener, fk = o && o.dataset.fk, main = $('#view');
    const back = (o && o.isConnected && o) || (fk && main.querySelector(`[data-fk="${CSS.escape(fk)}"]`)) || main;
    back.focus({ preventScroll: true });
  });
}
// The person closed it: go back to where they were, or home if the link was the way in.
function dismiss() {
  if (P.canBack) history.back();
  else { history.replaceState(null, '', bgHash || '#/'); route(); }
}
document.addEventListener('keydown', (e) => {
  if (e.key !== 'Escape' || !P.shown) return;
  e.preventDefault();
  if (P.ask) { P.ask = ''; bump(); } else dismiss();
});
// Tab and Shift-Tab stay inside the sheet.
document.addEventListener('keydown', (e) => {
  if (e.key !== 'Tab' || !P.shown) return;
  const panel = $('#panel');
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
    if (!r.ok) { if (!P.data) P.err = errText(r); await seasons; if (!stale()) bump(); return; }
    P.data = r.data; P.err = '';
    document.title = `${r.data.title} · Arr`;
  } catch {
    if (stale()) return;
    if (!P.data) P.err = "Couldn't reach Arr just now.";
  }
  await seasons;
  if (stale()) return;
  bump();
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
  if (!P.shown || document.hidden || !P.data) return;
  const moving = P.kind === 'movie' ? P.data.state === 'on_its_way' : P.data.seasons.some((s) => s.state === 'on_its_way');
  if (!moving) return;
  const tk = P.token;
  P.pollT = setTimeout(() => {
    if (tk !== P.token) return;
    if (P.busy.size || P.followBusy) schedulePoll(); else loadTitle(true);
  }, 5000);
}

function Sheet() {
  const open = P.shown;
  const body = !open ? null : P.data ? (P.kind === 'show' ? showBody() : movieBody()) : P.err ? errorBody() : skeleton();
  return h('div', { id: 'sheet', class: 'sheet', hidden: !open, onClick: (e) => { if (e.target === e.currentTarget) dismiss(); } },
    h('div', { class: 'panel', id: 'panel', role: 'dialog', 'aria-modal': 'true', 'aria-labelledby': 'sheet-title', tabindex: '-1' },
      open ? [
        h('div', { key: 'bar', class: 'sheet-bar' }, h('button', { type: 'button', class: 'btn ghost close', 'aria-label': 'Close', 'data-fk': 'close', onClick: dismiss }, ic('close'))),
        h('div', { key: 'body', class: 'sheet-body' }, body)] : null));
}
function skeleton() {
  const bar = (w, extra) => h('div', { class: 'sk', style: { width: w, ...extra } });
  return h('div', { key: 'sk', class: 'stack', 'aria-busy': 'true' },
    h('span', { id: 'sheet-title', class: 'sr' }, 'Loading'),
    h('div', { class: 'hero' }, h('div', { class: 'tile big sk-tile' }), h('div', { class: 'hero-t' }, bar('80%'), bar('50%'))),
    bar('100%'), bar('70%'),
    h('ul', { class: 'list' }, [1, 2, 3].map((i) => h('li', { key: i, class: 'item' }, h('div', { class: 'what' }, bar('40%'), bar('65%', { marginTop: '8px' }))))));
}
function errorBody() {
  return h('div', { key: 'err', class: 'stack' }, h('span', { id: 'sheet-title', class: 'sr' }, 'Something went wrong'),
    h(Hint, { text: P.err, role: 'status' }),
    h('button', { type: 'button', class: 'btn ghost', 'data-fk': 'retry', onClick: () => { P.err = ''; bump(); loadTitle(false); } }, ic('retry'), 'Try again'));
}
function hero(title, line) {
  return h('header', { class: 'hero' }, h(Tile, { title, mediaId: P.data.media_id, big: true }),
    h('div', { class: 'hero-t' }, h('h1', { id: 'sheet-title' }, title), line && h('p', { class: 'meta' }, line)));
}
function Summary({ text }) {
  const el = useRef(null), [over, setOver] = useState(false);
  // The clamped text gets a More button only if it really is cut off.
  useEffect(() => {
    if (P.more) return;
    const a = requestAnimationFrame(() => { const e = el.current; if (e) setOver(e.scrollHeight > e.clientHeight + 1); });
    return () => cancelAnimationFrame(a);
  });
  if (!text) return null;
  return h('div', null,
    h('p', { ref: el, class: 'sum' + (P.more ? '' : ' cl') }, text),
    h('button', { type: 'button', class: 'link more-sum', 'aria-expanded': String(P.more), 'data-fk': 'more', hidden: !(P.more || over), onClick: () => { P.more = !P.more; bump(); } }, P.more ? 'Less' : 'More'));
}
function msgFor(key) {
  const m = P.msg.get(key);
  return h(Msg, { st: m ? { msg: m.text, err: m.err } : {} });
}

// One action: Get, or Stop (which asks first). `key` identifies it on the page.
function actionNode(key, kind, label, noun, scope) {
  const busy = P.busy.has(key);
  if (kind === 'stop' && P.ask === key && !busy) {
    return h('span', { class: 'ask' }, h('span', { class: 'ask-t' }, 'Stop getting it?'),
      h('button', { type: 'button', class: 'btn', 'data-fk': 'act:' + key, 'aria-label': `Confirm: stop getting ${noun}`, onClick: () => act(key, 'stop', scope) }, 'Confirm'),
      h('button', { type: 'button', class: 'btn ghost', 'aria-label': `Keep getting ${noun}`, onClick: () => { P.ask = ''; bump(); } }, 'Keep'));
  }
  const text = busy ? (kind === 'get' ? 'Getting…' : 'Stopping…') : label;
  return h('button', {
    type: 'button', class: 'btn' + (kind === 'stop' ? ' ghost' : ''), 'data-fk': 'act:' + key, 'data-s': busy ? 'busy' : null,
    'aria-disabled': busy ? 'true' : 'false', 'aria-label': `${label} ${noun}`,
    onClick: () => {
      if (P.busy.has(key)) return;
      if (kind === 'get') return act(key, 'get', scope);
      P.ask = key; bump();
      clearTimeout(P.askT);
      P.askT = setTimeout(() => { if (P.ask === key) { P.ask = ''; bump(); } }, 6000);
    },
  }, !busy && kind === 'get' && ic('plus'), text);
}
async function act(key, kind, scope) {
  const tk = P.token;
  P.seq++; // an older read must not overwrite what this changes
  P.busy.add(key); P.ask = ''; P.msg.delete(key); bump();
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
  bump();
  await loadTitle(true);
}
async function toggleFollow() {
  if (P.followBusy) return;
  const want = !P.data.following, tk = P.token;
  P.seq++;
  P.followBusy = true; P.data.following = want; P.msg.delete('follow'); bump();
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
    bump();
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
    bump();
    if (o.copiesOpen) loadCopies();
    return;
  }
  o.saving = true; bump();
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
  P.msg.set('opt', res); say(res.text); bump();
  P.seq++;
  await loadTitle(true);
  if (tk !== P.token) return;
  if (!res.err) { o.level = null; bump(); }
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
  bump();
  const lv = pickedLevel();
  let c;
  try {
    const r = await api(`/v1/copies/${enc(scope)}` + (lv ? `?level=${lv}` : ''));
    c = r.ok ? { st: 'ready', scope, data: r.data } : { st: 'error', scope };
  } catch { c = { st: 'error', scope }; }
  // Only the newest lookup counts: an older answer is for a level or season no longer shown.
  if (tk !== P.token || o.gen !== gen) return;
  o.copies = c; bump();
  if (c.st === 'ready') say(c.data.copies.length ? plural(c.data.copies.length, 'copy', 'copies') + ' found.' : "Arr couldn't find any copies right now.");
}
function toggleCopies() {
  const o = P.opt;
  o.copiesOpen = !o.copiesOpen;
  if (o.copiesOpen && o.copies.st !== 'ready' && o.copies.st !== 'loading') loadCopies(); else bump();
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
  o.picking = c.id; P.msg.delete('copy'); P.seq++; bump();
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
  bump();
  await loadTitle(true);
}
function levelChoice(l) {
  const on = shownLevel() === l.level;
  const size = l.size_bytes ? fmtSize(l.size_bytes) : '';
  return h('label', { key: l.level, class: 'lv-opt' },
    h('input', { type: 'radio', name: 'level', value: l.level, checked: on, disabled: P.opt.saving, 'data-fk': 'lv:' + l.level, onChange: () => setLevel(l.level) }),
    h('span', { class: 'lv-t' }, l.label), h('span', { class: 'lv-r' }, l.resolution), size && h('span', { class: 'lv-s' }, size));
}
function copyRow(c) {
  const line = [c.resolution, c.source].filter(Boolean).join(' · ') || 'Unknown quality';
  const speed = { fast: 'Fast', ok: 'Medium speed', slow: 'Slow' }[c.speed];
  const busy = P.opt.picking === c.id;
  const label = `${line}${c.size_bytes ? ', ' + fmtSize(c.size_bytes) : ''}`;
  return h('li', { key: c.id, class: 'item copy' + (c.recommended ? ' rec' : '') },
    h('div', { class: 'what' },
      h('h3', null, line),
      h('p', { class: 'meta' }, joinDot(fmtSize(c.size_bytes), speed)),
      c.recommended && h('span', { class: 'tag' }, ic('check'), 'Recommended')),
    h('button', { type: 'button', class: 'btn' + (c.recommended ? '' : ' ghost'), 'data-fk': 'copy:' + c.id, 'data-s': busy ? 'busy' : null, 'aria-disabled': P.opt.picking ? 'true' : 'false', 'aria-label': `Get this copy: ${label}${c.recommended ? ', recommended' : ''}`, onClick: () => pickCopy(c) }, busy ? 'Getting…' : 'Get this one'));
}
function copiesNode() {
  const c = P.opt.copies;
  if (c.st === 'loading' || (c.st === 'idle' && P.opt.saving)) {
    return h('div', { key: 'load', class: 'copies', 'aria-busy': 'true' }, h('span', { class: 'sr', role: 'status' }, 'Looking for copies'),
      h('ul', { class: 'list' }, [1, 2, 3].map((i) => h('li', { key: i, class: 'item' }, h('div', { class: 'what' }, h('div', { class: 'sk', style: { width: '45%' } }), h('div', { class: 'sk', style: { width: '30%', marginTop: '8px' } })), h('div', { class: 'sk', style: { width: '92px', height: '44px', borderRadius: '12px' } })))));
  }
  const retry = h('button', { type: 'button', class: 'btn ghost', 'data-fk': 'copies-retry', onClick: loadCopies }, ic('retry'), 'Try again');
  if (c.st === 'error') return h('div', { key: 'err', class: 'copies' }, h(Hint, { text: "Couldn't look for copies just now.", role: 'status' }), retry);
  if (c.st !== 'ready') return null;
  const n = c.data.skipped.reduce((a, x) => a + x.count, 0);
  const skipped = n ? h('p', { class: 'hint skipped' }, `Skipped ${plural(n, 'copy', 'copies')}: ` + c.data.skipped.map((x) => `${x.count} ${x.reason}`).join(', ') + '.') : null;
  if (!c.data.copies.length) return h('div', { key: 'none', class: 'copies' }, h(Hint, { text: "Arr couldn't find any copies right now.", role: 'status' }), skipped, retry);
  return h('div', { key: 'ready', class: 'copies' }, h('ul', { class: 'list' }, c.data.copies.map(copyRow)), skipped);
}
function optionsPanel() {
  const o = P.opt, d = P.data, lv = d.levels.length > 0;
  const cur = lv ? d.levels.find((l) => l.level === shownLevel()) : null;
  const sub = cur ? `Quality: ${cur.label} (${cur.resolution})` : 'Choose a copy yourself';
  const season = P.kind === 'show' && o.copiesOpen && d.seasons.length > 1
    ? h('label', { class: 'pick-season' }, h('span', null, 'Season'),
      h('select', { 'data-fk': 'season-pick', onChange: (e) => { o.season = Number(e.target.value); o.copies = { st: 'idle' }; loadCopies(); } },
        d.seasons.map((s) => h('option', { key: s.number, value: s.number, selected: s.number === (o.season ?? defaultSeason()) }, 'Season ' + s.number)))) : null;
  const fs = lv && h('fieldset', { class: 'lv', disabled: o.saving, 'aria-busy': o.saving ? 'true' : null },
    h('legend', null, 'Quality'),
    h('p', { class: 'lv-note' }, P.kind === 'movie' ? 'Estimated size of this movie at each level.' : 'Estimated size of each episode at each level.'),
    h('div', { class: 'lv-row' }, d.levels.map(levelChoice)),
    h('p', { class: 'lv-note' }, shownLevel() === 'best' ? 'Best takes 1080p when there is no 4K copy. ' : '', 'This never removes or replaces anything already in Plex. It only changes what Arr looks for next.'),
    !d.tracked && o.level ? h('p', { class: 'lv-note' }, 'Arr will use this when you tap Get.') : null,
    msgFor('opt'));
  return h('section', { class: 'opts', 'aria-label': 'Download options' },
    h('button', { type: 'button', class: 'opts-head', 'aria-expanded': String(o.open), 'aria-controls': 'opts-body', 'data-fk': 'opts', onClick: () => { o.open = !o.open; bump(); } },
      h('span', { class: 'what' }, h('span', { class: 'sw-t' }, 'Download options'), h('span', { class: 'sw-m' }, sub)), ic('down')),
    o.open && h('div', { id: 'opts-body', class: 'opts-body' },
      fs,
      h('div', { class: 'copies-box' },
        h('button', { type: 'button', class: 'link', 'aria-expanded': String(o.copiesOpen), 'aria-controls': 'copies', 'data-fk': 'copies-open', onClick: toggleCopies }, 'Choose a copy yourself', ic('right')),
        h('div', { id: 'copies' }, o.copiesOpen ? [season, copiesNode()] : null),
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
function loadSeasonNow(n) {
  P.seasons.set(n, { loading: true });
  const tk = P.token;
  loadSeason(n, tk, P.seq).then(() => { if (tk === P.token) bump(); });
}
function toggleSeason(n) {
  if (P.open.has(n)) { P.open.delete(n); return bump(); }
  P.open.add(n);
  if (!(P.seasons.get(n) || {}).eps) loadSeasonNow(n);
  bump();
}
function episodesNode(n) {
  const st = P.seasons.get(n) || {};
  if (st.eps) {
    if (!st.eps.length) return h(Hint, { text: 'No episodes are listed for this season yet.' });
    return h('ul', { class: 'eps' }, st.eps.map(episodeRow));
  }
  if (st.err) {
    return h('div', { class: 'eps-err' }, h(Hint, { text: st.err, role: 'status' }),
      h('button', { type: 'button', class: 'btn ghost', 'data-fk': 'eretry:' + n, onClick: () => { loadSeasonNow(n); bump(); } }, ic('retry'), 'Try again'));
  }
  return h('div', { class: 'eps', 'aria-busy': 'true' }, [1, 2, 3].map((i) => h('div', { key: i, class: 'ep' }, h('div', { class: 'sk', style: { width: '28px' } }), h('div', { class: 'what' }, h('div', { class: 'sk', style: { width: '55%' } })))));
}
function episodeRow(e) {
  const key = 'e:' + e.media_id, title = e.title || `Episode ${e.number}`;
  const kind = e.state === 'none' ? 'get' : e.state === 'wanted' || e.state === 'on_its_way' ? 'stop' : null;
  const fact = e.state === 'in_plex' ? h('span', { class: 'tag' }, ic('check'), 'In Plex')
    : e.state === 'on_its_way' ? 'On its way' + (typeof e.progress === 'number' ? ` · ${pct(e.progress)}%` : '')
      : e.state === 'wanted' ? 'Arr is looking for it'
        : e.state === 'upcoming' ? (e.air_date ? `Airs ${fmtDate(e.air_date)}` : 'Not aired yet')
          : e.air_date ? `Aired ${fmtDate(e.air_date)}` : '';
  return h('li', { key: e.media_id, class: 'ep' }, h('span', { class: 'num', 'aria-hidden': 'true' }, e.number),
    h('div', { class: 'what' }, h('h4', { title }, title), fact && h('div', { class: 'meta' }, fact), msgFor(key)),
    kind && h('div', { class: 'act' }, actionNode(key, kind, kind === 'get' ? 'Get' : 'Stop', `episode ${e.number}, ${title}`, e.media_id)));
}
function seasonItem(s) {
  const n = s.number, key = 's:' + n, open = P.open.has(n), a = seasonAction(s);
  const scope = P.data.media_id.replace(/^series:/, 'season:') + ':' + n;
  return h('li', { key: n, class: 'item sea' },
    h('div', { class: 'sea-head' },
      h('button', { type: 'button', class: 'sbtn', 'aria-expanded': String(open), 'aria-controls': 'eps-' + n, 'data-fk': 'season:' + n, onClick: () => toggleSeason(n) },
        h('span', { class: 'what' }, h('span', { class: 't' }, 'Season ' + n), h('span', { class: 'meta' }, seasonFact(s))), ic('down')),
      a && h('div', { class: 'act' }, actionNode(key, a[0], a[1], 'Season ' + n, scope))),
    msgFor(key),
    open && h('div', { id: 'eps-' + n }, episodesNode(n)));
}
function showBody() {
  const d = P.data, on = d.following;
  return h('div', { key: 'show', class: 'stack' },
    hero(d.title, joinDot(d.year, d.network, d.status)),
    h(Summary, { text: d.summary }),
    h('div', null, h('button', { type: 'button', role: 'switch', class: 'switch', 'aria-checked': String(on), 'aria-labelledby': 'sw-t', 'data-fk': 'follow', onClick: toggleFollow },
      h('span', { class: 'what' }, h('span', { class: 'sw-t', id: 'sw-t' }, 'Get new episodes as they air'),
        h('span', { class: 'sw-m' }, on ? 'Arr gets each new episode when it airs.' : 'Off. Arr is not looking for new episodes.')),
      h('span', { class: 'track', 'aria-hidden': 'true' }, h('i'))), msgFor('follow')),
    optionsPanel(),
    h(Section, { id: 'seasons', title: 'Seasons' }, d.seasons.length ? h('ul', { class: 'list' }, d.seasons.map(seasonItem)) : h(Hint, { text: 'No seasons are listed for this show yet. Check back soon.' })));
}

// ----- the movie page
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
  return h('div', { key: 'movie', class: 'stack' },
    hero(d.title, joinDot(d.year, d.runtime_minutes && fmtRuntime(d.runtime_minutes))),
    h(Summary, { text: d.summary }),
    h('section', { class: 'item state' + (st === 'in_plex' ? ' arrived' : ''), 'aria-label': 'Status' },
      h('div', { class: 'what' },
        st === 'in_plex' ? h('span', { class: 'tag' }, ic('check'), 'In Plex') : null,
        h('p', { class: 'st' }, st === 'in_plex' ? 'Ready to watch' : line),
        st === 'on_its_way' ? h(Journey, { stage: p == null ? 0 : 1, p, fkey: 'movie:' + d.media_id }) : null,
        msgFor(key)),
      h('div', { class: 'act' }, control)),
    optionsPanel());
}

// ---------- sign in ----------
function showSignin(data) {
  S.out = true; token++;
  clearTimeout(pollT); clearTimeout(retryT);
  setBanner('');
  document.title = 'Sign in · Arr';
  setView({ t: 'signin', google: !!(data && data.signin && data.signin.google) });
}
function Signin({ google }) {
  return h('section', { class: 'signin' }, h('img', { src: '/app/mascot.svg', alt: '' }), h('h1', null, 'Sign in to Arr'),
    google ? [h('p', { key: 'p' }, 'Use your Google account to see what is coming to Plex.'), h('a', { key: 'a', class: 'btn', href: '/app/signin' }, 'Continue with Google')]
      : h('p', null, 'Ask the person who runs Arr for a sign-in link.'));
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
  if (P.shown) {
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
  R.path = path;
  searchAbort?.abort();
  document.title = pageName(path) + ' · Arr';
  $('#q').value = path === '/search' ? q : '';
  if (path === '/search') renderSearch(q, t);
  else if (path === '/ideas') renderIdeas(t);
  else if (path === '/library') { setView({ t: 'library', tok: t }); }
  else renderHome(t);
}

// ---------- the page ----------
function View() {
  switch (V.t) {
    case 'home': return h(Home);
    case 'ideas': return h(IdeasPage);
    case 'search': return h(SearchResults);
    case 'library': return h(Library, { key: V.tok, t: V.tok });
    case 'signin': return h(Signin, { google: V.google });
    default: return h(Hint, { text: V.text, role: V.role });
  }
}

// ---- Use Arr on your phone: the address, a QR code and how to put Arr on the home screen
const REMOTE_DOCS = 'https://github.com/newtonharry/Arr/blob/main/docs/REMOTE.md';

// The code is always dark on white, whatever the theme: phones scan it that way.
function QrCode({ text }) {
  const q = qrcodegen.QrCode.encodeText(text, qrcodegen.QrCode.Ecc.MEDIUM), pad = 3;
  let d = '';
  for (let y = 0; y < q.size; y++) for (let x = 0; x < q.size; x++) if (q.getModule(x, y)) d += `M${x + pad},${y + pad}h1v1h-1z`;
  return h('svg', { viewBox: `0 0 ${q.size + pad * 2} ${q.size + pad * 2}`, class: 'qr', role: 'img', 'aria-label': 'QR code for ' + text, 'shape-rendering': 'crispEdges' },
    h('rect', { width: '100%', height: '100%', fill: '#fff' }), h('path', { d, fill: '#000' }));
}
async function copyText(node, text) {
  try { await navigator.clipboard.writeText(text); return true; } catch { /* not allowed here: select it instead */ }
  const r = document.createRange(); r.selectNodeContents(node);
  const sel = getSelection(); sel.removeAllRanges(); sel.addRange(r);
  try { return document.execCommand('copy'); } catch { return false; }
}
function Phone({ url }) {
  const [msg, setMsg] = useState('');
  const code = useRef(null);
  if (!url) {
    return h('div', { class: 'stack' },
      h('p', null, 'Arr works on this computer for now. To use it on your phone, turn on remote access: run the installer again with ',
        h('code', null, '--remote'), ' (', h('a', { href: REMOTE_DOCS, target: '_blank', rel: 'noopener noreferrer' }, 'how it works'), ').'),
      h('p', null, 'Once that is done, this page shows an address and a QR code to scan.'));
  }
  return h('div', { class: 'stack' },
    h('p', null, 'Open this address on your phone and sign in with Google:'),
    h('div', { class: 'addr' }, h('code', { ref: code }, url),
      h('button', { type: 'button', class: 'btn ghost', onClick: async () => setMsg((await copyText(code.current, url)) ? 'Copied' : 'Press Ctrl+C to copy') }, 'Copy'),
      h('span', { class: 'msg', role: 'status' }, msg || null)),
    h(QrCode, { text: url }),
    h('div', { class: 'how' },
      h('p', null, h('b', null, 'iPhone: '), 'open the address in Safari, tap Share, then Add to Home Screen.'),
      h('p', null, h('b', null, 'Android: '), 'open the address in Chrome, tap the menu (three dots), then Add to Home screen or Install app.')));
}
function Footer() {
  const [open, setOpen] = useState(false);
  const shown = useRef(false);
  useEffect(() => {
    if (shown.current && open) $('#phone').scrollIntoView({ block: 'nearest', behavior: 'smooth' });
    shown.current = true;
  }, [open]);
  const me = S.me || {};
  return h('footer', { class: 'foot', id: 'foot', hidden: !S.me || S.out, inert: P.shown },
    h('span', { id: 'who' }, S.me ? (me.email ? 'Signed in as ' + me.email : 'Signed in') : null),
    h('button', { type: 'button', class: 'link', id: 'phone-btn', 'aria-expanded': String(open), 'aria-controls': 'phone', onClick: () => setOpen(!open) }, 'Use Arr on your phone'),
    h('button', { type: 'button', class: 'link', id: 'signout', onClick: signOut }, 'Sign out'),
    h('section', { class: 'phone', id: 'phone', 'aria-label': 'Use Arr on your phone', hidden: !open }, open && S.me ? h(Phone, { url: me.public_url || null }) : null),
    h('p', { class: 'credit' }, 'Movie details and posters from Plex',
      h('span', { id: 'tmdb-credit', hidden: !S.tmdb }, ' and ', h('img', { src: '/app/tmdb.svg', alt: 'TMDB', width: 85, height: 11 }), ' (Arr uses TMDB and the TMDB APIs but is not endorsed, certified, or otherwise approved by TMDB)'),
      ', show details and posters from ', h('a', { href: 'https://www.tvmaze.com', target: '_blank', rel: 'noopener noreferrer' }, 'TVmaze'), '.'));
}
async function signOut() {
  try { await api('/signout', { method: 'POST' }); } catch { /* signed out anyway */ }
  location.hash = ''; location.reload();
}

function App() {
  const [, set] = useState(0);
  useLayoutEffect(() => {
    rerender = () => set((n) => n + 1);
    return () => { rerender = () => {}; };
  }, []);
  // After every redraw: the sheet's scroll lock, focus a redraw took away, then what was waiting.
  useLayoutEffect(() => {
    document.body.classList.toggle('sheet-open', P.shown);
    const pk = pendingFk, a = document.activeElement;
    pendingFk = null;
    if (pk && pk.fk && (!a || a === document.body || !a.isConnected)) {
      $(pk.panel ? '#panel' : '#view')?.querySelector(`[data-fk="${CSS.escape(pk.fk)}"]`)?.focus({ preventScroll: true });
    }
    for (const fn of later.splice(0)) fn();
  });
  const inert = P.shown; // everything behind the sheet: out of reach of the keyboard and assistive tech
  return h(Fragment, null,
    h('a', { class: 'skip', href: '#view', inert }, 'Skip to content'),
    h('header', { class: 'top', id: 'top', inert },
      h('a', { class: 'brand', href: '#/', 'aria-label': 'Arr, home' }, h('img', { src: '/app/mascot.svg', alt: '', width: 34, height: 34 }), h('span', null, 'Arr')),
      h('nav', { id: 'tabs', 'aria-label': 'Main', hidden: S.out }, TABS.map(([p, l, i]) =>
        h('a', { key: p, href: '#' + p, 'aria-current': R.path === p ? 'page' : null }, ic(i), l))),
      h('form', { class: 'search', id: 'search', role: 'search', autocomplete: 'off', hidden: S.out, onSubmit: searchSubmit },
        h('span', { id: 'search-ico', 'aria-hidden': 'true' }, ic('search')),
        h('label', { class: 'sr', for: 'q' }, 'Find a movie or show'),
        h('input', { id: 'q', name: 'q', type: 'search', placeholder: 'Find a movie or show', enterkeyhint: 'search', autocapitalize: 'off', spellcheck: false, onInput: searchInput })),
      h('div', { id: 'banner', class: 'banner', role: 'status' }, S.banner || null)),
    h('main', { id: 'view', tabindex: '-1', inert }, h(View)),
    h(Footer),
    h(Sheet),
    h('div', { id: 'live', class: 'sr', 'aria-live': 'polite' }, S.live || null));
}

function searchSubmit(e) {
  e.preventDefault();
  const q = $('#q').value.trim();
  if (q.length >= 2) { clearTimeout(debounceT); go(q); }
}
function searchInput() {
  clearTimeout(debounceT);
  const q = $('#q').value.trim();
  debounceT = setTimeout(() => {
    if (q.length >= 2) go(q);
    else if (!q && parseHash().path === '/search') { history.replaceState(null, '', '#/'); route(); }
  }, 300);
}
function go(q) {
  const url = '#/search?q=' + encodeURIComponent(q);
  if (parseHash().path === '/search') { history.replaceState(null, '', url); bgHash = url; token++; renderSearch(q, token); }
  else location.hash = url;
}

async function boot() {
  document.addEventListener('visibilitychange', () => {
    if (!document.hidden && onHome()) loadFeed(); else clearTimeout(pollT);
    if (!document.hidden && P.shown) loadTitle(true);
  });
  window.addEventListener('hashchange', route);
  try {
    const r = await api('/me');
    if (r.ok) { S.me = r.data; bump(); }
  } catch {
    if (S.out) return;
    setView(hint('Your list will show up here as soon as Arr is back.'));
    setTimeout(boot2, 5000);
    return;
  }
  route();
  // The feed also says whether TMDB is used, which the footer credit needs on every screen.
  if (!onHome()) loadFeed();
}
async function boot2() {
  try {
    const r = await api('/me');
    if (r.ok) { S.me = r.data; bump(); route(); if (!onHome()) loadFeed(); }
  } catch { if (!S.out) setTimeout(boot2, 5000); }
}
document.body.replaceChildren();
render(h(App), document.body);
boot();
