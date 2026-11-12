// Arr web app. Preact (vendored under /app/vendor, no build step, no JSX). Server text only ever
// reaches the page as Preact children or attributes, never as markup.
import { h, render, Fragment } from '/app/vendor/preact.mjs';
import { useState, useEffect, useLayoutEffect, useRef } from '/app/vendor/hooks.mjs';

const $ = (s) => document.querySelector(s);
// The library order the person picked last, kept in this browser only.
const SORTS = [['added', 'Recently added'], ['title', 'Title A–Z'], ['size', 'Size']];
function savedSort() {
  try { const v = localStorage.getItem('arr.librarySort'); if (SORTS.some(([k]) => k === v)) return v; } catch { /* storage blocked: use the default */ }
  return 'added';
}
function saveSort(v) { try { localStorage.setItem('arr.librarySort', v); } catch { /* storage blocked: the choice lasts until the page closes */ } }
// Module state, read while rendering. Whatever changes it calls bump() to draw it again.
const S = { feed: null, ideas: null, ideasAt: 0, out: false, all: false, kind: 'movies', filter: '', sort: savedSort(), me: null, tmdb: false, banner: '', setup: false, live: '' };
let V = { t: 'hint', text: '' }; // what the main area shows
const R = { path: null }; // the screen under the sheet, for the active tab
const A = new Map(); // add / retry state by id, kept for the session
const fills = new Map();
// ---------- motion: it explains what just happened and never makes anyone wait ----------
// Only transform and opacity move. Nothing here is awaited by an action, and none of it runs when
// the person asked their device for less motion.
const RMQ = matchMedia('(prefers-reduced-motion: reduce)');
const reduced = () => RMQ.matches;
const EASE = 'cubic-bezier(.16,1,.3,1)', SPRING = 'cubic-bezier(.34,1.56,.64,1)', SOFT = 'cubic-bezier(.3,1.25,.5,1)';
const NOANIM = { finished: Promise.resolve(), cancel() {}, pause() {}, play() {} };
function anim(el, kf, opt) {
  if (!el || reduced() || !el.animate) return NOANIM;
  try { return el.animate(kf, { easing: EASE, ...opt }); } catch { return NOANIM; }
}
const settled = (a, fn) => a.finished.then(fn, fn);
const M = { fresh: false, drag: 0, tab: 0, celebrate: new Set() }; // fresh: the screen has not drawn its rows yet; drag: how far the sheet was pulled down
const onScreen = (r) => r.width > 0 && r.bottom > 0 && r.top < innerHeight && r.right > 0 && r.left < innerWidth;
const wide = () => matchMedia('(min-width:760px)').matches;
// The poster of the row or card an element sits in.
const tileOf = (el) => { const r = el && el.closest && (el.closest('li') || el.closest('.idea')); return r && r.querySelector('.tile'); };
// A copy of a poster flies from one place to another over everything, then goes away.
function fly(src, a, b) {
  if (reduced()) return NOANIM;
  const g = src.cloneNode(true), st = g.style;
  g.removeAttribute('id'); g.setAttribute('aria-hidden', 'true');
  Object.assign(st, { position: 'fixed', left: a.left + 'px', top: a.top + 'px', width: a.width + 'px', height: a.height + 'px', margin: '0', zIndex: '70', transformOrigin: '0 0', pointerEvents: 'none', visibility: 'visible', fontSize: a.width * 0.3 + 'px', borderRadius: a.width * 0.16 + 'px' });
  document.body.append(g);
  const an = anim(g, [{ transform: 'none' }, { transform: `translate(${b.left - a.left}px,${b.top - a.top}px) scale(${b.width / a.width},${b.height / a.height})` }], { duration: 440, easing: SOFT, fill: 'forwards' });
  settled(an, () => g.remove());
  setTimeout(() => g.remove(), 1200);
  return an;
}
// Rows that move (a download arriving, a card going, the list closing the gap) glide from where they
// were to where they are. bump() notes where the keyed rows are before a redraw, and the redraw plays it.
let snap = null;
function measureFlip() {
  const rects = new Map(), seen = new Map();
  for (const e of document.querySelectorAll('#view [data-flip]')) {
    const k = e.dataset.flip, r = e.getBoundingClientRect();
    seen.set(k, (seen.get(k) || 0) + 1);
    rects.set(k, { left: r.left, top: r.top + scrollY });
  }
  for (const [k, n] of seen) if (n > 1) rects.delete(k); // an id that appears twice can't be followed
  return { tok: token, t: V.t, rects };
}
function playFlip(sn) {
  const els = [...document.querySelectorAll('#view [data-flip]')];
  for (const e of els) { e._fl?.cancel(); e._fl = null; }
  if (sn.tok !== token || sn.t !== V.t) return;
  for (const e of els) {
    const a = sn.rects.get(e.dataset.flip);
    if (!a) continue;
    const r = e.getBoundingClientRect(), dx = a.left - r.left, dy = a.top - (r.top + scrollY);
    if (Math.abs(dx) < 1 && Math.abs(dy) < 1) continue;
    e._fl = anim(e, [{ transform: `translate(${dx}px,${dy}px)` }, { transform: 'none' }], { duration: 480, easing: SOFT });
  }
}
// A card that is going: it shrinks away first, then the others close the gap.
function leave(el) { return el ? settled(anim(el, [{ opacity: 1, transform: 'none' }, { opacity: 0, transform: 'scale(.9)' }], { duration: 200, fill: 'forwards' }), () => {}) : Promise.resolve(); }
// A short burst of confetti. At most 34 pieces, at most two bursts at once, every piece removed when it lands.
let bursts = 0;
function confetti(x, y, n = 24) {
  if (reduced() || bursts >= 2 || !document.body.animate) return;
  n = Math.min(n, 34); bursts++;
  const layer = document.createElement('div'), cols = ['#1c8576', '#ff7a5c', '#ffd166', '#4fd1bd', '#8ab4ff'];
  layer.className = 'cf-layer'; layer.setAttribute('aria-hidden', 'true');
  document.body.append(layer);
  let left = n, over = false;
  const end = () => { if (over) return; over = true; layer.remove(); bursts--; };
  for (let i = 0; i < n; i++) {
    const p = document.createElement('i'), a = Math.random() * Math.PI * 2, d = 36 + Math.random() * 74, dx = Math.cos(a) * d, dy = Math.sin(a) * d - 34, r = (Math.random() - 0.5) * 720;
    p.className = 'cf';
    Object.assign(p.style, { left: x - 3 + 'px', top: y - 5 + 'px', background: cols[i % 5] });
    if (i % 3 === 0) Object.assign(p.style, { borderRadius: '50%', height: '7px' });
    layer.append(p);
    const an = p.animate([{ transform: 'translate(0,0) rotate(0) scale(.3)', opacity: 1 }, { transform: `translate(${dx}px,${dy}px) rotate(${r}deg) scale(1)`, opacity: 1, offset: 0.55 }, { transform: `translate(${dx * 1.15}px,${dy + 70}px) rotate(${r * 1.5}deg) scale(.9)`, opacity: 0 }], { duration: 900 + Math.random() * 500, easing: 'cubic-bezier(.2,.7,.3,1)' });
    settled(an, () => { p.remove(); if (--left === 0) end(); });
  }
  setTimeout(end, 2600);
}
const middle = (el) => { const r = el.getBoundingClientRect(); return [r.left + r.width / 2, r.top + r.height / 2]; };
// A title that landed in Plex while the app was open: a sweep of light over the poster, a hop, confetti.
function celebrate(li) {
  if (li === document.body || !li.isConnected || reduced()) return;
  const tile = li.querySelector('.tile');
  if (tile) { tile.classList.remove('sweep'); void tile.offsetWidth; tile.classList.add('sweep'); setTimeout(() => tile.classList.remove('sweep'), 1000); }
  anim(li, [{ transform: 'none' }, { transform: 'translateY(-8px) scale(1.02)' }, { transform: 'none' }], { duration: 600, easing: SPRING });
  anim(li.querySelector('.tag'), [{ transform: 'scale(.4)', opacity: 0 }, { transform: 'scale(1)', opacity: 1 }], { duration: 480, easing: SPRING });
  confetti(...middle(tile || li), 34);
}
// Which arrivals have been celebrated: kept in this browser, so a reload never plays one twice.
const SEEN_KEY = 'arr.celebrated', seen = new Set();
let seenLoaded = false, liveFeed = false;
const arrKey = (it) => it.media_id + '|' + it.at;
function noteArrivals(feed) {
  if (!seenLoaded) { seenLoaded = true; try { for (const k of JSON.parse(localStorage.getItem(SEEN_KEY) || '[]')) seen.add(k); } catch { /* nothing remembered */ } }
  const fresh = (feed.arrived || []).map(arrKey).filter((k) => !seen.has(k));
  const first = !liveFeed;
  liveFeed = true;
  if (!fresh.length) return;
  for (const k of fresh) seen.add(k);
  try { localStorage.setItem(SEEN_KEY, JSON.stringify([...seen].slice(-200))); } catch { /* storage blocked: this session only */ }
  if (!first) for (const k of fresh) M.celebrate.add(k); // the first answer of a visit is only a baseline
}
// After the redraw that shows them: celebrate on Home, or pulse the Home tab from anywhere else.
function runCelebrations() {
  const keys = [...M.celebrate];
  M.celebrate.clear();
  if (reduced()) return;
  if (!onHome() || V.t !== 'home') {
    anim($('#tabs a[href="#/"]'), [{ transform: 'scale(1)' }, { transform: 'scale(1.18)' }, { transform: 'scale(1)' }], { duration: 600, easing: SPRING });
    return;
  }
  keys.slice(0, 3).forEach((k, i) => setTimeout(() => celebrate(document.querySelector(`[data-arrived="${CSS.escape(k)}"]`) || document.body), 480 + i * 200));
}
const hideEl = (el, on) => { if (el) el.style.visibility = on ? 'hidden' : ''; };
// Answers kept in memory so a screen can show the last one at once while a fresh one is fetched.
// Anything the person changes makes them wrong, so bust() forgets them all.
const SC = new Map(), LC = new Map(), TC = new Map(); // search, library and title answers
let driveC = null; // the drives line of the library: { drives, at }
const SEARCH_TTL = 3 * 60e3;
function remember(m, k, v, max) { m.delete(k); m.set(k, v); if (m.size > max) m.delete(m.keys().next().value); }
// A read that started before authentication ended (authGen) paints and stores nothing; one that
// started before the last change (cacheGen) may still paint but stores nothing.
let authGen = 0, cacheGen = 0, feedGen = 0;
function bust() { cacheGen++; SC.clear(); LC.clear(); TC.clear(); driveC = null; dropFeed(); }
// The last feed, kept in this browser for the person who saw it, so opening Arr paints at once.
const FEED_KEY = 'arr.feed';
function saveFeed() {
  if (!S.me || !S.feed) return;
  try { localStorage.setItem(FEED_KEY, JSON.stringify({ u: S.me.email || '', feed: S.feed })); } catch { /* storage blocked: next open starts empty */ }
}
function dropFeed() { try { localStorage.removeItem(FEED_KEY); } catch { /* nothing kept */ } }
function savedFeed() {
  try {
    const c = JSON.parse(localStorage.getItem(FEED_KEY) || 'null');
    if (c && c.u === ((S.me && S.me.email) || '') && c.feed && typeof c.feed === 'object') return c.feed;
    if (c) dropFeed(); // someone else's, or unreadable
  } catch { dropFeed(); }
  return null;
}
let token = 0, pollT = 0, retryT = 0, feedP = null, ideasP = null, searchSeq = 0, searchAbort = null, debounceT = 0;

// ---------- drawing ----------
let rerender = () => {}, pendingFk = null;
const I = { genre: '', own: false }; // Ideas: the genre picked, and whether shows already here are listed
const later = [];
// Focus that a redraw takes away (a button swapped for another) goes to the element with the same data-fk.
function focusKey() {
  const a = document.activeElement, fk = a && a.dataset && a.dataset.fk;
  if (!fk) return null;
  const p = $('#panel');
  return { fk, panel: !!(p && p.contains(a)) };
}
function bump() {
  if (!snap && !reduced() && V.t !== 'skel') snap = measureFlip();
  if (!pendingFk) pendingFk = focusKey() || { fk: '' };
  rerender();
}
// Runs once the page has been redrawn.
function after(fn) { later.push(fn); bump(); }
function setView(v) { V = v; bump(); }
// The banner says why the page may be out of date: Arr can't be reached, or setup isn't finished.
// Whatever it says, Arr asks the feed again until the answer is good, on every screen, so it
// never outlives the problem.
const SETUP_MSG = 'Arr is still being set up. Check back soon.';
let watchT = 0;
function setBanner(t) { if (S.banner !== t) { S.banner = t; bump(); } if (t) watchBanner(); }
function setSetup(on) { if (S.setup !== on) { S.setup = on; bump(); } if (on) watchBanner(); }
function watchBanner() {
  if (watchT || S.out) return;
  watchT = setTimeout(async () => {
    watchT = 0;
    await loadFeed();
    if ((S.banner || S.setup) && !S.out) watchBanner();
  }, 5000);
}
const say = (m) => { S.live = ''; bump(); setTimeout(() => { S.live = m; bump(); }, 50); };
const hint = (text, role) => ({ t: 'hint', text, role });
const skel = (text) => ({ t: 'skel', text });

const ICONS = {
  search: 'M11 4a7 7 0 1 0 0 14a7 7 0 0 0 0-14zM20 20l-4-4',
  check: 'M5 12.5l4.5 4.5L19 7.5',
  retry: 'M20 12a8 8 0 1 1-2.5-5.8M20 4v4.5h-4.5',
  home: 'M4 11l8-7l8 7v8a1 1 0 0 1-1 1h-4v-6h-6v6H5a1 1 0 0 1-1-1z',
  ideas: 'M12 3l2.2 5.8L20 11l-5.8 2.2L12 19l-2.2-5.8L4 11l5.8-2.2z',
  library: 'M4 5h4v14H4zM10 5h4v14h-4zM16.5 5.5l3.7 1l-3.2 13l-3.7-1z',
  plus: 'M12 5v14M5 12h14',
  pause: 'M9 5v14M15 5v14',
  alert: 'M12 4a8 8 0 1 0 0 16a8 8 0 0 0 0-16zM12 8v5M12 16.5v.01',
  close: 'M6 6l12 12M18 6L6 18',
  down: 'M6 9l6 6l6-6',
  right: 'M9 6l6 6l-6 6',
  settings: 'M12 9a3 3 0 1 0 0 6a3 3 0 0 0 0-6zM12 3v3M12 18v3M3 12h3M18 12h3M5.6 5.6l2.1 2.1M16.3 16.3l2.1 2.1M5.6 18.4l2.1-2.1M16.3 7.7l2.1-2.1',
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
  if (code === 'catalog_not_imported') { setSetup(true); throw new Error(code); }
  // Only a reply that isn't Arr's own error (a proxy or tunnel failing) means Arr is out of reach.
  if (r.status >= 500 && !code) { setBanner("Can't reach Arr right now. Trying again…"); throw new Error('unreachable'); }
  setBanner('');
  return { ok: r.ok, status: r.status, data };
}

// ---------- small helpers ----------
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

// ---------- toast: a short note after Add, with Undo when that is safe ----------
let TOAST = null, toastT = 0, toastN = 0;
const held = { hover: false, focus: false };
function toastTimer() {
  clearTimeout(toastT);
  if (!TOAST) return;
  if (held.hover || held.focus) { ringAn?.pause(); return; }
  ringTo(true);
  const n = TOAST.n;
  toastT = setTimeout(() => { if (TOAST && TOAST.n === n) closeToast(); }, 6000);
}
function toastOut() {
  const el = $('#toast');
  if (!el || reduced()) return;
  const c = el.cloneNode(true);
  c.removeAttribute('id'); c.inert = true; c.setAttribute('aria-hidden', 'true'); c.style.pointerEvents = 'none';
  el.parentNode.append(c);
  settled(anim(c, [{ transform: 'none', opacity: 1 }, { transform: 'translateY(140%)', opacity: 0 }], { duration: 240, fill: 'forwards' }), () => c.remove());
  setTimeout(() => c.remove(), 800);
}
// The ring on Undo drains over the time the toast has left, and starts again whenever that time does.
let ringAn = null;
function ringTo(run) {
  ringAn?.cancel(); ringAn = null;
  const c = $('#toast .ring circle');
  if (c && run && !reduced()) ringAn = c.animate([{ strokeDashoffset: 0 }, { strokeDashoffset: 44 }], { duration: 6000, easing: 'linear', fill: 'forwards' });
}
function closeToast() { toastOut(); ringTo(false); TOAST = null; held.hover = held.focus = false; clearTimeout(toastT); bump(); }
// `t` is { before, title, after }: "Added " + the title in bold + a tail. `undo` is async and says
// in a sentence what it did, or returns null if it could not.
function showToast(t, undo) {
  const had = !!TOAST, n = ++toastN;
  TOAST = { ...t, n, undo: undo || null, busy: false };
  toastTimer(); bump();
  after(() => {
    if (!had) anim($('#toast'), [{ transform: 'translateY(140%)', opacity: 0 }, { transform: 'none', opacity: 1 }], { duration: 440, easing: SPRING });
    if (TOAST && TOAST.n === n && !held.hover && !held.focus) ringTo(true);
  });
}
function toastAdded(title, outcome, detail, undo) {
  if (outcome === 'in_plex') return showToast({ before: '', title, after: ' is already in Plex.' });
  showToast({ before: outcome === 'following' ? 'Following ' : 'Added ', title, after: detail ? ` (${detail})` : '' }, undo);
}
async function undoToast() {
  const t = TOAST;
  if (!t || !t.undo || t.busy) return;
  t.busy = true; bump();
  let m = null;
  try { m = await t.undo(); } catch { /* said below */ }
  showToast({ before: m ? (m.ok ? 'Undone. ' : '') + m.text : "Couldn't undo that just now. Try again in a moment.", title: '', after: '' });
  after(() => $('#toast')?.focus({ preventScroll: true }));
}
// It waits while the pointer is over it or focus is in it.
const inToast = (n) => !!(n && n.closest && n.closest('#toast'));
document.addEventListener('mouseover', (e) => { held.hover = inToast(e.target); toastTimer(); });
document.addEventListener('mouseout', (e) => { held.hover = inToast(e.relatedTarget); toastTimer(); });
document.addEventListener('focusin', (e) => { held.focus = inToast(e.target); toastTimer(); });
document.addEventListener('focusout', (e) => { held.focus = inToast(e.relatedTarget); toastTimer(); });
document.addEventListener('keydown', (e) => {
  if (e.key !== 'Escape' || !TOAST || !inToast(e.target)) return;
  e.stopImmediatePropagation();
  closeToast();
  const o = P.shown ? $('#panel') : $('#view');
  o && o.focus({ preventScroll: true });
});
// Undo is also on a key, so nobody has to tab to it: Z, when nothing is being typed.
document.addEventListener('keydown', (e) => {
  if (e.key.toLowerCase() !== 'z' || e.ctrlKey || e.metaKey || e.altKey || !TOAST || !TOAST.undo) return;
  const a = e.target;
  if (a && a.closest && a.closest('input,textarea,select,[contenteditable]')) return;
  e.preventDefault();
  undoToast();
});
function Toast() {
  const t = TOAST;
  return h('div', { class: 'toast-region', 'aria-live': 'polite' }, t && h('div', { id: 'toast', class: 'toast', tabindex: '-1' },
    h('span', { class: 'toast-t' }, t.before, t.title ? h('b', null, t.title) : null, t.after),
    t.undo && h('button', { type: 'button', class: 'toast-undo', 'data-s': t.busy ? 'busy' : null, 'aria-disabled': t.busy ? 'true' : 'false', 'aria-keyshortcuts': 'Z', onClick: undoToast }, t.busy ? 'Undoing…' : 'Undo',
      h('svg', { class: 'ring', viewBox: '0 0 18 18', 'aria-hidden': 'true' }, h('circle', { cx: 9, cy: 9, r: 7 })))));
}
// Undo takes the add back by removing the title from Arr (POST /remove): downloads that have not
// finished are cancelled, a request still retrying ends, and Arr stops tracking it. Files already in
// Plex are never touched. It answers { ok, text }: what happened, in a sentence.
async function removeTitle(mediaId) {
  const r = await api('/v1/remove', { method: 'POST', body: { media_id: mediaId } });
  if (r.ok) {
    forget(mediaId); bust(); S.stale = true; loadFeed();
    const n = r.data && r.data.cancelled;
    return { ok: true, text: n > 0 ? 'Arr cancelled the download and stopped tracking it.' : 'Arr stopped tracking it.' };
  }
  const code = r.data && r.data.error && r.data.error.code;
  if (r.status === 409 && code === 'active_downloads') return { ok: false, text: "Too late to undo: it's nearly downloaded." };
  return { ok: false, text: "Couldn't undo that just now. Try again in a moment." };
}

// ---------- add / retry controls ----------
function addBtn(it) {
  const st = A.get(it.id) || {}, s = st.busy ? 'busy' : st.state || it.base;
  const L = { add: ['plus', 'Add'], busy: [null, 'Adding…'], added: ['check', 'Added'], following: ['check', 'Following'], inplex: ['check', 'In Plex'] }[s];
  return h('button', {
    type: 'button', class: 'btn', 'data-fk': 'add:' + it.id, 'data-s': s, 'aria-disabled': s === 'add' ? 'false' : 'true',
    'aria-label': s === 'add' ? `Add ${it.title}` : `${L[1]}, ${it.title}`, onClick: () => addOne(it),
  }, s === 'busy' ? h('span', { class: 'spin', 'aria-hidden': 'true' }) : s === 'added' && st.pop ? tickIc() : L[0] ? ic(L[0]) : null, L[1]);
}
// The tick draws itself the moment an add succeeds; in every other place it is simply there.
const tickIc = () => h('svg', { viewBox: '0 0 24 24', class: 'ic tick', 'aria-hidden': 'true' }, h('path', { d: ICONS.check }));
const addEl = (id) => document.querySelector(`[data-fk="add:${CSS.escape(id)}"]`);
// Tap: a squish. Added: the button pops and the poster hops. Failed: a shake, next to the message.
function addFx(id, kind) {
  after(() => {
    const b = addEl(id);
    if (!b) return;
    if (kind === 'tap') anim(b, [{ transform: 'scale(1)' }, { transform: 'scale(.88)' }, { transform: 'scale(1)' }], { duration: 260 });
    else if (kind === 'ok') {
      anim(b, [{ transform: 'scale(.7)' }, { transform: 'scale(1)' }], { duration: 520, easing: SPRING });
      confetti(...middle(b));
      anim(tileOf(b), [{ transform: 'none' }, { transform: 'translateY(-7px) rotate(-4deg)' }, { transform: 'none' }], { duration: 520, easing: SPRING });
    } else anim(b, [0, -7, 7, -5, 5, -2, 0].map((x) => ({ transform: `translateX(${x}px)` })), { duration: 420, easing: 'linear' });
  });
}
const Msg = ({ st }) => h('p', { class: 'msg', 'data-err': st.err ? '' : null }, st.msg || null);
const addMsg = (it) => h(Msg, { st: A.get(it.id) || {} });
async function addOne(it) {
  const st0 = A.get(it.id) || {};
  if ((st0.busy ? 'busy' : st0.state || it.base) !== 'add') return;
  A.set(it.id, { busy: true }); bump(); addFx(it.id, 'tap');
  let next, got = null;
  try {
    const r = await api('/v1/quick-add', { method: 'POST', body: { media_id: it.id } });
    if (r.ok && r.data) {
      const o = r.data.outcome;
      got = r.data;
      next = { msg: r.data.message || 'Added.', state: o === 'in_plex' ? 'inplex' : o === 'following' ? 'following' : o === 'movies_off' ? undefined : 'added', err: o === 'movies_off', pop: o !== 'movies_off' };
    } else next = { msg: "Couldn't add this one. Try again in a moment.", err: true };
  } catch { next = { msg: "Couldn't reach Arr. Try again in a moment.", err: true }; }
  A.set(it.id, next); bump(); say(next.msg); addFx(it.id, next.err ? 'bad' : 'ok');
  bust();
  if (next.err) return;
  // Undo only for a title Arr did not track at all before this tap (the search result says so).
  const o = got.outcome;
  toastAdded(it.title, o, got.detail, it.fresh ? () => undoAdd(it, got) : null);
  loadFeed();
}
async function undoAdd(it, got) {
  const m = await removeTitle(got.media_id);
  if (m.ok) { A.delete(it.id); bump(); }
  return m;
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
  bust();
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

// A short note on the shared toast, with an optional Undo that says in a sentence what it did.
function toast(text, undo) { showToast({ before: text, title: '', after: '' }, undo); }
const owned = (x) => (x.owned ?? x.in_library) === true;
// "Not interested": the idea goes at once; the server remembers it for everyone using this Arr.
async function hideIdea(x) {
  const list = (S.ideas && S.ideas.items) || [], at = list.indexOf(x);
  if (at < 0) return;
  const post = api('/v1/ideas/hide', { method: 'POST', body: { media_id: x.media_id } }).then((r) => r.ok, () => false); // asked now; the card shrinks meanwhile
  await leave(document.querySelector(`[data-fk="hide:${CSS.escape(x.media_id)}"]`)?.closest('li'));
  S.ideas = { ...S.ideas, items: ((S.ideas && S.ideas.items) || list).filter((i) => i.media_id !== x.media_id) };
  const back = () => { S.ideas = { ...S.ideas, items: S.ideas.items.some((i) => i.media_id === x.media_id) ? S.ideas.items : [...S.ideas.items.slice(0, at), x, ...S.ideas.items.slice(at)] }; };
  toast('Hidden', async () => {
    back(); bump();
    try { await api('/v1/ideas/hide/' + encodeURIComponent(x.media_id), { method: 'DELETE' }); } catch { /* shown again for now; hidden again on the next load */ }
    return { ok: true, text: 'It is back in Ideas.' };
  });
  const ok = await post;
  if (!ok) { back(); toast("Couldn't hide that. Try again in a moment."); }
}
const notInterested = (x) => h('button', { type: 'button', class: 'link quiet', 'data-fk': 'hide:' + x.media_id, 'aria-label': `Not interested in ${x.title}`, onClick: () => hideIdea(x) }, 'Not interested');

// ---------- rows ----------
// What went wrong, in a sentence, and the one thing that helps: 'retry' asks Arr again, 'copies' opens
// the title with its list of copies to pick from. Anything the app doesn't know keeps the server's
// own words and Try again.
const WHY = {
  stalled: ['This download stopped getting data, so Arr gave up on it.'],
  removed_from_client: ['This download was removed from your downloader.'],
  submission_failed: ["Arr couldn't hand this to your downloader."],
  no_metadata: ['No one is sharing this copy any more. Arr will look for another.'],
  invalid_torrent: ['This copy was damaged, so Arr dropped it.'],
  client_error: ['Your downloader ran into an error with this one.'],
  missing_files: ['Your downloader lost the files for this one.'],
  invalid_media: ['The download had no video Arr could use.'],
  foreign_torrent: ['Your downloader already had this one, so Arr left it alone.'],
  wrong_payload: ["The download wasn't the right movie or episode."],
  wrong_quality: ["This copy wasn't the quality you chose.", 'copies'],
  wrong_language: ["This copy wasn't in a language you chose.", 'copies'],
};
function needInfo(it) {
  let [text, act] = (it.kind === 'download_failed' && WHY[it.failure]) || [];
  if (!text && it.kind === 'import_blocked') text = "The download finished, but Arr couldn't add it to Plex.";
  if (!text && it.kind === 'request_expired') text = "Arr looked for a while and couldn't find a copy.";
  return { text: text || it.problem, act: act === 'copies' && titleHref(it.media_id) ? 'copies' : 'retry' };
}
// The title's page opens with Download options and the copy list already showing.
const W = { id: '', season: null };
function wantCopies(it) {
  const m = /^(?:season|episode):[a-z]+:\d+:(\d+)/.exec(it.target || '');
  const s = /^(?:series|season|episode):((?:tvdb|tvmaze):\d+)/.exec(it.media_id || '');
  W.id = s ? 'series:' + s[1] : it.media_id; W.season = m ? Number(m[1]) : null;
}
function NeedRow({ it }) {
  const id = retryId(it), st = A.get(id) || {}, off = st.busy || st.state, { text, act } = needInfo(it);
  const btn = act === 'copies'
    ? h('a', { class: 'btn coral', href: titleHref(it.media_id), 'data-fk': id, 'aria-label': `Choose a copy yourself, ${it.title}`, onClick: () => wantCopies(it) }, ic('right'), 'Choose a copy yourself')
    : h('button', { type: 'button', class: 'btn coral', 'data-fk': id, 'aria-disabled': off ? 'true' : 'false', 'aria-label': `Try again, ${it.title}`, onClick: () => retryOne(it) },
      ic('retry'), st.busy ? 'Trying…' : st.state ? 'Trying again' : 'Try again');
  return h('li', { class: 'item need', 'data-flip': it.media_id }, h(Tile, { title: it.title, mediaId: it.media_id }),
    h('div', { class: 'what' }, h(Title, { title: it.title, id: it.media_id }),
      meta(it) && h('p', { class: 'meta' }, meta(it)),
      h('p', { class: 'problem' }, ic('alert'), h('span', null, text)), h(Msg, { st }), btn));
}
function ArrivedRow({ it }) {
  return h('li', { class: 'item arrived', 'data-flip': it.media_id, 'data-arrived': arrKey(it) }, h(Tile, { title: it.title, mediaId: it.media_id }),
    h('div', { class: 'what' }, h('span', { class: 'tag' }, ic('check'), 'In Plex'),
      h(Title, { title: it.title, id: it.media_id }), h('p', { class: 'meta' }, joinDot(it.detail, ago(it.at)))));
}
// Time left, in words a person would say. Nothing when it isn't known.
function fmtLeft(sec) {
  if (typeof sec !== 'number' || !(sec >= 0)) return '';
  if (sec < 60) return 'less than a minute left';
  const m = Math.round(sec / 60);
  return m < 60 ? `about ${m} min left` : `about ${Math.max(1, Math.round(sec / 3600))} h left`;
}
const STAGES = ['Found', 'Downloading', 'Checking', 'In Plex'];
// The bar slides from where it last was to where it is now.
function Journey({ stage, p, fkey, paused, live }) {
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
  return h('ol', { class: 'journey' + (paused ? ' paused' : ''), 'aria-label': 'Progress' }, h('i', { class: 'fill' + (live ? ' live' : ''), ref: el }),
    STAGES.map((s, i) => h('li', { key: s, class: i <= stage ? 'on' : '', 'aria-current': i === stage ? 'step' : null }, s)));
}
// A number that counts up to its new value instead of jumping.
function Count({ to }) {
  const [v, setV] = useState(to), cur = useRef(to);
  useEffect(() => {
    if (reduced() || cur.current === to) { cur.current = to; setV(to); return; }
    const a = cur.current, t0 = performance.now();
    let raf = 0;
    const f = (t) => {
      const k = Math.min(1, (t - t0) / 800), n = Math.round(a + (to - a) * (1 - (1 - k) ** 3));
      cur.current = n; setV(n);
      if (k < 1) raf = requestAnimationFrame(f);
    };
    raf = requestAnimationFrame(f);
    return () => cancelAnimationFrame(raf);
  }, [to]);
  return v;
}
function WayRow({ it }) {
  const stage = Math.max(0, ['found', 'downloading', 'checking'].indexOf(it.stage));
  const p = typeof it.progress === 'number' ? Math.min(1, Math.max(0, it.progress)) : null;
  const left = stage === 1 && p != null && !it.paused ? fmtLeft(it.eta_seconds) : '';
  const pc = p == null ? 0 : Math.round(p * 100);
  const status = it.paused ? 'Paused' : stage === 0 ? 'Found, getting ready' : stage === 1 ? (p == null ? 'Downloading' : [h(Count, { key: 'n', to: pc }), left ? `% · ${left}` : '% downloaded']) : 'Checking the file';
  return h('li', { class: 'item', 'data-flip': it.media_id }, h(Tile, { title: it.title, mediaId: it.media_id }),
    h('div', { class: 'what' }, h(Title, { title: it.title, id: it.media_id }),
      h('p', { class: 'meta status' }, it.paused && ic('pause'), meta(it), meta(it) ? ' · ' : null, status),
      h(Journey, { stage, p, fkey: it.grab_id, paused: it.paused, live: stage === 1 && !it.paused })));
}
function LookRow({ it }) {
  const until = it.until ? parseDate(it.until) : null;
  return h('li', { class: 'item', 'data-flip': it.media_id }, until ? chip(until, 'month') : h(Tile, { title: it.title, mediaId: it.media_id }),
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
// IMDb and Rotten Tomatoes scores from Plex, and a quick verdict. Only the scores that exist show.
const VERDICTS = {
  likely_good: ['Likely good', 'both high'],
  probably_skip: ['Probably skip', 'both low'],
  critics_pick: ['Critics\u2019 pick', 'critics liked it more'],
  crowd_pleaser: ['Crowd-pleaser', 'audiences liked it more'],
  mixed: ['Mixed reviews', 'scores are in the middle'],
};
function scoreLine(sc) {
  if (!sc || typeof sc !== 'object') return null;
  const num = (v, max) => typeof v === 'number' && v >= 0 && v <= max;
  const parts = [];
  if (num(sc.imdb, 10)) parts.push(['IMDb', sc.imdb.toFixed(1)]);
  if (num(sc.critics, 100)) parts.push(['Critics', Math.round(sc.critics) + '%']);
  if (num(sc.audience, 100)) parts.push(['Audience', Math.round(sc.audience) + '%']);
  if (!parts.length) return null;
  const v = VERDICTS[sc.verdict] && num(sc.critics, 100) && num(sc.audience, 100) ? VERDICTS[sc.verdict] : null;
  const why = v && `Critics ${Math.round(sc.critics)}%, audience ${Math.round(sc.audience)}%: ${v[1]}`;
  const row = [];
  parts.forEach(([k, n], i) => {
    row.push(h('span', { key: k, class: 'sc' }, k + ' ', h('b', null, n), i < parts.length - 1 && h('span', { class: 'dot', 'aria-hidden': 'true' }, ' ·'), ' '));
  });
  return h('div', { class: 'scores' }, h('p', { class: 'sc-row' }, row),
    v && h('span', { class: 'verdict ' + sc.verdict, title: why }, v[0], h('span', { class: 'sr' }, '. ' + why)));
}
// TVmaze viewers' average out of 10: a star and the number, read out in words.
function rating(x) {
  const r = Number(x.rating);
  if (!(r > 0 && r <= 10)) return null;
  const n = r.toFixed(1);
  return h('span', { class: 'rating', title: `Rated ${n} out of 10 by TVmaze viewers` },
    ic('star'), h('span', { 'aria-hidden': 'true' }, n), h('span', { class: 'sr' }, `Rated ${n} out of 10`));
}
// Plex scores replace the TVmaze star when there are any.
function ideaMeta(x, ...facts) {
  const sc = scoreLine(x.scores), r = sc ? null : rating(x), text = joinDot(...facts);
  return [h('p', { class: 'meta' }, r, r && text ? ' · ' : null, text), sc];
}
function IdeaCard({ x, plain }) {
  return h('li', { class: 'idea', 'data-flip': x.media_id }, h(Tile, { title: x.title, mediaId: x.media_id }), h(Title, { title: x.title, id: x.media_id }),
    ideaMeta(x, x.network || x.year || ''), chooseLink(x), plain ? null : notInterested(x));
}
function IdeaRow({ x }) {
  return h('li', { class: 'item top-align', 'data-flip': x.media_id }, h(Tile, { title: x.title, mediaId: x.media_id }),
    h('div', { class: 'what' }, h(Title, { title: x.title, id: x.media_id }),
      ideaMeta(x, x.year, x.network), x.summary && h('p', { class: 'clamp' }, x.summary), notInterested(x)), chooseLink(x));
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
  const ideas = ((S.ideas && S.ideas.items) || []).filter((x) => !owned(x)).slice(0, 6);
  if (!secs.length) {
    secs.push(h('section', { key: 'empty', class: 'empty', 'aria-labelledby': 'h-empty' }, h('img', { src: '/app/mascot.svg', alt: '' }),
      h('h2', { id: 'h-empty' }, 'Find something to watch'), h('p', null, ideas.length ? 'Search above for a movie or show, or start with one of these.' : 'Search above for a movie or show.')));
  }
  if (ideas.length) secs.push(h(Section, { key: 'ideas', id: 'ideas', title: 'Ideas for you', extra: h('a', { href: '#/ideas' }, 'See all') }, h('ul', { class: 'ideas-row' }, keyed(ideas, (x) => x.media_id, (x, key) => h(IdeaCard, { key, x })))));
  return h('div', { class: 'stack' }, S.cached && h('p', { class: 'hint updating', role: 'status' }, 'Updating…'), secs);
}
function paintHome() { if (S.feed) setView({ t: 'home' }); }
const onHome = () => parseHash().path === '/';
// One feed read at a time: asking while one is on its way waits for that answer.
function loadFeed() {
  if (S.out) return Promise.resolve(false);
  if (feedP && feedGen !== cacheGen) return feedP.then(loadFeed); // that read began before a change
  return (feedP ||= (async () => {
    const ag = authGen, cg = (feedGen = cacheGen);
    try {
      const r = await api('/v1/feed');
      if (ag !== authGen) return false;
      if (r.ok) { noteArrivals(r.data); S.feed = r.data; S.cached = false; S.tmdb = !!r.data.tmdb_backup; if (cg === cacheGen) saveFeed(); setSetup(false); if (onHome()) paintHome(); bump(); }
      return r.ok;
    } catch { return false; } finally {
      feedP = null;
      if (S.cached) { S.cached = false; bump(); }
      clearTimeout(pollT);
      if (!document.hidden && onHome() && !S.out) pollT = setTimeout(loadFeed, 5000);
    }
  })());
}
function loadIdeas() {
  if (S.ideas && Date.now() - S.ideasAt < 6e5) return Promise.resolve(S.ideas);
  return (ideasP ||= (async () => {
    const ag = authGen;
    try { const r = await api('/v1/ideas'); if (r.ok && ag === authGen) { S.ideas = r.data; S.ideasAt = Date.now(); } } catch { /* optional */ } finally { ideasP = null; }
    return S.ideas;
  })());
}
function failed(t) {
  setView(hint('Your list will show up here as soon as Arr is back.'));
  clearTimeout(retryT);
  retryT = setTimeout(() => { if (t === token) route(); }, 5000);
}
async function renderHome(t) {
  if (S.feed) paintHome(); else setView(skel());
  const had = S.ideas, feed = loadFeed(), ideas = loadIdeas(); // asked together
  const ok = await feed;
  if (t !== token) return;
  if (!ok && !S.feed) return failed(t);
  await ideas;
  if (t === token && S.ideas !== had) paintHome();
}

// ---------- loading: placeholders shaped like the rows, shimmering ----------
function SkelRows({ n = 4 }) {
  return h('ul', { class: 'list', 'aria-hidden': 'true' }, Array.from({ length: n }, (_, i) => h('li', { key: i, class: 'item' },
    h('div', { class: 'tile sk' }), h('div', { class: 'what' }, h('div', { class: 'sk ln', style: { width: i % 2 ? '48%' : '64%' } }), h('div', { class: 'sk ln s', style: { width: '34%' } })))));
}
// The words stay for screen readers; sighted people see the shapes.
function SkelView({ text }) {
  return h('div', { class: 'stack', 'aria-busy': 'true' }, h('p', { class: 'sr', role: 'status' }, text || 'Loading…'), h(SkelRows));
}
// A thin line across the top while the page on screen is the last answer and a new one is coming.
function Topline({ on }) {
  const el = useRef(null), run = useRef(null);
  useEffect(() => {
    const e = el.current;
    if (!e) return;
    if (on) {
      run.current?.cancel();
      run.current = anim(e, [{ transform: 'scaleX(0)', opacity: 1 }, { transform: 'scaleX(.85)', opacity: 1 }], { duration: 1400, easing: 'cubic-bezier(.1,.7,.2,1)', fill: 'forwards' });
    } else if (run.current) {
      const before = run.current, out = anim(e, [{ transform: 'scaleX(1)', opacity: 1 }, { transform: 'scaleX(1)', opacity: 0 }], { duration: 320, fill: 'forwards' });
      run.current = null; before.cancel();
      settled(out, () => { if (!run.current) out.cancel(); });
    }
  }, [on]);
  return h('div', { class: 'topline', ref: el, 'aria-hidden': 'true' });
}

// ---------- ideas ----------
function IdeasPage() {
  const d = S.ideas;
  if (!d) return h(SkelView);
  const all = d.items, have = all.filter(owned).length;
  const base = I.own ? all : all.filter((x) => !owned(x));
  // The six genres most of the ideas share; one at a time, or all.
  const count = new Map();
  for (const x of base) for (const g of x.genres || []) count.set(g, (count.get(g) || 0) + 1);
  const top = [...count].sort((p, q) => q[1] - p[1]).slice(0, 6).map(([g]) => g);
  const pick = top.includes(I.genre) ? I.genre : '';
  const items = pick ? base.filter((x) => (x.genres || []).includes(pick)) : base;
  const toggle = have > 0 && h('button', { type: 'button', class: 'link', 'aria-pressed': String(I.own), 'data-fk': 'own', onClick: () => { I.own = !I.own; bump(); } }, I.own ? ic('check') : null, 'Show ones I have');
  const note = d.status === 'disabled' ? "Ideas aren't switched on for this Arr." : d.status === 'unavailable' && !all.length ? "Ideas aren't available right now. Check back soon."
    : !all.length ? 'No ideas yet. Check back soon.' : !items.length ? (have && !I.own ? 'You already have everything on this list.' : 'No ideas yet. Check back soon.') : null;
  const chips = top.length > 1 && h('div', { class: 'chips', role: 'group', 'aria-label': 'Genre' },
    [['', 'All'], ...top.map((g) => [g, g])].map(([g, l]) => h('button', { key: g || 'all', type: 'button', class: 'chip', 'aria-pressed': String(pick === g), 'data-fk': 'g:' + g, onClick: () => { I.genre = g; bump(); } }, l)));
  return h(Section, { id: 'ideas', title: 'Popular this week', extra: toggle }, chips,
    note ? h(Hint, { text: note }) : h(List, null, keyed(items, (x) => x.media_id, (x, key) => h(IdeaRow, { key, x }))));
}
async function renderIdeas(t) {
  setView(skel());
  const d = await loadIdeas();
  if (t !== token) return;
  if (!d) return failed(t);
  setView({ t: 'ideas' });
}

// ---------- search ----------
async function renderSearch(q, t) {
  q = q.trim();
  if (!q) {
    // Before typing: what was searched lately, and what is popular.
    searchSeq++;
    setView({ t: 'start' });
    const had = S.ideas;
    loadIdeas().then(() => { if (t === token && S.ideas !== had) bump(); });
    return;
  }
  if (q.length < 2) return setView(hint('Type at least two letters to search.'));
  const seq = ++searchSeq;
  searchAbort?.abort();
  const ac = (searchAbort = new AbortController());
  if (V.t === 'search') { V.busy = true; bump(); } else setView(skel('Searching…'));
  const key = q.toLowerCase(), hit = SC.get(key), ag = authGen, cg = cacheGen;
  let view = null, pend = null;
  const attach = () => { if (pend && view && V === view && seq === searchSeq) { view.people = pend; bump(); } };
  // Searched a moment ago: shown at once, with the people it found.
  if (hit && Date.now() - hit.at < SEARCH_TTL) {
    view = { t: 'search', q, rows: hit.rows, notes: hit.notes, busy: false, loading: false, people: hit.people || null };
    M.fresh = true; setView(view);
    if (!hit.people) findPeople(q, seq, ac.signal).then((p) => { if (p) hit.people = p; pend = p; attach(); });
    return;
  }
  findPeople(q, seq, ac.signal).then((p) => { pend = p; attach(); }); // never waited for
  const ok = (r) => r && r.ok;
  const part = (kind) => api(`/v1/discover?kind=${kind}&q=${encodeURIComponent(q)}`, { signal: ac.signal }).then((r) => ({ ok: r.ok, data: r.data }), () => ({ ok: false }));
  const done = {}, shown = {};
  const pm = part('movie').then((r) => (done.m = r)), ps = part('series').then((r) => (done.s = r));
  const both = () => !!(done.m && done.s);
  const rowsOf = (r, label) => (r.data.results || []).map((x) => ({ x, label }));
  let rows = null;
  // Each half is drawn as it arrives and the rows already there stay put: a later half goes below.
  const paint = () => {
    if (seq !== searchSeq || t !== token || ag !== authGen) return;
    if (!ok(done.m) && !ok(done.s)) { if (both()) setView(hint('Search will work again as soon as Arr is back.', 'status')); return; }
    const m = done.m && !shown.m && ok(done.m) ? rowsOf(done.m, 'Movie') : [], sh = done.s && !shown.s && ok(done.s) ? rowsOf(done.s, 'Show') : [];
    shown.m = !!done.m; shown.s = !!done.s;
    if (!rows) { rows = []; for (let i = 0; i < Math.max(m.length, sh.length); i++) { if (m[i]) rows.push(m[i]); if (sh[i]) rows.push(sh[i]); } } else rows = rows.concat(m, sh);
    const moviesOff = (S.feed && S.feed.movies_enabled === false) || (ok(done.m) && (done.m.data.providers || []).some((p) => p.name === 'plex' && p.status === 'not_configured'));
    const notes = [];
    if (done.m && moviesOff) notes.push("Movie search isn't set up yet.");
    else if (done.m && !ok(done.m)) notes.push("Movie results aren't available right now.");
    if (done.s && !ok(done.s)) notes.push("Show results aren't available right now.");
    if (view) { Object.assign(view, { rows, notes, loading: !both() }); bump(); } else { view = { t: 'search', q, rows, notes, busy: false, loading: !both(), people: null }; M.fresh = true; setView(view); }
    attach();
    if (both() && ok(done.m) && ok(done.s) && cg === cacheGen) remember(SC, key, { at: Date.now(), rows, notes, people: pend }, 20);
  };
  await Promise.race([pm, ps]);
  if (!both()) await Promise.race([Promise.all([pm, ps]), new Promise((r) => setTimeout(r, 120))]); // near-simultaneous halves are mixed together
  paint();
  if (!both()) { await Promise.all([pm, ps]); paint(); }
}
// People matching the search: the person (with their work) when it is clearly who was meant,
// else a few names. Anything going wrong is no people block, never a failed search.
const fold = (t) => String(t).normalize('NFD').replace(/[\u0300-\u036f]/g, '').toLowerCase().replace(/['’]/g, '').replace(/[^\p{L}\p{N}]+/gu, ' ').trim();
async function findPeople(q, seq, signal) {
  try {
    const r = await api('/v1/people?q=' + encodeURIComponent(q), { signal });
    const list = r.ok ? r.data.people || [] : [];
    if (!list.length || seq !== searchSeq) return null;
    const top = list[0];
    if (top.score >= 0.8 || (fold(q) && fold(top.name) === fold(q))) {
      const p = await api('/v1/people/' + encodeURIComponent(top.person_id), { signal });
      if (p.ok && (p.data.groups || []).length) return { person: p.data };
    }
    return { names: list.slice(0, 3) };
  } catch { return null; }
}
const creditRows = (groups) => keyed(groups.flatMap((g) => g.credits.map((c) => ({ c, job: g.job }))), (r) => r.c.media_id,
  (r, key) => h(SearchRow, { key, x: r.c, label: r.c.kind === 'movie' ? 'Movie' : 'Show' }));
const PERSON_ROWS = 12;
function PersonBlock({ person }) {
  const all = person.groups.flatMap((g) => g.credits), more = all.length > PERSON_ROWS;
  const shown = [{ job: '', credits: all.slice(0, PERSON_ROWS) }];
  return h(Section, { id: 'person', title: person.name, extra: more ? h('a', { href: '#/person/' + encodeURIComponent(person.person_id) }, 'Show all ' + all.length) : null },
    person.known_for && h('p', { class: 'meta' }, person.known_for), h(List, null, creditRows(shown)));
}
function PeopleRow({ names }) {
  return h(Section, { id: 'people', title: 'People' },
    h('div', { class: 'chips' }, names.map((n) => h('a', { key: n.person_id, class: 'chip', href: '#/person/' + encodeURIComponent(n.person_id) }, n.name))));
}
async function renderPerson(id, t) {
  setView(skel());
  try {
    const r = await api('/v1/people/' + encodeURIComponent(id));
    if (t !== token) return;
    if (r.status === 404) return setView(hint('Arr could not find that person.', 'status'));
    if (!r.ok) return failed(t);
    document.title = r.data.name + ' · Arr';
    setView({ t: 'person', person: r.data });
  } catch { if (t === token) failed(t); }
}
function PersonPage() {
  const p = V.person;
  return h('div', { class: 'stack' }, h('h1', { class: 'person-name' }, p.name), p.known_for && h('p', { class: 'meta' }, p.known_for),
    p.groups.length ? p.groups.map((g, i) => h(Section, { key: g.job, id: 'job' + i, title: g.job }, h(List, null, creditRows([g]))))
      : h(Hint, { text: 'Nothing Arr can get for this person yet.' }));
}
// What a person searched lately, on this device only.
function recents() {
  try { const a = JSON.parse(localStorage.getItem('arr.recent') || '[]'); return Array.isArray(a) ? a.filter((q) => typeof q === 'string' && q.trim()).slice(0, 8) : []; } catch { return []; }
}
function saveRecents(a) { try { localStorage.setItem('arr.recent', JSON.stringify(a.slice(0, 8))); } catch { /* private mode: nothing is kept */ } }
function noteSearch(q) {
  q = (q || '').trim();
  if (q.length >= 2) saveRecents([q, ...recents().filter((r) => r.toLowerCase() !== q.toLowerCase())]);
}
function dropRecent(q) {
  saveRecents(recents().filter((r) => r !== q));
  after(() => ($('.recent-x') || $('#q'))?.focus({ preventScroll: true }));
}
function SearchStart() {
  const rec = recents(), pop = ((S.ideas && S.ideas.items) || []).filter((x) => !owned(x)).slice(0, 6);
  return h('div', { class: 'stack' },
    rec.length ? h(Section, { id: 'recent', title: 'Recent', extra: h('button', { type: 'button', class: 'link', onClick: () => { saveRecents([]); after(() => $('#q')?.focus({ preventScroll: true })); } }, 'Clear') },
      h('ul', { class: 'recent' }, rec.map((q) => h('li', { key: q },
        h('a', { class: 'recent-q', href: '#/search?q=' + encodeURIComponent(q) }, ic('search'), h('span', null, q)),
        h('button', { type: 'button', class: 'recent-x', 'aria-label': `Remove ${q} from recent searches`, onClick: () => dropRecent(q) }, ic('close')))))) : null,
    pop.length ? h(Section, { id: 'pop', title: 'Popular now', extra: h('a', { href: '#/ideas' }, 'See all') }, h('ul', { class: 'ideas-row' }, keyed(pop, (x) => x.media_id, (x, key) => h(IdeaCard, { key, x, plain: true })))) : null,
    !rec.length && !pop.length ? h(Hint, { text: 'Type at least two letters to search.' }) : null);
}
// What Arr already has, as a small badge.
const HAVE = { in_plex: ['check', 'In Plex'], on_its_way: ['check', 'On its way'], tracking: ['check', 'Tracking'] };
const haveBadge = (x, label) => HAVE[x.library_state] && !(label === 'Movie' && x.library_state === 'in_plex') && h('span', { class: 'tag' }, ic(HAVE[x.library_state][0]), HAVE[x.library_state][1]);
function SearchRow({ x, label }) {
  // A movie is one tap; a show opens its page to pick what to get.
  if (label === 'Show') {
    return h('li', { class: 'item top-align' }, h(Tile, { title: x.title, mediaId: x.media_id }),
      h('div', { class: 'what' }, h(Title, { title: x.title, id: x.media_id }), h('p', { class: 'meta' }, joinDot(x.year, label, x.role, x.upcoming && 'Not out yet')), scoreLine(x.scores), haveBadge(x, label)),
      chooseLink({ media_id: x.media_id, title: x.title, in_library: (x.in_catalog && x.monitored) || !!x.library_state }));
  }
  // fresh: the result says Arr did not have it at all, so the add can be undone.
  const it = { id: x.media_id, title: x.title, base: x.has_file || x.library_state === 'in_plex' ? 'inplex' : x.in_catalog && x.monitored ? 'added' : 'add', fresh: x.in_catalog === false && !x.has_file && !x.library_state };
  return h('li', { class: 'item top-align' }, h(Tile, { title: x.title, mediaId: x.media_id }),
    h('div', { class: 'what' }, h(Title, { title: x.title, id: x.media_id }), h('p', { class: 'meta' }, joinDot(x.year, label, x.role, x.upcoming && 'Not out yet')), scoreLine(x.scores), haveBadge(x, label), addMsg(it)), addBtn(it));
}
function SearchResults() {
  const { q, rows, notes, busy, loading, people } = V;
  // Opening or adding a result counts the search as one worth keeping.
  return h('div', { 'data-results': '', class: busy ? 'busy' : null, 'aria-busy': loading ? 'true' : null, onClick: (e) => { if (e.target.closest('a,button')) noteSearch(q); } },
    people && people.person ? h(PersonBlock, { person: people.person }) : null,
    h(Section, { id: 'res', title: 'Results' }, rows.length ? h(List, null, keyed(rows, (r) => r.label + r.x.media_id, (r, key) => h(SearchRow, { key, x: r.x, label: r.label })))
      : h(Hint, { text: `Nothing found for “${q}”. Check the spelling or try fewer words.` })),
    people && people.names ? h(PeopleRow, { names: people.names }) : null,
    loading ? h(Hint, { key: 'more', text: 'Still looking…', role: 'status' }) : null,
    notes.map((t) => h(Hint, { key: t, text: t })));
}

// ---------- library ----------
// Drives are sold in decimal units, so the space is shown that way ("4 TB").
function fmtDisk(bytes) {
  const tb = bytes / 1e12;
  if (tb >= 1) return `${Number(tb.toFixed(1))} TB`;
  return `${Math.max(0, Math.round(bytes / 1e9))} GB`;
}
// How full the drives holding the library are. Quiet if Arr can't tell.
function Storage() {
  const [drives, setDrives] = useState(driveC ? driveC.drives : []);
  useEffect(() => {
    let live = true;
    if (driveC && Date.now() - driveC.at < 60e3) return; // read a moment ago
    const ag = authGen, cg = cacheGen;
    api('/v1/storage').then((r) => {
      if (!r.ok || ag !== authGen) return;
      const ds = (r.data.drives || []).filter((d) => d.total_bytes > 0);
      if (cg === cacheGen) driveC = { drives: ds, at: Date.now() };
      if (live) setDrives(ds);
    }).catch(() => {});
    return () => { live = false; };
  }, []);
  if (!drives.length) return null;
  return h('div', { class: 'drives' }, drives.map((d) => {
    const low = d.free_bytes < d.total_bytes * 0.1, used = Math.round((1 - d.free_bytes / d.total_bytes) * 100);
    return h('div', { key: d.label, class: 'drive' + (low ? ' low' : '') },
      h('p', { class: 'drive-t' }, `${d.label} drive: ${fmtDisk(d.free_bytes)} free of ${fmtDisk(d.total_bytes)}`, low && h('b', null, ' · Running low on space')),
      h('div', { class: 'meter', role: 'meter', 'aria-label': `${d.label} drive, ${used}% full`, 'aria-valuemin': 0, 'aria-valuemax': 100, 'aria-valuenow': used }, h('i', { style: { width: used + '%' } })));
  }));
}
function libraryRow(x, mv) {
  const title = x.title || 'Untitled';
  const mid = mv ? (x.tmdb_id && `movie:tmdb:${x.tmdb_id}`) : x.tvdb_id ? `series:tvdb:${x.tvdb_id}` : x.tvmaze_id ? `series:tvmaze:${x.tvmaze_id}` : null;
  return { key: mid || title, el: (key) => h('li', { key, class: 'item' }, h(Tile, { title, mediaId: mid }), h('div', { class: 'what' }, h(Title, { title, id: mid }),
    h('p', { class: 'meta' }, joinDot(x.year, mv ? 'Movie' : 'Show', mv && x.has_file === false && 'Not in Plex yet')),
    x.requested_by && h('p', { class: 'meta' }, `Added by ${x.requested_by}`))) };
}
function Library({ t }) {
  const [kind, setKind] = useState(S.kind);
  const [fv, setFv] = useState(S.filter);
  const [sort, setSort] = useState(S.sort);
  const [st, setSt] = useState({ items: [], total: 0, status: 'Loading…' });
  const m = useRef({ seq: 0, items: [], ft: 0 }).current;
  async function load(reset) {
    const ag = authGen, cg = cacheGen, my = ++m.seq, key = `${S.kind}|${S.sort}|${S.filter.trim()}`;
    if (reset) {
      // The last answer for this list shows while the new one is fetched.
      const hit = LC.get(key);
      m.items = []; m.enter = true;
      setSt(hit ? { items: hit.items, total: hit.total, status: 'Updating…' } : { items: [], total: 0, status: 'Loading…' });
    } else if (m.gen !== cacheGen) {
      // The rows on screen were read before something changed: "Show more" starts over from page one.
      m.items = [];
      setSt((s) => ({ ...s, status: 'Updating…' }));
    }
    const qs = `limit=50&offset=${m.items.length}&sort=${S.sort}` + (S.filter.trim() ? '&q=' + encodeURIComponent(S.filter.trim()) : '');
    try {
      const r = await api(`/v1/library/${S.kind}?${qs}`);
      if (my !== m.seq || t !== token || ag !== authGen) return;
      if (!r.ok) { setSt((s) => ({ ...s, status: "Couldn't load the library just now." })); return; }
      m.items = m.items.concat(r.data.items || []);
      m.gen = cg;
      if (cg === cacheGen) remember(LC, key, { items: m.items, total: r.data.total || 0 }, 12);
      setSt({ items: m.items, total: r.data.total || 0, status: m.items.length ? '' : S.filter.trim() ? `No matches for “${S.filter.trim()}”.` : 'Nothing here yet. Add something from the search above.' });
    } catch { if (my === m.seq && t === token && !S.out) { setSt((s) => ({ ...s, status: '' })); if (!m.items.length) failed(t); } }
  }
  useEffect(() => { load(true); return () => clearTimeout(m.ft); }, []);
  // Rows come in one after another when a list is first drawn (and when its kind, sort or filter changes).
  useLayoutEffect(() => { if (m.enter && st.items.length) { m.enter = false; stagger([...document.querySelectorAll('#view .list > li')]); } }, [st.items]);
  // The top line runs while the list on screen is the last answer.
  useEffect(() => {
    const on = st.status === 'Updating…';
    if (S.libUpd !== on) { S.libUpd = on; bump(); }
  }, [st.status]);
  useEffect(() => () => { if (S.libUpd) { S.libUpd = false; bump(); } }, []);
  const mv = kind === 'movies';
  return h(Section, { id: 'lib', title: 'Library' }, h('div', null,
    h(Storage),
    h('div', { class: 'bar' },
      h('div', { class: 'seg', role: 'group', 'aria-label': 'What to show' },
        [['movies', 'Movies'], ['series', 'Shows']].map(([k, l]) =>
          h('button', { key: k, type: 'button', 'aria-pressed': String(kind === k), onClick: () => { S.kind = k; setKind(k); load(true); } }, l))),
      h('input', {
        type: 'search', class: 'filter', placeholder: 'Filter your library', 'aria-label': 'Filter your library', autocomplete: 'off', value: fv,
        onInput: (e) => { const v = e.target.value; setFv(v); clearTimeout(m.ft); m.ft = setTimeout(() => { S.filter = v; load(true); }, 250); },
      }),
      h('label', { class: 'sort' }, h('span', { class: 'sr' }, 'Sort by'),
        h('select', { value: sort, onChange: (e) => { S.sort = e.target.value; saveSort(S.sort); setSort(S.sort); load(true); } },
          SORTS.map(([k, l]) => h('option', { key: k, value: k }, l))))),
    h('p', { class: st.status === 'Loading…' ? 'sr' : 'hint', role: 'status' }, st.status || null),
    st.status === 'Loading…' && !st.items.length ? h(SkelRows) : null,
    h('ul', { class: 'list', 'aria-busy': st.status === 'Updating…' ? 'true' : null }, keyed(st.items.map((x) => libraryRow(x, mv)), (r) => r.key, (r, key) => r.el(key))),
    h('div', { class: 'more' }, st.items.length < st.total ? [
      h('button', { key: 'b', type: 'button', class: 'btn ghost', onClick: () => load(false) }, 'Show more'),
      h('span', { key: 's' }, `Showing ${st.items.length} of ${st.total}`)] : null)));
}

// ---------- title pages (a show or a movie, as a sheet over the current screen) ----------
const P = { del: null, confirm: false, removing: false, shown: false, id: '', kind: '', token: 0, seq: 0, data: null, err: '', seasons: new Map(), open: new Set(), ask: '', busy: new Set(), msg: new Map(), more: false, followBusy: false, pollT: 0, askT: 0, opener: null, scrollY: 0, canBack: false, opt: null, peek: null, upd: false };
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
  const from = first ? tileOf(P.opener) : null;
  if (first) {
    P.canBack = !!bgHash; // came from a screen of the app: closing goes back to it
    if (!bgHash) renderPath('/', '', '#/'); // a link straight to a title: home sits underneath
    P.scrollY = window.scrollY;
    P.shown = true; // the page behind goes inert with the same redraw
  }
  clearTimeout(P.pollT); clearTimeout(P.askT);
  // Until fresh data arrives: what was read last time, else the title, year and poster of the row that was tapped.
  const hit = TC.get(t.id), o = P.opener, tr = o && o.getAttribute && titleRoute(parseHash(o.getAttribute('href') || '').path);
  const row = tr && tr.id === t.id && o.isConnected ? o.closest('li') : null;
  const peek = row && o.textContent ? { title: o.textContent, year: ((row.querySelector('.meta') || {}).textContent || '').match(/\b(?:19|20)\d\d\b/)?.[0] || '' } : null;
  if (hit) document.title = `${hit.title} · Arr`;
  Object.assign(P, { id: t.id, kind: t.kind, token: P.token + 1, data: hit || null, peek, upd: !!hit, err: '', seasons: new Map(), open: new Set(), ask: '', busy: new Set(), msg: new Map(), more: false, followBusy: false, confirm: false, del: null, removing: false, opt: freshOpt() });
  if (W.id === t.id) Object.assign(P.opt, { open: true, copiesOpen: true, season: W.season, fetch: true }); // from Needs you
  W.id = '';
  after(() => {
    const panel = $('#panel');
    panel.scrollTop = 0;
    if (first) { panel.querySelector('.close')?.focus({ preventScroll: true }); sheetIn(from); }
  });
  loadTitle(false);
}
function closeSheet() {
  if (!P.shown) return;
  sheetOut();
  clearTimeout(P.pollT); clearTimeout(P.askT);
  P.token++;
  P.shown = false; P.confirm = false; P.del = null;
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
  if (P.del) { if (!P.del.busy) closeDelete(); } else if (P.confirm) { if (!P.removing) closeConfirm(); } else if (P.ask) { P.ask = ''; bump(); } else dismiss();
});
// Tab and Shift-Tab stay inside the sheet.
document.addEventListener('keydown', (e) => {
  if (e.key !== 'Tab' || !P.shown) return;
  const panel = $('#confirm') || $('#panel'); // an open question holds the focus until it is answered
  // The toast, when there is one, is part of the loop. It comes before the sheet in the page, so it
  // is first here too and the loop runs in the order the browser would tab (not while a question is open).
  const f = [...(panel.id === 'confirm' ? [] : document.querySelectorAll('#toast button')), ...panel.querySelectorAll('a[href],button:not([disabled]),input,select,textarea,[tabindex]:not([tabindex="-1"])')].filter((x) => !x.hidden && x.getClientRects().length);
  const a = document.activeElement;
  if (!f.length) { e.preventDefault(); panel.focus(); return; }
  const i = f.indexOf(a), last = f.length - 1;
  // The browser steps inside the panel (radio groups and all); the edges and the toast are ours.
  if (i < 0) { e.preventDefault(); f[0].focus(); }
  else if (e.shiftKey && i === 0) { e.preventDefault(); f[last].focus(); }
  else if (!e.shiftKey && i === last) { e.preventDefault(); f[0].focus(); }
  else if (inToast(a)) { e.preventDefault(); f[i + (e.shiftKey ? -1 : 1)].focus(); }
});

function errText(r) {
  if (P.kind === 'movie') return r && r.status === 404 ? (S.feed && S.feed.movies_enabled === false ? "Movie details need movie search, which isn't set up yet." : "Arr doesn't have details for this movie right now.") : "Couldn't load this movie just now.";
  return r && r.status === 404 ? "Arr doesn't have an episode list for this show right now." : "Couldn't load this show just now.";
}
// Reads are numbered: a newer read, or the start of an action, makes an older answer stale.
async function loadTitle(quiet) {
  const tk = P.token, sq = ++P.seq, ag = authGen, cg = cacheGen, stale = () => tk !== P.token || sq !== P.seq || ag !== authGen;
  // Expanded seasons reload on their own, so a failed title read can't leave one loading forever.
  const seasons = P.kind === 'show' ? Promise.all([...P.open].map((n) => loadSeason(n, tk, sq))) : null;
  try {
    const r = await api(`/v1/${P.kind}/${enc(P.id)}`);
    if (stale()) return;
    P.upd = false;
    if (!r.ok) { TC.delete(P.id); if (!P.data || r.status === 404) { P.data = null; P.err = errText(r); } await seasons; if (!stale()) bump(); return; }
    P.data = r.data; P.err = '';
    if (cg === cacheGen) remember(TC, P.id, r.data, 30);
    document.title = `${r.data.title} · Arr`;
    if (P.opt.fetch) { P.opt.fetch = false; loadCopies(); }
  } catch {
    if (stale()) return;
    P.upd = false;
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
  return h('div', { id: 'sheet', class: 'sheet', hidden: !open, onClick: (e) => { if (e.target === e.currentTarget || e.target.classList.contains('scrim')) dismiss(); } },
    h('div', { class: 'scrim', 'aria-hidden': 'true' }),
    h('div', { class: 'panel', id: 'panel', role: 'dialog', 'aria-modal': 'true', 'aria-labelledby': 'sheet-title', 'aria-busy': open && (P.upd || !(P.data || P.err)) ? 'true' : null, tabindex: '-1', inert: open && (P.confirm || !!P.del) },
      open ? [
        h('div', { key: 'bar', class: 'sheet-bar' }, h(Grab), P.upd ? h('span', { class: 'updating', role: 'status' }, 'Updating…') : null, h('button', { type: 'button', class: 'btn ghost close', 'aria-label': 'Close', 'data-fk': 'close', onClick: dismiss }, ic('close'))),
        h('div', { key: 'body', class: 'sheet-body' }, body)] : null),
    open && P.confirm && P.data ? h(ConfirmRemove) : open && P.del && P.data ? h(ConfirmDelete) : null);
}
// The handle at the top: pull the sheet down to close it. Only the handle moves it, so the page inside still scrolls.
const pull = { on: false, y: 0, dy: 0 };
function Grab() {
  const end = () => {
    if (!pull.on) return;
    pull.on = false;
    const dy = pull.dy, panel = $('#panel'), scrim = $('#sheet .scrim');
    if (dy > 80) { M.drag = dy; dismiss(); return; }
    if (panel) panel.style.transform = '';
    if (scrim) scrim.style.opacity = '';
    if (dy && panel) anim(panel, [{ transform: `translateY(${dy}px)` }, { transform: 'none' }], { duration: 420, easing: SOFT });
  };
  return h('div', {
    class: 'grab', 'aria-hidden': 'true',
    onPointerDown: (e) => { pull.on = true; pull.y = e.clientY; pull.dy = 0; try { e.currentTarget.setPointerCapture(e.pointerId); } catch { /* not a live pointer */ } },
    onPointerMove: (e) => {
      if (!pull.on) return;
      pull.dy = Math.max(0, e.clientY - pull.y);
      const panel = $('#panel'), scrim = $('#sheet .scrim');
      if (panel) panel.style.transform = `translateY(${pull.dy}px)`;
      if (scrim) scrim.style.opacity = String(Math.max(0, 1 - pull.dy / 300));
    },
    onPointerUp: end, onPointerCancel: end,
  });
}
// The sheet comes in over the page: the tapped poster grows into the big one, the panel rises.
function sheetIn(from) {
  const panel = $('#panel'), scrim = $('#sheet .scrim');
  if (!panel || reduced()) return;
  const big = panel.querySelector('.tile.big:not(.sk)'), a = from && from.isConnected ? from.getBoundingClientRect() : null, b = big && big.getBoundingClientRect(), w = wide();
  anim(scrim, [{ opacity: 0 }, { opacity: 1 }], { duration: 300 });
  anim(panel, w ? [{ transform: 'translateX(48px)', opacity: 0 }, { transform: 'none', opacity: 1 }] : [{ transform: 'translateY(100%)' }, { transform: 'none' }], { duration: w ? 320 : 440, easing: 'cubic-bezier(.2,.9,.25,1)' });
  if (a && b && big && onScreen(a) && onScreen(b)) {
    hideEl(big, true); hideEl(from, true);
    settled(fly(from, a, b), () => { hideEl(big, false); hideEl(from, false); });
  }
}
// It goes the way it came: a copy of the sheet slides (or is let go) while the real one is already gone.
function sheetOut() {
  const sheet = $('#sheet'), panel = $('#panel'), scrim = $('#sheet .scrim'), dy = M.drag;
  M.drag = 0;
  if (!sheet || !panel || sheet.hidden) return;
  const op = scrim ? getComputedStyle(scrim).opacity : '1';
  panel.style.transform = ''; if (scrim) scrim.style.opacity = '';
  if (reduced()) return;
  const c = sheet.cloneNode(true);
  c.removeAttribute('id'); c.removeAttribute('hidden'); c.querySelectorAll('[id]').forEach((e) => e.removeAttribute('id'));
  c.inert = true; c.setAttribute('aria-hidden', 'true'); c.style.pointerEvents = 'none';
  document.body.append(c);
  const cp = c.querySelector('.panel'), cs = c.querySelector('.scrim'), w = wide();
  cp.style.transform = ''; cs.style.opacity = '';
  cp.scrollTop = panel.scrollTop;
  const out = anim(cp, w ? [{ transform: 'none', opacity: 1 }, { transform: 'translateX(48px)', opacity: 0 }] : [{ transform: `translateY(${dy}px)` }, { transform: 'translateY(110%)' }], { duration: 300, easing: 'cubic-bezier(.4,0,.9,.6)', fill: 'forwards' });
  anim(cs, [{ opacity: op }, { opacity: 0 }], { duration: 280, fill: 'forwards' });
  const o = P.opener, small = !dy && o && o.isConnected ? tileOf(o) : null, big = cp.querySelector('.tile.big:not(.sk)');
  if (small && big) {
    const a = big.getBoundingClientRect(), b = small.getBoundingClientRect();
    if (onScreen(a) && onScreen(b)) { hideEl(big, true); hideEl(small, true); settled(fly(big, a, b), () => hideEl(small, false)); }
  }
  settled(out, () => c.remove());
  setTimeout(() => { c.remove(); hideEl(small, false); }, 1200);
}
function skeleton() {
  const bar = (w, extra) => h('div', { class: 'sk', style: { width: w, ...extra } }), pk = P.peek;
  return h('div', { key: 'sk', class: 'stack', 'aria-busy': 'true' },
    pk ? h('header', { class: 'hero' }, h(Tile, { title: pk.title, mediaId: P.id, big: true }),
      h('div', { class: 'hero-t' }, h('h1', { id: 'sheet-title' }, pk.title), pk.year && h('p', { class: 'meta' }, pk.year)))
      : [h('span', { key: 't', id: 'sheet-title', class: 'sr' }, 'Loading'),
        h('div', { key: 'h', class: 'hero' }, h('div', { class: 'tile big sk-tile' }), h('div', { class: 'hero-t' }, bar('80%'), bar('50%')))],
    bar('100%'), bar('70%'),
    h('ul', { class: 'list' }, [1, 2, 3].map((i) => h('li', { key: i, class: 'item' }, h('div', { class: 'what' }, bar('40%'), bar('65%', { marginTop: '8px' }))))));
}
function errorBody() {
  return h('div', { key: 'err', class: 'stack' }, h('span', { id: 'sheet-title', class: 'sr' }, 'Something went wrong'),
    h(Hint, { text: P.err, role: 'status' }),
    h('button', { type: 'button', class: 'btn ghost', 'data-fk': 'retry', onClick: () => { P.err = ''; bump(); loadTitle(false); } }, ic('retry'), 'Try again'));
}
function hero(title, line, extra, by) {
  return h('header', { class: 'hero' }, h(Tile, { title, mediaId: P.data.media_id, big: true }),
    h('div', { class: 'hero-t' }, h('h1', { id: 'sheet-title' }, title), line && h('p', { class: 'meta' }, line), extra, by && h('p', { class: 'meta' }, `Added by ${by}`)));
}
function Genres({ list }) {
  return list && list.length ? h('ul', { class: 'genres', 'aria-label': 'Genres' }, list.map((g) => h('li', { key: g }, g))) : null;
}
function Cast({ list }) {
  if (!list || !list.length) return null;
  return h(Section, { id: 'cast', title: 'Cast' }, h('ul', { class: 'cast' }, list.map((c) => h('li', { key: c.name }, h('b', null, c.name), c.character ? h('span', null, c.character) : null))));
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
// The one-word state of a movie, season or episode on the open page, as it was last read.
function stateOf(scope) {
  const d = P.data;
  if (!d) return null;
  if (/^movie:/.test(scope)) return d.state;
  const s = /^season:[a-z]+:\d+:(\d+)$/.exec(scope), e = /^episode:[a-z]+:\d+:(\d+):\d+$/.exec(scope);
  if (s) return ((d.seasons || []).find((x) => x.number === Number(s[1])) || {}).state || null;
  if (e) return (((P.seasons.get(Number(e[1])) || {}).eps || []).find((x) => x.media_id === scope) || {}).state || null;
  return null;
}
// Undo for a Get on the page: only when Arr did not track the title at all before (the page said
// removable: false, meaning Arr does not manage it). It removes the whole title (a movie, or the show a season or episode belongs to).
// Unsure means no Undo. `id` is the title's, read before the request: the page may show another
// title by the time the answer comes back.
function undoGet(tk, id) {
  return async () => {
    const m = await removeTitle(id);
    if (m.ok && tk === P.token && P.shown) loadTitle(true);
    return m;
  };
}
async function act(key, kind, scope) {
  const tk = P.token, id = P.data ? P.data.media_id : '', title = P.data ? P.data.title : '', was = kind === 'get' && P.data && P.data.removable === false ? stateOf(scope) : null;
  P.seq++; // an older read must not overwrite what this changes
  bust();
  P.busy.add(key); P.ask = ''; P.msg.delete(key); bump();
  let res, got = null;
  try {
    const body = { media_id: scope };
    if (kind === 'get' && pickedLevel()) body.level = pickedLevel();
    const r = await api(kind === 'get' ? '/v1/quick-add' : '/v1/stop', { method: 'POST', body });
    if (!r.ok) res = { text: "Couldn't do that just now. Try again in a moment.", err: true };
    else if (kind === 'get') { got = r.data; res = { text: r.data.message || 'Done.', err: r.data.outcome === 'movies_off' }; }
    else if (r.data.cancelled > 0) res = { text: 'Stopped. Arr cancelled the download and will not look for it.' };
    else if (r.data.finishing > 0) res = { text: 'Stopped looking. A download that is nearly done will still finish.' };
    else res = { text: 'Stopped. Arr will not look for it.' };
  } catch { res = { text: "Couldn't reach Arr. Try again in a moment.", err: true }; }
  bust();
  if (got && !res.err) toastAdded(got.title || title, got.outcome, got.detail, was === 'none' && id ? undoGet(tk, id) : null);
  if (tk !== P.token) return;
  P.busy.delete(key); P.msg.set(key, res); say(res.text);
  // Reads the action made stale are issued again (also after a failure), so nothing stays loading.
  bump();
  await loadTitle(true);
}
async function toggleFollow() {
  if (P.followBusy) return;
  const want = !P.data.following, tk = P.token, fresh = P.data.removable === false;
  P.seq++; bust();
  P.followBusy = true; P.data.following = want; P.msg.delete('follow'); bump();
  let bad = false;
  try {
    const r = await api('/v1/follow', { method: 'POST', body: { media_id: P.data.media_id, on: want, ...(want && pickedLevel() ? { level: pickedLevel() } : {}) } });
    bad = !r.ok;
  } catch { bad = true; }
  bust();
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
  if (want) {
    const id = P.data.media_id;
    showToast({ before: 'Following ', title: P.data.title, after: '' }, !fresh ? null : async () => {
      const m = await removeTitle(id);
      if (m.ok && tk === P.token && P.shown) loadTitle(true);
      return m;
    });
  }
  await loadTitle(true);
}

// ----- remove a title from Arr (files are never touched), or delete its files (into the bin)
function removeNode() {
  const d = P.data, files = P.kind === 'movie' ? d.state === 'in_plex' : (d.seasons || []).some((s) => s.in_plex > 0);
  if (!d.removable && !files) return null;
  return h('div', { class: 'remove' },
    d.removable && h('button', { type: 'button', class: 'link danger', 'data-fk': 'remove', onClick: () => { P.confirm = true; P.msg.delete('remove'); after(() => $('#confirm [data-fk="rm-cancel"]')?.focus({ preventScroll: true })); } }, 'Remove from Arr'),
    files && deleteLink('delete', 'Delete files…', d.media_id, d.title, `Delete the files of ${d.title}`),
    msgFor('remove'), msgFor('delete'));
}
const deleteLink = (fk, text, scope, noun, label) =>
  h('button', { type: 'button', class: 'link danger', 'data-fk': fk, 'aria-label': label, onClick: () => openDelete(fk, scope, noun) }, text);
// The question: what would go (read fresh from Arr), then Cancel or Delete. Nothing is deleted
// until the person taps Delete, and the files go to the bin, where they stay for 7 days.
async function openDelete(fk, scope, noun) {
  const tk = P.token;
  P.del = { fk, scope, noun, st: 'loading', pv: null, keep: false, busy: false, err: '' };
  P.msg.delete('delete');
  after(() => $('#confirm [data-fk="del-cancel"]')?.focus({ preventScroll: true }));
  let st = 'err', pv = null, err = "Couldn't look at the files just now. Try again in a moment.";
  try {
    const r = await api('/v1/delete/preview', { method: 'POST', body: { media_id: scope } });
    if (r.ok) { st = 'ready'; pv = r.data; err = ''; } else if (r.status === 409 && r.data && r.data.error) err = "Arr won't delete these: " + r.data.error.message;
  } catch { err = "Couldn't reach Arr. Try again in a moment."; }
  if (tk !== P.token || !P.del || P.del.scope !== scope) return;
  Object.assign(P.del, { st, pv, err });
  bump();
}
function closeDelete() {
  const fk = P.del && P.del.fk;
  P.del = null;
  after(() => $(`#panel [data-fk="${fk}"]`)?.focus({ preventScroll: true }));
}
async function restoreEntry(id) {
  const r = await api('/v1/bin/' + enc(id) + '/restore', { method: 'POST' });
  if (!r.ok) return { ok: false, text: "Couldn't put the files back just now. Try again in a moment." };
  bust(); S.stale = true; loadFeed();
  const n = r.data.restored, left = (r.data.skipped || []).length;
  return { ok: true, text: left ? `${plural(n, 'file is', 'files are')} back. ${plural(left, 'file', 'files')} could not go back because something is already there.` : `${plural(n, 'file is', 'files are')} back.` };
}
async function doDelete() {
  const dl = P.del;
  if (!dl || dl.busy || dl.st !== 'ready' || !dl.pv.files.length || (dl.pv.blocked && !dl.keep)) return;
  const tk = P.token, d = P.data, scope = dl.scope, whole = scope === d.media_id;
  dl.busy = true; bump();
  let res;
  try {
    const r = await api('/v1/delete', { method: 'POST', body: { media_id: scope, token: dl.pv.token, keep_wanted: dl.keep } });
    const code = r.data && r.data.error && r.data.error.code;
    if (r.ok) res = { ok: true, id: r.data.delete_id, kept: r.data.downloads_kept > 0 };
    else if (code === 'active_downloads') res = { text: 'A download is nearly done and is being added to Plex. Try again in a minute.', err: true };
    else if (code === 'changed') res = { text: 'Something changed in the files. Look again before deleting.', err: true };
    else if (code === 'move_failed') res = { text: "A file couldn't be moved, so nothing was deleted.", err: true };
    else if (code === 'delete_refused') res = { text: "Arr won't delete these: " + r.data.error.message, err: true };
    else res = { text: "Couldn't delete the files just now. Try again in a moment.", err: true };
  } catch { res = { text: "Couldn't reach Arr. Try again in a moment.", err: true }; }
  if (tk !== P.token) return;
  P.del = null;
  if (!res.ok) { P.msg.set('delete', res); say(res.text); bump(); after(() => $(`#panel [data-fk="${dl.fk}"]`)?.focus({ preventScroll: true })); return; }
  forget(d.media_id); bust();
  S.stale = true; loadFeed();
  say(`${dl.noun} deleted. It stays in the bin for 7 days.`);
  const undo = async () => {
    const m = await restoreEntry(res.id);
    if (tk === P.token && P.shown) loadTitle(true);
    return m;
  };
  showToast({ before: 'Deleted ', title: dl.noun, after: '. Undo within 7 days.' + (res.kept ? ' A download is still being shared, so some space stays used.' : '') }, undo);
  // A whole show leaves Arr unless a new copy is wanted; the page is then closed like Remove does.
  if (whole && P.kind === 'show' && !dl.keep) dismiss(); else await loadTitle(true);
}
function ConfirmDelete() {
  const dl = P.del, pv = dl.pv, busy = dl.busy, files = pv ? pv.files.length : 0;
  const shared = pv ? pv.downloads.filter((x) => !x.removed) : [];
  const parts = [];
  if (dl.st === 'loading') parts.push(h('p', { key: 'l', role: 'status' }, 'Looking at the files…'));
  else if (dl.st === 'err') parts.push(h('p', { key: 'e', class: 'msg', role: 'alert', 'data-err': '' }, dl.err));
  else if (!files) parts.push(h('p', { key: 'n' }, 'Arr has no files of ', h('em', null, dl.noun), ' that it can delete.'));
  else {
    parts.push(h('p', { key: 'f' }, `${plural(files, 'file', 'files')}${fmtSize(pv.library_bytes) ? ` (${fmtSize(pv.library_bytes)})` : ''} of `, h('em', null, dl.noun), " will move to Arr's bin. They stay there for 7 days, and the space comes back then or when you empty the bin in Settings."));
    if (pv.downloads.some((x) => x.removed)) parts.push(h('p', { key: 'd', class: 'hint' }, 'Arr also removes the finished download, so the space is really freed.'));
    if (shared.length) parts.push(h('p', { key: 's', class: 'hint' }, 'A download also holds episodes you are keeping. It stays, so some space stays used.'));
    if (pv.blocked && !dl.keep) parts.push(h('p', { key: 'b', class: 'msg', role: 'alert', 'data-err': '' }, pv.blocked));
    if (pv.unknown) parts.push(h('p', { key: 'u', class: 'hint' }, `Arr couldn't find the file for ${plural(pv.unknown, 'item', 'items')}, so ${pv.unknown === 1 ? 'it stays' : 'they stay'}.`));
    parts.push(h('label', { key: 'k', class: 'chk' }, h('input', { type: 'checkbox', checked: dl.keep, disabled: busy, onChange: (e) => { dl.keep = e.target.checked; bump(); } }), h('span', null, 'Get a new copy afterwards')));
  }
  const ready = dl.st === 'ready' && files > 0 && !(pv.blocked && !dl.keep);
  return h('div', { class: 'confirm-back', onClick: (e) => { if (e.target === e.currentTarget && !dl.busy) closeDelete(); } },
    h('div', { id: 'confirm', class: 'confirm', role: 'alertdialog', 'aria-modal': 'true', 'aria-labelledby': 'confirm-t', 'aria-busy': dl.st === 'loading' ? 'true' : 'false' },
      h('h2', { id: 'confirm-t' }, `Delete the files of ${dl.noun}?`),
      parts,
      h('div', { class: 'confirm-act' },
        h('button', { type: 'button', class: 'btn ghost', 'data-fk': 'del-cancel', 'aria-disabled': busy ? 'true' : 'false', onClick: () => { if (!dl.busy) closeDelete(); } }, 'Cancel'),
        h('button', { type: 'button', class: 'btn danger', 'data-fk': 'del-go', 'data-s': busy ? 'busy' : null, 'aria-disabled': ready && !busy ? 'false' : 'true', onClick: doDelete }, busy ? 'Deleting…' : 'Delete'))));
}
function closeConfirm() {
  P.confirm = false;
  after(() => $('#panel [data-fk="remove"]')?.focus({ preventScroll: true }));
}
// Forget what the page remembers of an add or a retry for this title, so it can be added again.
function forget(id) {
  const bare = id.replace(/^series:/, '');
  for (const k of [...A.keys()]) if (k === id || (k.startsWith('r:') && k.includes(bare))) A.delete(k);
}
async function doRemove() {
  if (P.removing) return;
  const tk = P.token, d = P.data;
  P.removing = true; bump();
  let res = null;
  try {
    const r = await api('/v1/remove', { method: 'POST', body: { media_id: d.media_id } });
    const code = r.data && r.data.error && r.data.error.code;
    if (r.ok) res = { ok: true };
    else if (code === 'active_downloads') res = { text: 'A download is nearly done and is being added to Plex. Try again in a minute.', err: true };
    else if (code === 'not_managed') res = { text: 'Arr is not tracking this one.', err: true };
    else res = { text: "Couldn't remove it just now. Try again in a moment.", err: true };
  } catch { res = { text: "Couldn't reach Arr. Try again in a moment.", err: true }; }
  if (tk !== P.token) return;
  P.removing = false;
  if (!res.ok) { P.msg.set('remove', res); say(res.text); closeConfirm(); return; }
  P.confirm = false;
  forget(d.media_id); bust();
  S.stale = true; // the screen under the sheet is drawn again, without this title
  dismiss();
  say(`${d.title} removed. Anything already in Plex stays there.`);
}
function ConfirmRemove() {
  const d = P.data, busy = P.removing;
  return h('div', { class: 'confirm-back', onClick: (e) => { if (e.target === e.currentTarget && !P.removing) closeConfirm(); } },
    h('div', { id: 'confirm', class: 'confirm', role: 'alertdialog', 'aria-modal': 'true', 'aria-labelledby': 'confirm-t', 'aria-describedby': 'confirm-d' },
      h('h2', { id: 'confirm-t' }, `Remove ${d.title} from Arr?`),
      h('p', { id: 'confirm-d' }, 'Arr will stop getting new copies and episodes of ', h('em', null, d.title), '. Anything already in Plex stays there. To delete files, use “Delete files…”.'),
      h('div', { class: 'confirm-act' },
        h('button', { type: 'button', class: 'btn ghost', 'data-fk': 'rm-cancel', 'aria-disabled': busy ? 'true' : 'false', onClick: () => { if (!P.removing) closeConfirm(); } }, 'Cancel'),
        h('button', { type: 'button', class: 'btn danger', 'data-fk': 'rm-go', 'data-s': busy ? 'busy' : null, 'aria-disabled': busy ? 'true' : 'false', onClick: doRemove }, busy ? 'Removing…' : 'Remove'))));
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
  o.saving = true; bust(); bump();
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
  const scope = o.copies.data.media_id, was = P.data.removable === false ? stateOf(scope) : null, title = P.data.title, id = P.data.media_id;
  let res;
  try {
    const r = await api(`/v1/copies/${enc(scope)}/get`, { method: 'POST', body: { id: c.id, ...(pickedLevel() ? { level: pickedLevel() } : {}) } });
    if (r.ok) res = { text: r.data.message || 'Arr is getting that copy now.' };
    else res = { text: PICK_ERRORS[r.data && r.data.error && r.data.error.code] || "Couldn't start that one just now. Try again in a moment.", err: true };
  } catch { res = { text: "Couldn't reach Arr. Try again in a moment.", err: true }; }
  bust();
  if (tk !== P.token) return;
  o.picking = '';
  if (!res.err) {
    const season = /^season:[a-z]+:\d+:(\d+)$/.exec(scope);
    toastAdded(title, 'on_its_way', season ? 'Season ' + season[1] : '', was === 'none' ? undoGet(tk, id) : null);
  }
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
    (kind || e.state === 'in_plex') && h('div', { class: 'act' },
      kind && actionNode(key, kind, kind === 'get' ? 'Get' : 'Stop', `episode ${e.number}, ${title}`, e.media_id),
      e.state === 'in_plex' && deleteLink('del:' + e.media_id, 'Delete…', e.media_id, `${P.data.title}, episode ${e.number}`, `Delete the file of episode ${e.number}, ${title}`)));
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
    open && h('div', { id: 'eps-' + n },
      episodesNode(n),
      s.in_plex > 0 && h('div', { class: 'sea-del' }, deleteLink('del:' + scope, 'Delete this season’s files…', scope, `${P.data.title}, season ${n}`, `Delete the files of season ${n}`))));
}
function showBody() {
  const d = P.data, on = d.following;
  return h('div', { key: 'show', class: 'stack' },
    hero(d.title, joinDot(d.year, d.network, d.status, d.runtime_minutes && `${d.runtime_minutes}m episodes`), scoreLine(d.scores), d.requested_by),
    h(Summary, { text: d.summary }),
    h(Genres, { list: d.genres }),
    h('div', null, h('button', { type: 'button', role: 'switch', class: 'switch', 'aria-checked': String(on), 'aria-labelledby': 'sw-t', 'data-fk': 'follow', onClick: toggleFollow },
      h('span', { class: 'what' }, h('span', { class: 'sw-t', id: 'sw-t' }, 'Get new episodes as they air'),
        h('span', { class: 'sw-m' }, on ? 'Arr gets each new episode when it airs.' : 'Off. Arr is not looking for new episodes.')),
      h('span', { class: 'track', 'aria-hidden': 'true' }, h('i'))), msgFor('follow')),
    optionsPanel(),
    removeNode(),
    h(Section, { id: 'seasons', title: 'Seasons' }, d.seasons.length ? h('ul', { class: 'list' }, d.seasons.map(seasonItem)) : h(Hint, { text: 'No seasons are listed for this show yet. Check back soon.' })),
    h(Cast, { list: d.cast }));
}

// ----- the movie page
function movieBody() {
  const d = P.data, st = d.state, key = 'movie';
  const p = typeof d.progress === 'number' ? d.progress : null;
  const line = st === 'in_plex' ? 'In Plex' : st === 'on_its_way' ? 'On its way' + (p == null ? '' : ` · ${pct(p)}%`)
    : st === 'waiting' ? (d.until ? `Out ${fmtDate(d.until)}. Arr will get it then.` : 'Not out yet. Arr will get it then.')
      : st === 'wanted' ? 'Arr is looking for a copy' : 'Not in Plex yet';
  let control = null;
  if (st === 'in_plex') control = null;
  else if (st === 'none') control = actionNode(key, 'get', 'Get', d.title, d.media_id);
  else control = actionNode(key, 'stop', 'Stop', d.title, d.media_id);
  return h('div', { key: 'movie', class: 'stack' },
    hero(d.title, joinDot(d.year, d.runtime_minutes && fmtRuntime(d.runtime_minutes)), scoreLine(d.scores), d.requested_by),
    h(Summary, { text: d.summary }),
    h(Genres, { list: d.genres }),
    h('section', { class: 'item state' + (st === 'in_plex' ? ' arrived' : ''), 'aria-label': 'Status' },
      h('div', { class: 'what' },
        st === 'in_plex' ? h('span', { class: 'tag' }, ic('check'), 'In Plex') : null,
        h('p', { class: 'st' }, st === 'in_plex' ? 'Ready to watch' : line),
        st === 'on_its_way' ? h(Journey, { stage: p == null ? 0 : 1, p, fkey: 'movie:' + d.media_id, live: p != null }) : null,
        msgFor(key)),
      control && h('div', { class: 'act' }, control)),
    optionsPanel(),
    h(Cast, { list: d.cast }),
    removeNode());
}

// ---------- sign in ----------
function showSignin(data) {
  S.out = true; authGen++; token++; dropFeed();
  clearTimeout(pollT); clearTimeout(retryT);
  setBanner(''); S.setup = false;
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
const personRoute = (path) => {
  const m = /^\/person\/(.+)$/.exec(path);
  try { const id = m && decodeURIComponent(m[1]); return id && /^(plex:[A-Za-z0-9]+|tvmaze:\d+)$/.test(id) ? id : null; } catch { return null; }
};
const pageName = (path) => (path === '/search' || personRoute(path) ? 'Search' : path === '/ideas' ? 'Ideas' : path === '/library' ? 'Library' : path === '/settings' ? 'Settings' : 'Home');
const TABS = [['/', 'Home', 'home'], ['/ideas', 'Ideas', 'ideas'], ['/library', 'Library', 'library'], ['/settings', 'Settings', 'settings']];
function route() {
  if (S.out) return;
  const { path, q } = parseHash();
  const tr = titleRoute(path);
  if (tr) return openTitle(tr);
  const hash = location.hash || '#/';
  if (P.shown) {
    closeSheet();
    // Back to the screen the sheet was opened over: it is still there as it was.
    if (hash === bgHash) {
      if (S.stale) { S.stale = false; renderPath(path, q, hash); } else if (onHome()) loadFeed();
      return;
    }
  }
  renderPath(path, q, hash);
}
// A new screen: tabs slide the way they sit in the bar, any other change is a short rise.
function enterRoute(prev, path) {
  if (prev === path) return;
  M.fresh = true;
  if (prev === null) return;
  const a = TABS.findIndex((t) => t[0] === prev), b = TABS.findIndex((t) => t[0] === path), dir = a >= 0 && b >= 0 ? Math.sign(b - a) : 0;
  after(() => {
    anim($('#view'), [{ opacity: 0, transform: dir ? `translateX(${dir * 28}px)` : 'translateY(8px)' }, { opacity: 1, transform: 'none' }], { duration: 300 });
    if (b >= 0) anim($('#tabs a[aria-current] .ic'), [{ transform: 'none' }, { transform: 'translateY(-7px) scale(1.25) rotate(-8deg)', offset: 0.4 }, { transform: 'none' }], { duration: 520, easing: SPRING });
  });
}
function renderPath(path, q, hash) {
  bgHash = hash;
  enterRoute(R.path, path);
  const t = ++token;
  clearTimeout(retryT); clearTimeout(pollT); clearTimeout(debounceT);
  R.path = path;
  searchAbort?.abort();
  document.title = pageName(path) + ' · Arr';
  $('#q').value = path === '/search' ? q : '';
  if (path === '/search') renderSearch(q, t);
  else if (personRoute(path)) renderPerson(personRoute(path), t);
  else if (path === '/ideas') renderIdeas(t);
  else if (path === '/library') { M.fresh = false; setView({ t: 'library', tok: t }); }
  else if (path === '/settings') renderSettings();
  else renderHome(t);
}

// ---------- the page ----------
function View() {
  switch (V.t) {
    case 'skel': return h(SkelView, { text: V.text });
    case 'home': return h(Home);
    case 'ideas': return h(IdeasPage);
    case 'search': return h(SearchResults);
    case 'person': return h(PersonPage);
    case 'start': return h(SearchStart);
    case 'library': return h(Library, { key: V.tok, t: V.tok });
    case 'settings': return h(Settings);
    case 'signin': return h(Signin, { google: V.google });
    default: return h(Hint, { text: V.text, role: V.role });
  }
}

// ---- Use Arr on your phone: the address, a QR code and how to put Arr on the home screen
const REMOTE_DOCS = 'https://github.com/newtonharry/Arr/blob/main/docs/REMOTE.md';

// The code is always dark on white, whatever the theme: phones scan it that way.
let qrP = null;
function loadQr() {
  return (qrP ||= new Promise((ok, no) => {
    if (self.qrcodegen) return ok();
    const el = document.createElement('script');
    el.src = '/app/qrcodegen.js';
    el.onload = () => ok();
    el.onerror = () => { qrP = null; el.remove(); no(new Error('qr')); };
    document.head.append(el);
  }));
}
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
  const [qr, setQr] = useState(!!self.qrcodegen);
  // The code's script is only fetched when this section opens; the address shows without it.
  useEffect(() => { if (url && !qr) loadQr().then(() => setQr(true), () => {}); }, [url]);
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
    qr ? h(QrCode, { text: url }) : null,
    h('div', { class: 'how' },
      h('p', null, h('b', null, 'iPhone: '), 'open the address in Safari, tap Share, then Add to Home Screen.'),
      h('p', null, h('b', null, 'Android: '), 'open the address in Chrome, tap the menu (three dots), then Add to Home screen or Install app.')));
}
// ---------- notifications: standard Web Push, opt-in on each device ----------
// The service worker lives at /app/sw.js, so its scope is /app/ (the page itself is at /).
const SW_SCOPE = '/app/';
const isIos = /iPad|iPhone|iPod/.test(navigator.userAgent) || (navigator.platform === 'MacIntel' && navigator.maxTouchPoints > 1);
const isStandalone = () => (window.matchMedia && matchMedia('(display-mode: standalone)').matches) || navigator.standalone === true;
const canPush = () => 'serviceWorker' in navigator && 'PushManager' in window && 'Notification' in window;
const keyBytes = (b64) => Uint8Array.from(atob(b64.replace(/-/g, '+').replace(/_/g, '/')), (c) => c.charCodeAt(0));
// `navigator.serviceWorker.ready` never settles for a worker outside the page's scope, so wait on the worker itself.
function activated(reg) {
  const w = reg.installing || reg.waiting || reg.active;
  if (!w || w.state === 'activated') return Promise.resolve(reg);
  return new Promise((ok, no) => w.addEventListener('statechange', () => {
    if (w.state === 'activated') ok(reg); else if (w.state === 'redundant') no(new Error('worker failed'));
  }));
}
// 'on', 'off', 'blocked', 'home' (iPhone: add to the Home Screen first) or 'none' (this browser can't).
async function pushState() {
  if (isIos && !isStandalone()) return 'home';
  if (!canPush()) return 'none';
  if (Notification.permission === 'denied') return 'blocked';
  try {
    const reg = await navigator.serviceWorker.getRegistration(SW_SCOPE);
    return reg && (await reg.pushManager.getSubscription()) ? 'on' : 'off';
  } catch { return 'off'; }
}
async function turnOn() {
  if ((await Notification.requestPermission()) !== 'granted') return Notification.permission === 'denied' ? 'blocked' : 'off';
  const reg = await activated(await navigator.serviceWorker.register('/app/sw.js', { scope: SW_SCOPE }));
  const k = await api('/v1/notifications/key');
  if (!k.ok) throw new Error('key');
  // A subscription made for another key (Arr was set up again) has to go first.
  const old = await reg.pushManager.getSubscription();
  if (old) await old.unsubscribe();
  const sub = await reg.pushManager.subscribe({ userVisibleOnly: true, applicationServerKey: keyBytes(k.data.key) });
  const r = await api('/v1/notifications/subscribe', { method: 'POST', body: sub.toJSON() });
  if (!r.ok) { await sub.unsubscribe(); throw new Error('subscribe'); }
  return 'on';
}
async function turnOff() {
  const reg = await navigator.serviceWorker.getRegistration(SW_SCOPE);
  const sub = reg && await reg.pushManager.getSubscription();
  if (sub) {
    await sub.unsubscribe();
    await api('/v1/notifications/subscribe', { method: 'DELETE', body: { endpoint: sub.endpoint } });
  }
  return 'off';
}
const NOTIFY_TEXT = {
  on: 'On', off: 'Off',
  blocked: 'Blocked in your browser settings',
  home: 'Add Arr to your Home Screen first',
  none: "This browser can't show notifications",
};
function Notify() {
  const [st, setSt] = useState(null);
  const [busy, setBusy] = useState(false);
  const [err, setErr] = useState('');
  useEffect(() => { pushState().then(setSt); }, []);
  const toggle = async () => {
    setBusy(true); setErr('');
    try { setSt(st === 'on' ? await turnOff() : await turnOn()); }
    catch (e) { if (e.message !== 'signed out') setErr("Couldn't change that. Try again."); setSt(await pushState()); }
    setBusy(false);
  };
  const usable = st === 'on' || st === 'off';
  return h('section', { class: 'notify', 'aria-label': 'Notifications' },
    h('div', { class: 'notify-row' },
      h('span', { id: 'notify-label' }, 'Notifications'),
      h('button', { type: 'button', class: 'toggle', id: 'notify-switch', role: 'switch', 'aria-checked': String(st === 'on'), 'aria-labelledby': 'notify-label notify-state', disabled: !usable || busy, onClick: toggle }, h('span', { class: 'knob' })),
      h('span', { id: 'notify-state', class: 'notify-state', role: 'status' }, st ? NOTIFY_TEXT[st] : '')),
    err ? h('p', { class: 'notify-err', role: 'alert' }, err) : null,
    h('p', { class: 'notify-note' }, 'Get a message on this device when something is ready to watch or needs you. It goes through your browser maker’s push service (Google, Apple or Mozilla) and carries only a title. On iPhone this works from the Home Screen, on iOS 16.4 or later.'));
}
// ---------- updates, settings and health ----------
const REPO = 'https://github.com/newtonharry/Arr';
const PRIVACY = REPO + '/blob/main/docs/PRIVACY.md';
// A read that never raises the "can't reach Arr" banner: Arr is meant to be away while it restarts.
async function quietGet(path) {
  try {
    const r = await fetch('/app/api' + path, { credentials: 'same-origin', headers: { 'X-Arr-App': '1' } });
    return r.ok ? await r.json() : null;
  } catch { return null; }
}
const errMsg = (r, fallback) => (r.data && r.data.error && r.data.error.message) || fallback;
const whatsNew = (href) => h('a', { href, target: '_blank', rel: 'noopener noreferrer' }, 'What’s new');

// "Update now". U.phase: idle, waiting (the updater is working), done, failed.
const U = { info: null, phase: 'idle', from: '', prevAt: null, err: '', pollT: 0, updated: '', seen: false, checkedAt: 0 };
// The version this browser last saw, kept so a change (even a nightly one) is told once.
function noteVersion(current) {
  try {
    const was = localStorage.getItem('arr.seenVersion');
    localStorage.setItem('arr.seenVersion', current);
    return was && was !== current ? current : '';
  } catch { return ''; /* storage blocked: nothing to compare with */ }
}
async function loadUpdate() {
  U.checkedAt = Date.now();
  try {
    const r = await api('/v1/update');
    if (!r.ok) return;
    U.info = r.data;
    if (!U.seen) { U.seen = true; U.updated = noteVersion(r.data.current); }
    bump();
  } catch { /* the banner says Arr can't be reached */ }
}
async function startUpdate() {
  if (U.phase === 'waiting' || !U.info) return;
  U.err = '';
  let r;
  try { r = await api('/v1/update', { method: 'POST' }); } catch { return; }
  if (!r.ok) { U.err = errMsg(r, 'Couldn’t start the update.'); bump(); return; }
  U.phase = 'waiting'; U.from = U.info.current; U.prevAt = U.info.status ? U.info.status.at : null;
  const since = Date.now();
  bump(); say('Updating. Arr will be back in a minute.');
  const poll = async () => {
    const d = await quietGet('/v1/update');
    if (d && d.current && d.current !== U.from) {
      U.info = d; U.phase = 'done'; bump(); say(`Updated to ${d.current}.`);
      setTimeout(() => location.reload(), 1800);
      return;
    }
    // The updater says "failed" when the new version didn't start and the old one came back.
    if ((d && d.status && d.status.state === 'failed' && d.status.at !== U.prevAt) || Date.now() - since > 10 * 60 * 1000) {
      if (d) U.info = d;
      U.phase = 'failed'; bump(); return;
    }
    U.pollT = setTimeout(poll, 3000);
  };
  U.pollT = setTimeout(poll, 3000);
}
function clearUpdate() { U.phase = 'idle'; U.updated = ''; U.err = ''; bump(); }
function UpdateBar() {
  const i = U.info;
  if (!S.me || S.out) return null;
  const close = h('button', { type: 'button', class: 'link', onClick: clearUpdate }, 'Dismiss');
  let line = null, bad = false;
  if (U.phase === 'waiting') line = ['Updating… Arr will be back in a minute.'];
  else if (U.phase === 'done') line = [`Updated to ${i.current} · `, whatsNew(`${REPO}/releases/tag/v${i.current}`)];
  else if (U.phase === 'failed') { bad = true; line = [`The update didn’t work, so Arr is still on ${U.from}. `, close]; }
  else if (U.err) { bad = true; line = [U.err + ' ', close]; }
  else if (U.updated) line = [`Updated to ${U.updated} · `, whatsNew(`${REPO}/releases/tag/v${U.updated}`), ' ', close];
  else if (i && i.available && i.can_update) line = [`Arr ${i.latest} is available · `, h('button', { type: 'button', class: 'link', id: 'update-now', onClick: startUpdate }, 'Update now')];
  if (!line) return null;
  return h('div', { class: 'upd-wrap', inert: P.shown }, h('p', { class: 'upd' + (bad ? ' bad' : ''), id: 'update-bar', role: bad ? 'alert' : 'status' }, line));
}

// Settings: the few things a person may change from a phone. Every change restarts Arr.
const G = { data: null, health: null, busy: '', msg: '', err: '', restarting: false, restartMsg: '', draft: { upload: '', port: '', key: '' } };
async function loadSettings() {
  try {
    const r = await api('/v1/settings/app');
    if (!r.ok) return;
    G.data = r.data;
    G.draft = { upload: r.data.upload_limit_kib == null ? '' : String(r.data.upload_limit_kib), port: r.data.torrent_port ? String(r.data.torrent_port) : '', key: '' };
    bump();
  } catch { /* the banner says Arr can't be reached */ }
}
async function loadHealth() {
  try {
    const r = await api('/v1/system/health');
    if (r.ok) { G.health = r.data; bump(); }
  } catch { /* the banner says Arr can't be reached */ }
}
async function renderSettings() {
  G.msg = ''; G.err = '';
  setView({ t: 'settings' });
  // Open at the top, where "You" and Sign out are, not where the last screen was scrolled to.
  window.scrollTo(0, 0);
  await Promise.all([loadSettings(), loadHealth(), loadUpdate(), loadBin()]);
}
// The bin: what "Delete files" moved out of the library, kept for 7 days.
const B = { entries: [], busy: '', ask: '', msg: '', err: false, loaded: false };
const binRow = (id) => document.querySelector(`[data-fk="bin-restore:${CSS.escape(id)}"], [data-fk="bin-go:${CSS.escape(id)}"]`)?.closest('li');
async function loadBin() {
  try {
    const r = await api('/v1/bin');
    if (r.ok) {
      // An entry that turns up while the page is open pops in.
      const had = new Set(B.entries.map((e) => e.delete_id)), first = !B.loaded;
      B.entries = r.data.entries; B.loaded = true; bump();
      const added = first ? [] : B.entries.map((e) => e.delete_id).filter((x) => !had.has(x));
      if (added.length) after(() => added.forEach((x) => anim(binRow(x), [{ transform: 'scale(.6)', opacity: 0 }, { transform: 'none', opacity: 1 }], { duration: 480, easing: SPRING })));
    }
  } catch { /* the banner says Arr can't be reached */ }
}
async function binAct(id, what) {
  if (B.busy) return;
  B.busy = id; B.ask = ''; B.msg = ''; bump();
  let m;
  try {
    if (what === 'restore') m = await restoreEntry(id);
    else {
      const r = await api('/v1/bin/' + enc(id), { method: 'DELETE' });
      m = r.ok ? { ok: true, text: 'Deleted for good. The space is free.' } : { ok: false, text: "Couldn't empty that just now. Try again in a moment." };
    }
  } catch { m = { ok: false, text: "Couldn't reach Arr. Try again in a moment." }; }
  B.busy = ''; B.msg = m.text; B.err = !m.ok; say(m.text);
  // A deleted row drops away; a restored one pops back out. Then the list closes the gap.
  const gone = m.ok ? anim(binRow(id), what === 'restore' ? [{ transform: 'none', opacity: 1 }, { transform: 'scale(1.06)', opacity: 0 }] : [{ transform: 'none', opacity: 1 }, { transform: 'translateY(4px) rotate(-3deg) scale(.97)', opacity: 1, offset: 0.3 }, { transform: 'translateY(70px) rotate(-9deg) scale(.8)', opacity: 0 }], { duration: what === 'restore' ? 300 : 440, easing: what === 'restore' ? EASE : 'cubic-bezier(.5,0,.8,.4)', fill: 'forwards' }) : NOANIM;
  await settled(gone, () => {});
  await loadBin();
  gone.cancel();
  bump();
}
const binScope = (id) => {
  const s = /^season:[a-z]+:\d+:(\d+)$/.exec(id), e = /^episode:[a-z]+:\d+:(\d+):(\d+)$/.exec(id);
  return s ? ` · Season ${s[1]}` : e ? ` · Season ${e[1]}, episode ${e[2]}` : '';
};
function BinSection() {
  if (!B.entries.length && !B.msg) return null;
  return h(Section, { id: 'bin', title: 'Bin' },
    B.entries.length ? h('p', { class: 'hint' }, 'Files you deleted stay here for 7 days. Put them back, or empty them to free the space now.') : null,
    B.msg ? h('p', { class: 'msg', role: 'status', 'data-err': B.err ? '' : null }, B.msg) : null,
    h('ul', { class: 'list' }, B.entries.map((e) => h('li', { key: e.delete_id, class: 'item bin', 'data-flip': 'bin:' + e.delete_id },
      h('div', { class: 'what' }, h('h3', null, e.title, binScope(e.media_id)),
        h('p', { class: 'meta' }, `${plural(e.files, 'file', 'files')}${fmtSize(e.bytes) ? ` · ${fmtSize(e.bytes)}` : ''} · empties ${fmtDate(e.purge_at)}`)),
      B.ask === e.delete_id
        ? h('span', { class: 'ask' }, h('span', { class: 'ask-t' }, 'Delete for good?'),
          h('button', { type: 'button', class: 'btn danger', 'data-fk': 'bin-go:' + e.delete_id, 'aria-label': `Confirm: delete ${e.title} for good`, onClick: () => binAct(e.delete_id, 'purge') }, 'Delete'),
          h('button', { type: 'button', class: 'btn ghost', 'aria-label': 'Keep it in the bin', onClick: () => { B.ask = ''; bump(); } }, 'Keep'))
        : h('div', { class: 'act' },
          h('button', { type: 'button', class: 'btn', 'data-fk': 'bin-restore:' + e.delete_id, 'data-s': B.busy === e.delete_id ? 'busy' : null, 'aria-disabled': B.busy ? 'true' : 'false', 'aria-label': `Restore ${e.title}${binScope(e.media_id)}`, onClick: () => binAct(e.delete_id, 'restore') }, 'Restore'),
          h('button', { type: 'button', class: 'link danger', 'data-fk': 'bin-purge:' + e.delete_id, 'aria-disabled': B.busy ? 'true' : 'false', 'aria-label': `Delete ${e.title}${binScope(e.media_id)} now`, onClick: () => { if (!B.busy) { B.ask = e.delete_id; bump(); } } }, 'Delete now'))))));
}
async function applySetting(name, value) {
  if (G.busy || G.restarting) return;
  G.busy = name; G.err = ''; G.msg = ''; bump();
  let r;
  try { r = await api('/v1/settings/app', { method: 'POST', body: { name, value } }); } catch { G.busy = ''; G.err = 'Couldn’t reach Arr. Try again.'; bump(); return; }
  G.busy = '';
  if (!r.ok) { G.err = errMsg(r, 'That didn’t work.'); bump(); return; }
  if (!r.data.restarting) { G.msg = r.data.message; bump(); say(G.msg); return; }
  G.restarting = true; G.restartMsg = r.data.message; bump(); say('Restarting. Arr will be back in a minute.');
  waitForRestart();
}
// Arr ends itself and its service manager starts it again. Wait for it to go away and come back.
function waitForRestart() {
  const t0 = Date.now();
  let down = false;
  const tick = async () => {
    const d = await quietGet('/v1/update');
    if (!d) down = true;
    if (d && (down || Date.now() - t0 > 25000)) {
      G.restarting = false;
      await Promise.all([loadSettings(), loadHealth()]);
      G.msg = G.restartMsg; bump(); say(G.msg);
      return;
    }
    if (Date.now() - t0 > 5 * 60 * 1000) { G.restarting = false; G.err = 'Arr hasn’t come back yet. Give it a minute, then reload this page.'; bump(); return; }
    setTimeout(tick, 2000);
  };
  setTimeout(tick, 2500);
}
function SwitchRow({ id, title, meta, on, disabled, onToggle }) {
  return h('div', null, h('button', { type: 'button', role: 'switch', class: 'switch', id, 'aria-checked': String(on), 'aria-labelledby': id + '-t', 'aria-describedby': id + '-m', disabled, 'data-fk': id, onClick: onToggle },
    h('span', { class: 'what' }, h('span', { class: 'sw-t', id: id + '-t' }, title), h('span', { class: 'sw-m', id: id + '-m' }, meta)),
    h('span', { class: 'track', 'aria-hidden': 'true' }, h('i'))));
}
function FieldRow({ id, title, meta, value, saveLabel, type, inputmode, disabled, placeholder, onInput, onSave }) {
  return h('form', { class: 'item setting', onSubmit: (e) => { e.preventDefault(); onSave(); } },
    h('div', { class: 'what' },
      h('label', { class: 'sw-t', for: id }, title),
      h('span', { class: 'sw-m', id: id + '-m' }, meta),
      h('div', { class: 'field-row' },
        h('input', { id, class: 'filter', type, inputmode, value, placeholder, disabled, autocomplete: 'off', autocapitalize: 'off', spellcheck: false, 'aria-describedby': id + '-m', onInput: (e) => onInput(e.target.value) }),
        h('button', { type: 'submit', class: 'btn', disabled }, saveLabel))));
}
const HEALTH_WORD = { ok: 'OK', warn: 'Needs a look', bad: 'Problem' };
function HealthRow({ it }) {
  return h('li', { class: 'item health', 'data-level': it.level },
    h('span', { class: 'dot', 'aria-hidden': 'true' }, ic(it.level === 'ok' ? 'check' : 'alert')),
    h('div', { class: 'what' }, h('h3', null, it.label, h('span', { class: 'sr' }, ': ' + (HEALTH_WORD[it.level] || ''))), h('p', null, it.text)));
}
function when(iso) {
  const d = new Date(iso);
  if (Number.isNaN(d.getTime())) return '';
  return d.toLocaleString(undefined, { day: 'numeric', month: 'short', hour: 'numeric', minute: '2-digit' });
}
function HealthSection() {
  const hd = G.health;
  if (!hd) return h(Section, { id: 'health', title: 'Health' }, h(Hint, { text: 'Checking…' }));
  const probs = hd.problems || [];
  return h(Section, { id: 'health', title: 'Health', extra: h('button', { type: 'button', class: 'link', onClick: loadHealth }, 'Check again') },
    h('ul', { class: 'list' }, hd.items.map((it) => h(HealthRow, { key: it.id, it }))),
    h('details', { class: 'problems' },
      h('summary', null, probs.length ? `Recent problems (${probs.length})` : 'Recent problems'),
      probs.length
        ? h('ul', { class: 'plist' }, probs.map((p, n) => h('li', { key: n + p.at, 'data-level': p.level },
          h('span', { class: 'p-meta' }, when(p.at), ' · ', p.component, p.level === 'error' ? ' · error' : ''), h('span', { class: 'p-msg' }, p.message))))
        : h('p', { class: 'hint' }, 'Nothing has gone wrong lately.')));
}
function UpdatesSection() {
  const i = U.info;
  if (!i) return null;
  let line;
  if (!i.auto_updates) line = 'Automatic updates are off. To update, run the installer again on the computer that runs Arr.';
  else if (i.available && !i.can_update) line = `Arr ${i.latest} is available. To update from here, run the installer again on the computer that runs Arr.`;
  else if (i.available) line = `Arr ${i.latest} is available.`;
  else if (i.latest) line = 'Arr is up to date.';
  else line = 'Couldn’t check for a newer version just now.';
  return h(Section, { id: 'updates', title: 'Updates' },
    h('div', { class: 'item setting' }, h('div', { class: 'what' },
      h('span', { class: 'sw-t' }, `Arr ${i.current}`), h('span', { class: 'sw-m' }, line),
      i.available && i.can_update && U.phase === 'idle' ? h('button', { type: 'button', class: 'btn', onClick: startUpdate }, 'Update now') : null,
      i.available && i.release_url ? h('p', { class: 'sw-m' }, whatsNew(i.release_url)) : null)));
}
function Settings() {
  // "You" comes first and never waits on the settings: Sign out works while they load, fail or restart.
  const you = h(You);
  if (G.restarting) {
    return h('div', { class: 'stack' }, you,
      h('section', { class: 'empty', 'aria-labelledby': 'h-restart' }, h('img', { src: '/app/mascot.svg', alt: '' }),
        h('h2', { id: 'h-restart' }, 'Restarting…'), h('p', { role: 'status' }, 'Arr will be back in a minute.')));
  }
  const d = G.data;
  if (!d) return h('div', { class: 'stack' }, you, h(Hint, { text: 'Loading settings…' }));
  const lock = !d.can_restart || !!G.busy;
  const busy = (n) => G.busy === n;
  return h('div', { class: 'stack' }, you,
    !d.can_restart ? h('p', { class: 'hint', role: 'note' }, 'Arr isn’t running as a service on its computer, so it can’t restart itself. Change these there with “arr set”.') : null,
    G.err ? h('p', { class: 'msg', role: 'alert', 'data-err': '' }, G.err) : null,
    G.msg ? h('p', { class: 'msg', role: 'status' }, G.msg) : null,
    h(Section, { id: 'dl', title: 'Downloads' }, h('div', { class: 'rows' },
      h(SwitchRow, { id: 'set-dl', title: 'Downloading', on: d.downloading, disabled: lock,
        meta: d.downloading ? 'On. Arr gets what you ask for and adds it to your library.' : 'Off. Arr keeps your list, but downloads nothing and adds nothing to your library.',
        onToggle: () => applySetting('downloading', d.downloading ? 'off' : 'on') }),
      d.embedded ? h(Fragment, null,
        h(FieldRow, { id: 'set-up', title: 'Upload limit', meta: 'KiB per second. Leave it blank for no limit.', value: G.draft.upload, type: 'text', inputmode: 'numeric', placeholder: 'No limit', disabled: lock,
          saveLabel: busy('upload-limit-kib') ? 'Saving…' : 'Save', onInput: (v) => { G.draft.upload = v; bump(); }, onSave: () => applySetting('upload-limit-kib', G.draft.upload.trim()) }),
        h(FieldRow, { id: 'set-port', title: 'Torrent port', meta: 'The port Arr listens on for other people sharing the same file.', value: G.draft.port, type: 'text', inputmode: 'numeric', disabled: lock,
          saveLabel: busy('torrent-port') ? 'Saving…' : 'Save', onInput: (v) => { G.draft.port = v; bump(); }, onSave: () => applySetting('torrent-port', G.draft.port.trim()) })) : null)),
    h(Section, { id: 'privacy', title: 'Privacy' }, h('div', { class: 'rows' },
      h(SwitchRow, { id: 'set-reports', title: 'Send error reports', on: d.error_reports, disabled: lock || !d.error_reports_available,
        meta: !d.error_reports_available ? 'Off. This copy of Arr has nowhere to send reports.'
          : d.error_reports ? 'On. When something breaks, Arr sends its developer a short note with names, files, addresses and keys taken out.' : 'Off. Nothing is sent. Turn it on to send its developer a short note, with names, files, addresses and keys taken out, when something breaks.',
        onToggle: () => applySetting('error-reports', d.error_reports ? 'off' : 'on') }),
      h('p', { class: 'hint' }, h('a', { href: PRIVACY, target: '_blank', rel: 'noopener noreferrer' }, 'What is and isn’t sent')))),
    h(Section, { id: 'tmdb', title: 'Movie details' },
      h(FieldRow, { id: 'set-tmdb', title: 'TMDB key', meta: d.tmdb_key_set ? 'Set. Paste a new key to replace it.' : 'Not set. Paste a key to add TMDB details and posters.', value: G.draft.key, type: 'password', disabled: lock,
        placeholder: d.tmdb_key_set ? '••••••••' : 'Paste your key', saveLabel: busy('tmdb-key') ? 'Checking…' : d.tmdb_key_set ? 'Replace key' : 'Save key',
        onInput: (v) => { G.draft.key = v; bump(); }, onSave: () => { if (G.draft.key.trim()) applySetting('tmdb-key', G.draft.key.trim()); } })),
    d.can_restart ? h('p', { class: 'hint' }, 'Changing a setting restarts Arr. It’s back in about a minute.') : null,
    h(BinSection),
    h(UpdatesSection),
    h(HealthSection));
}

// Who is signed in, Arr on a phone, notifications and Sign out: the top of Settings.
function You() {
  const [open, setOpen] = useState(false);
  const shown = useRef(false);
  useEffect(() => {
    if (shown.current && open) $('#phone').scrollIntoView({ block: 'nearest', behavior: 'smooth' });
    shown.current = true;
  }, [open]);
  const me = S.me || {};
  return h(Section, { id: 'you', title: 'You' }, h('div', { class: 'rows' },
    h('div', { class: 'item setting' }, h('div', { class: 'what' },
      h('span', { class: 'sw-t', id: 'who' }, me.email ? 'Signed in as ' + me.email : 'Signed in'),
      h('button', { type: 'button', class: 'link', id: 'phone-btn', 'aria-expanded': String(open), 'aria-controls': 'phone', onClick: () => setOpen(!open) }, 'Use Arr on your phone'),
      h('section', { class: 'phone', id: 'phone', 'aria-label': 'Use Arr on your phone', hidden: !open }, open ? h(Phone, { url: me.public_url || null }) : null))),
    h('div', { class: 'item setting' }, h(Notify)),
    h('div', { class: 'item setting' }, h('div', { class: 'what' },
      h('button', { type: 'button', class: 'btn ghost', id: 'signout', onClick: signOut }, 'Sign out')))));
}
function Footer() {
  return h('footer', { class: 'foot', id: 'foot', hidden: !S.me || S.out, inert: P.shown },
    h('p', { class: 'credit' }, 'Movie details and posters from Plex',
      h('span', { id: 'tmdb-credit', hidden: !S.tmdb }, ' and ', h('img', { src: '/app/tmdb.svg', alt: 'TMDB', width: 85, height: 11 }), ' (Arr uses TMDB and the TMDB APIs but is not endorsed, certified, or otherwise approved by TMDB)'),
      ', show details and posters from ', h('a', { href: 'https://www.tvmaze.com', target: '_blank', rel: 'noopener noreferrer' }, 'TVmaze'), '.'));
}
async function signOut() {
  authGen++; dropFeed();
  try { await api('/signout', { method: 'POST' }); } catch { /* signed out anyway */ }
  location.hash = ''; location.reload();
}

const ENTER = new Set(['home', 'ideas', 'search', 'person', 'start']);
function stagger(els) {
  els.slice(0, 10).forEach((e, i) => anim(e, [{ opacity: 0, transform: 'translateY(14px) scale(.98)' }, { opacity: 1, transform: 'none' }], { duration: 420, delay: i * 60, fill: 'backwards' }));
}
const enterRows = () => stagger([...document.querySelectorAll('#view .item, #view .idea, #view .recent li')]);
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
    // A screen's rows come in one after another, the first time they are drawn and never again.
    const sn = snap; snap = null;
    if (M.celebrate.size) runCelebrations();
    if (M.fresh && ENTER.has(V.t)) { M.fresh = false; enterRows(); } else if (sn) playFlip(sn);
    for (const fn of later.splice(0)) fn();
  });
  const tabAt = TABS.findIndex((t) => t[0] === R.path);
  if (tabAt >= 0) M.tab = tabAt;
  const inert = P.shown; // everything behind the sheet: out of reach of the keyboard and assistive tech
  return h(Fragment, null,
    h('a', { class: 'skip', href: '#view', inert }, 'Skip to content'),
    h('header', { class: 'top', id: 'top', inert },
      h('a', { class: 'brand', href: '#/', 'aria-label': 'Arr, home' }, h('img', { src: '/app/mascot.svg', alt: '', width: 34, height: 34 }), h('span', null, 'Arr')),
      h('nav', { id: 'tabs', 'aria-label': 'Main', hidden: S.out, style: { '--i': M.tab } },
        h('span', { class: 'pill', 'aria-hidden': 'true', 'data-off': tabAt < 0 ? '' : null }),
        TABS.map(([p, l, i]) =>
        h('a', { key: p, href: '#' + p, 'aria-current': R.path === p ? 'page' : null }, ic(i), l))),
      h('form', { class: 'search', id: 'search', role: 'search', autocomplete: 'off', hidden: S.out, onSubmit: searchSubmit },
        h('span', { id: 'search-ico', 'aria-hidden': 'true' }, ic('search')),
        h('label', { class: 'sr', for: 'q' }, 'Find a movie or show'),
        h('input', { id: 'q', name: 'q', type: 'search', placeholder: 'Find a movie or show', enterkeyhint: 'search', autocapitalize: 'off', spellcheck: false, onInput: searchInput, onFocus: openSearch, onClick: openSearch })),
      h('div', { id: 'banner', class: 'banner', role: 'status' }, S.banner || (S.setup ? SETUP_MSG : null))),
    h(UpdateBar),
    h(Topline, { on: !!(S.cached || P.upd || S.libUpd) }),
    h('main', { id: 'view', tabindex: '-1', inert }, h(View)),
    h(Toast),
    h(Footer),
    h(Sheet),
    h('div', { id: 'live', class: 'sr', 'aria-live': 'polite' }, S.live || null));
}

function searchSubmit(e) {
  e.preventDefault();
  const q = $('#q').value.trim();
  if (q.length >= 2) { clearTimeout(debounceT); noteSearch(q); go(q); }
}
// Tapping the search field opens Search, which shows recent searches and popular titles.
function openSearch() {
  if (!S.out && !P.shown && parseHash().path !== '/search') location.hash = '#/search';
}
function searchInput() {
  clearTimeout(debounceT);
  const q = $('#q').value.trim();
  debounceT = setTimeout(() => {
    if (q.length >= 2) go(q);
    else if (!q && parseHash().path === '/search') { history.replaceState(null, '', '#/search'); bgHash = '#/search'; renderSearch('', ++token); }
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
    // The server remembers its release check for six hours; asking now and then is cheap.
    if (!document.hidden && S.me && U.phase === 'idle' && Date.now() - U.checkedAt > 30 * 60 * 1000) loadUpdate();
  });
  window.addEventListener('hashchange', route);
  // Who is signed in, the feed and the ideas are asked together; none waits for another.
  const path = parseHash().path;
  if (path === '/' || path === '/ideas' || path === '/search') loadIdeas();
  if (path === '/') loadFeed();
  try {
    const r = await api('/me');
    if (r.ok) { S.me = r.data; bump(); }
  } catch {
    if (S.out) return;
    setView(hint('Your list will show up here as soon as Arr is back.'));
    setTimeout(boot2, 5000);
    return;
  }
  start();
}
// Signed in: show what was seen last time (only ever the same person's) while the feed is fetched.
function start() {
  if (S.feed) saveFeed();
  else if (onHome()) { const f = savedFeed(); if (f) { S.feed = f; S.cached = true; } }
  route();
  // The feed also says whether TMDB is used, which the footer credit needs on every screen.
  if (!onHome()) loadFeed();
  if (!U.checkedAt) loadUpdate();
}
async function boot2() {
  try {
    const r = await api('/me');
    if (r.ok) { S.me = r.data; bump(); start(); }
  } catch { if (!S.out) setTimeout(boot2, 5000); }
}
document.body.replaceChildren();
render(h(App), document.body);
boot();
