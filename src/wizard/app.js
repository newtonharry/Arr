"use strict";

const $ = (id) => document.getElementById(id);
let state = null;
// The library lists the form was last filled from.
let filled = null;
let linking = false;

const GONE = "Arr's setup has stopped, so this page can't go on. Look at the terminal window to see why, or run the installer again.";
// Requests in a row that nothing answered. Any answer, even an error, resets it.
let unanswered = 0;

async function call(method, path, body) {
  const opts = { method, headers: { "x-arr-setup": "1" } };
  if (body !== undefined) {
    opts.headers["content-type"] = "application/json";
    opts.body = JSON.stringify(body);
  }
  let resp;
  try {
    resp = await fetch(path, opts);
    unanswered = 0;
  } catch (_) {
    // Nothing answered: the installer, and this page's server with it, may have stopped.
    unanswered++;
    const e = new Error(GONE);
    e.gone = true;
    throw e;
  }
  let data = null;
  try { data = await resp.json(); } catch (_) { /* not JSON */ }
  if (!resp.ok) throw new Error((data && data.error) || `Something went wrong (${resp.status})`);
  return data;
}

// Shows an error unless it's already showing, so a later render doesn't scroll to it again.
function showErrorOnce(msg) {
  if ($("error").textContent !== msg) showError(msg);
}

// One pair of quotes around a pasted path (Windows' "Copy as path" adds them) isn't part of it.
function unquote(raw) {
  const p = raw.trim();
  const q = p[0];
  return p.length >= 2 && (q === '"' || q === "'") && p[p.length - 1] === q ? p.slice(1, -1).trim() : p;
}

function showError(msg) {
  const el = $("error");
  el.textContent = msg || "";
  el.hidden = !msg;
  if (msg) el.scrollIntoView({ behavior: "smooth", block: "center" });
}

function render(s) {
  state = s;
  const phase = s.phase.state;
  const busy = phase === "working" || phase === "done";
  $("form").hidden = busy;
  $("working").hidden = phase !== "working";
  $("done").hidden = phase !== "done";
  if (phase === "working") {
    $("working-step").textContent = s.phase.step;
    $("working-detail").textContent = s.phase.detail || "";
    return;
  }
  if (phase === "done") {
    showDone(s);
    return;
  }
  // The form is back, and Finish starts again where it stopped (the import resumes).
  if (phase === "failed") showErrorOnce(`${s.phase.message}\n\nClick Finish setup to try again.`);

  $("signin").hidden = s.signed_in;
  $("signed-in").hidden = !s.signed_in;
  $("signin-wait").hidden = !s.waiting_for_signin;

  $("step-server").hidden = !s.signed_in;
  renderServer(s);

  $("reporting-box").hidden = !s.reporting.available;
  $("reporting").checked = s.reporting.enabled;
  const ready = !!s.plex;
  const noLibraries = ready && s.plex.movies.length === 0 && s.plex.tv.length === 0;
  $("step-libraries").hidden = !ready;
  $("step-options").hidden = !ready || noLibraries;
  $("install-hint").textContent = INSTALL_HINT[s.platform] || INSTALL_HINT.linux;
  // Refilled when Plex's libraries change (added in Plex, then Look again).
  const lists = ready ? JSON.stringify([s.plex.movies, s.plex.tv]) : null;
  if (ready && lists !== filled) { fillLibraries(s.plex); filled = lists; }
}

const MISSING = {
  not_installed: [
    "Arr couldn't find Plex running on this computer. Arr puts movies and shows where Plex looks, so it has to run on the computer that runs Plex. If you already have Plex, open it, then click Look again. If not, ",
    "install Plex Media Server", "https://www.plex.tv/media-server-downloads/",
    ", open it, then click Look again.",
  ],
  not_linked: [
    "Plex is on this computer, but it isn't linked to the Plex account you signed in with. ",
    // The same computer this page comes from, even when it's opened from another one.
    "Open Plex", `http://${location.hostname}:32400/web`,
    " on this computer and sign in with the same account, then look again.",
  ],
  unclaimed: [
    "Plex is installed but isn't linked to your Plex account yet. Click Look again to link it.",
    "", "", "",
  ],
  unreadable: [
    "Arr found Plex on this computer but couldn't read its libraries. If Plex is still starting, wait a minute, then look again.",
    "", "", "",
  ],
};

// What happens when Arr installs Plex, on the computer this page's server runs on.
const INSTALL_HINT = {
  linux: "Your computer will ask for your password, as it does when you install any app.",
  windows: "Arr downloads Plex first. Then Windows asks if the Plex installer may make changes to this computer: click Yes.",
  macos: "Arr downloads Plex and puts it in your Applications folder. It doesn't need your password.",
};

function renderServer(s) {
  $("connected").hidden = !s.plex;
  if (s.plex) $("connected").textContent = `Found ${s.plex.name} on this computer.`;
  const install = s.install.state;
  // No Plex here (or it's being installed): offer to install it, where there's a way to.
  const busy = install === "running" || linking;
  const canOffer = s.can_install || !!s.install_command;
  // A failed install counts only while the latest look still finds no Plex: one found since
  // (linked to another account, say) shows its own way on.
  const notInstalled = s.missing === "not_installed";
  const installing = !s.plex && s.signed_in && (busy || (install === "failed" && notInstalled) ||
    (canOffer && notInstalled && install !== "done"));
  $("install").hidden = !installing;
  if (installing) {
    $("missing").hidden = true;
    const failed = install === "failed" && !busy;
    $("install-intro").hidden = busy;
    $("install-progress").hidden = !busy;
    $("install-step").textContent = linking ? "Linking Plex to your account…" : (s.install.step || "");
    $("install-ask").hidden = busy || !s.can_install;
    // The line to paste is shown when the page can't install Plex, or couldn't.
    $("install-manual").hidden = busy || !s.install_command || (s.can_install && !failed);
    $("install-cmd").textContent = s.install_command || "";
    $("install-download").hidden = !failed || !!s.install_command;
    // Plex may be installed but not running, or be installed by hand: always look again.
    $("install-look").hidden = busy;
    if (failed) showErrorOnce(s.install.message);
    return;
  }
  // Signed in but not connected: always offer to look again, even when the last look
  // failed before it could say why (plex.tv unreachable, say).
  $("missing").hidden = !s.signed_in || !!s.plex;
  if (s.plex || !s.signed_in) return;
  $("switch-account").hidden = s.missing !== "not_linked";
  const [before, label, href, after] = MISSING[s.missing] ||
    ["Arr couldn't check your Plex account. Check this computer's internet connection, then look again.", "", "", ""];
  const parts = [before];
  if (label) {
    const a = document.createElement("a");
    a.href = href; a.target = "_blank"; a.rel = "noopener noreferrer"; a.textContent = label;
    parts.push(a, after);
  }
  $("missing-text").replaceChildren(...parts);
}

function fillLibraries(plex) {
  const none = plex.movies.length === 0 && plex.tv.length === 0;
  const noMovies = plex.movies.length === 0, noTv = plex.tv.length === 0;
  // Arr can create the missing kind of library, except in a Plex that runs in Docker.
  $("new-libraries").hidden = !(noMovies || noTv) || plex.docker;
  $("libs-docker").hidden = !(noMovies || noTv) || !plex.docker;
  const which = none ? "a Movies or TV Shows library" : noMovies ? "a Movies library" : "a TV Shows library";
  $("libs-docker").textContent = `Plex runs in Docker on this computer, so Arr can't create libraries in it. Add ${which} in Plex, then click Look again.`;
  $("new-libraries-text").textContent = none
    ? "Plex has no movie or TV library yet. Arr will create them in these folders:"
    : `Plex has no ${noMovies ? "movie" : "TV"} library yet. Arr can create one in this folder:`;
  $("create-libs").textContent = none ? "Create my libraries"
    : noMovies ? "Create the Movies library" : "Create the TV Shows library";
  $("new-movies-box").hidden = !noMovies;
  $("new-tv-box").hidden = !noTv;
  if (!$("new-movies").value) $("new-movies").value = state.media_movies;
  if (!$("new-tv").value) $("new-tv").value = state.media_tv;
  $("libraries-intro").hidden = none;
  for (const el of document.querySelectorAll(".library")) el.hidden = plex[el.dataset.kind].length === 0;
  $("step-options").hidden = none;
  $("apply").disabled = none;
  if (none) {
    // Nothing to choose from: forget the old lists, so libraries made next start fresh.
    for (const kind of ["movies", "tv"]) $(`${kind}-lib`).replaceChildren();
    return;
  }
  for (const kind of ["movies", "tv"]) {
    const sel = $(`${kind}-lib`);
    const before = sel.options.length ? sel.value : null;
    // Whether the list offered a library, and where the chosen one was, before this refill.
    const offered = [...sel.options].some((o) => o.value);
    const where = sel.selectedOptions.length ? sel.selectedOptions[0].dataset.where : undefined;
    sel.replaceChildren();
    for (const lib of plex[kind]) {
      const o = document.createElement("option");
      o.value = lib.id;
      o.dataset.where = JSON.stringify([lib.plex_path, lib.local_path]);
      o.textContent = plex[kind].filter((l) => l.title === lib.title).length > 1
        ? `${lib.title} (${lib.plex_path})` : lib.title;
      sel.append(o);
    }
    const skip = document.createElement("option");
    skip.value = "";
    skip.textContent = kind === "movies" ? "Don't add movies" : "Don't add TV shows";
    sel.append(skip);
    // Keep what was chosen (and its folder) while it's still in the list where it was. A
    // library moved in Plex gets its new folder; "Don't add" stays only if it was chosen over
    // a library, so a library just created gets chosen.
    const match = before === null ? null : [...sel.options].find((o) => o.value === before);
    const keep = match && (before ? match.dataset.where === where : offered);
    if (match && (before || offered)) sel.value = before;
    if (!keep) libraryChanged(kind);
  }
  // A library found just now may sit where the suggested download folder is.
  suggestDownloads();
}

const DOWNLOADS_HINT = $("downloads-hint").textContent;

for (const kind of ["movies", "tv"]) {
  $(`${kind}-lib`).addEventListener("change", () => libraryChanged(kind));
  $(`${kind}-folder`).addEventListener("change", () => {
    checkFolder(`${kind}-folder`, `${kind}-hint`, false);
    suggestDownloads();
  });
}
// Once the person types in the download folder, Arr stops suggesting one.
$("downloads").addEventListener("input", () => { $("downloads").dataset.edited = "1"; });
$("downloads").addEventListener("change", () => checkFolder("downloads", "downloads-hint", true));
for (const id of ["new-movies", "new-tv"]) {
  $(id).addEventListener("change", () => { $(id).value = unquote($(id).value); });
}

function chosenLibrary(kind) {
  return state.plex[kind].find((l) => l.id === $(`${kind}-lib`).value);
}

// What the hint under a library's folder says until the folder is checked.
function libraryHint(lib) {
  if (!lib) return "";
  return lib.local_path
    ? (lib.local_path === lib.plex_path ? "" : `Plex sees this folder as ${lib.plex_path}.`)
    : `Plex keeps this library in ${lib.plex_path}. Enter where that folder is on this computer.`;
}

function libraryChanged(kind) {
  const lib = chosenLibrary(kind);
  const folder = $(`${kind}-folder`);
  const hint = $(`${kind}-hint`);
  folder.closest(".folder").hidden = !lib;
  hint.classList.remove("bad", "was-bad");
  hint.textContent = libraryHint(lib);
  if (lib) {
    folder.value = lib.local_path || "";
    if (lib.local_path) checkFolder(`${kind}-folder`, `${kind}-hint`, false);
  }
  suggestDownloads();
}

// A path without its trailing separators, for comparing: "D:\Media\" -> "d:\media".
function pathKey(p) {
  const t = p.replace(/[\\/]+$/, "");
  return /^([A-Za-z]:|\\\\)/.test(p) ? t.replace(/\//g, "\\").toLowerCase() : t;
}

// Whether `path` is `folder` or inside it. A root ("/", "D:\") holds everything on it.
function inside(path, folder) {
  const a = pathKey(path), b = pathKey(folder);
  const sep = /^([A-Za-z]:|\\\\)/.test(folder) ? "\\" : "/";
  return a === b || a.startsWith(b + sep);
}

// A download folder beside the first library folder that has room beside it:
// /srv/media/Movies -> /srv/media/arr-downloads, D:\Media\Movies -> D:\Media\arr-downloads.
// Never inside a library (one at D:\ has no folder beside it), and never beside a network
// share (\\NAS\Movies), since \\NAS\arr-downloads isn't a folder. Until the person types
// their own, it follows the library folders.
function suggestDownloads() {
  const dl = $("downloads");
  if (dl.dataset.edited) return;
  const folders = ["movies", "tv"].map((k) => $(`${k}-lib`).value && unquote($(`${k}-folder`).value)).filter(Boolean);
  // Every library folder the page knows of, chosen or not: the download folder must neither
  // be in one nor hold one.
  const known = state && state.plex ? [...state.plex.movies, ...state.plex.tv] : [];
  const libraries = folders.concat(known.map((l) => l.local_path).filter(Boolean));
  let suggestion = "";
  for (const f of folders) {
    const sep = /^([A-Za-z]:)?\\/.test(f) ? "\\" : "/";
    const trimmed = f.replace(/[\\/]+$/, "");
    const cut = Math.max(trimmed.lastIndexOf("/"), trimmed.lastIndexOf("\\"));
    if (cut < 0) continue;
    const parent = trimmed.slice(0, cut);
    if (/^\\\\[^\\]*$/.test(parent) || parent === "\\") continue;
    const candidate = `${parent}${sep}arr-downloads`;
    if (libraries.some((lib) => inside(candidate, lib) || inside(lib, candidate))) continue;
    suggestion = candidate;
    break;
  }
  if (suggestion === dl.value) return;
  dl.value = suggestion;
  checkFolder("downloads", "downloads-hint", true);
}

async function checkFolder(inputId, hintId, create) {
  const input = $(inputId), hint = $(hintId);
  const path = unquote(input.value);
  if (input.value !== path) input.value = path;
  if (!path) {
    // Nothing to check: the hint goes back to what it said before.
    hint.classList.remove("bad", "was-bad");
    hint.textContent = inputId === "downloads" ? DOWNLOADS_HINT
      : libraryHint(chosenLibrary(inputId.replace("-folder", "")));
    return;
  }
  try {
    const r = await call("POST", "/api/folder", { path, create });
    // Changed while it was being checked: the newer check has the say.
    if (unquote(input.value) !== path) return;
    hint.classList.toggle("bad", !r.ok);
    if (!r.ok) hint.textContent = r.message;
    else if (hint.classList.contains("was-bad") || !hint.textContent) hint.textContent = "✓ Arr can use this folder.";
    hint.classList.toggle("was-bad", !r.ok);
  } catch (e) { showError(e.message); }
}

async function act(button, fn) {
  showError("");
  if (button) button.disabled = true;
  try {
    render(await fn());
  } catch (e) {
    showError(e.message);
  } finally {
    if (button) button.disabled = false;
  }
}

let pollId = 0;

// Calls step every `ms` until it returns false. Each call waits for the one before, and
// starting another poll ends this one. When nothing answers a few times in a row, the
// installer has stopped (its window closed, say): the page says so and stops asking.
function poll(step, ms) {
  const id = ++pollId;
  const tick = async () => {
    if (id !== pollId) return;
    let more = true;
    try {
      more = await step();
    } catch (e) {
      if (e.gone && unanswered >= 4) {
        showError(GONE);
        more = false;
      }
    }
    if (more !== false && id === pollId) setTimeout(tick, ms);
  };
  setTimeout(tick, ms);
}

// Until signed in, or the sign-in expires (the server then forgets it): a failed look,
// plex.tv having a bad moment, doesn't end it.
function startSigninPolling() {
  poll(async () => {
    let s;
    try {
      s = await call("POST", "/api/plex/check");
      showError("");
    } catch (e) {
      // Nothing answered: poll says so after a few times, and asking again now won't help.
      if (e.gone) throw e;
      showError(e.message);
      s = await call("GET", "/api/state");
    }
    render(s);
    return !s.signed_in && s.waiting_for_signin;
  }, 2000);
}

function startProgressPolling() {
  poll(async () => {
    const s = await call("GET", "/api/state");
    render(s);
    return s.phase.state === "working";
  }, 1000);
}

let signingIn = false;

// A new sign-in starts over, so it can be with a different account.
async function startSignin() {
  // A second click while the first is under way would open a second tab.
  if (signingIn) return;
  signingIn = true;
  $("signin-btn").disabled = $("switch-account").disabled = true;
  showError("");
  // Open the tab now, while the click still counts: browsers block pop-ups opened later.
  const tab = window.open("about:blank", "_blank");
  try {
    const r = await call("POST", "/api/plex/signin");
    filled = null;
    for (const kind of ["movies", "tv"]) $(`${kind}-lib`).replaceChildren();
    $("signin-link").href = r.url;
    if (tab) tab.location.href = r.url; else window.open(r.url, "_blank");
    render(await call("GET", "/api/state"));
    startSigninPolling();
  } catch (e) {
    if (tab) tab.close();
    showError(e.message);
  } finally {
    signingIn = false;
    $("signin-btn").disabled = $("switch-account").disabled = false;
  }
}

$("signin-btn").addEventListener("click", startSignin);
$("switch-account").addEventListener("click", startSignin);

// Reports start as soon as the box is ticked, so a problem later in setup is covered too.
// An error already showing (a failed Finish, say) stays as it is.
$("reporting").addEventListener("change", async (ev) => {
  try {
    const r = (await call("POST", "/api/reporting", { enabled: ev.target.checked })).reporting;
    if (state) state.reporting = r;
    $("reporting-box").hidden = !r.available;
    $("reporting").checked = r.enabled;
  } catch (e) { showError(e.message); }
});

for (const id of ["look-again", "install-look", "libs-look"]) {
  $(id).addEventListener("click", (ev) => act(ev.target, () => call("POST", "/api/plex/check")));
}

// Install Plex, then link it to the account (the check does that), showing each step.
$("install-btn").addEventListener("click", async (ev) => {
  await act(ev.target, () => call("POST", "/api/plex/install"));
  if (state && state.install.state === "running") followInstall();
});

// Shows the install's steps until it ends, then links the new Plex (the check does that).
function followInstall() {
  poll(async () => {
    const s = await call("GET", "/api/state");
    if (s.install.state === "running") { render(s); return true; }
    if (s.install.state === "done") await linkPlex(s); else render(s);
    return false;
  }, 1000);
}

async function linkPlex(s) {
  linking = true;
  render(s);
  try { render(await call("POST", "/api/plex/check")); }
  catch (e) { showError(e.message); }
  finally { linking = false; render(state); }
}

// Only the kinds Plex doesn't have yet (Arr never makes a second library of a kind).
$("create-libs").addEventListener("click", async (ev) => {
  const folder = (kind) => state.plex[kind].length ? null : unquote($(`new-${kind}`).value);
  const body = { movies: folder("movies"), tv: folder("tv"), language: navigator.language || "" };
  await act(ev.target, () => call("POST", "/api/plex/libraries", body));
  const warnings = (state && state.warnings) || [];
  $("libs-warning").textContent = warnings.join(" ");
  $("libs-warning").hidden = warnings.length === 0;
});

$("form").addEventListener("submit", async (ev) => {
  ev.preventDefault();
  const lib = (kind) => {
    const id = $(`${kind}-lib`).value;
    return id ? { id, folder: unquote($(`${kind}-folder`).value) } : null;
  };
  const body = {
    movies: lib("movies"),
    tv: lib("tv"),
    downloads: unquote($("downloads").value),
    auto_update: $("auto-update").checked,
    reporting: $("reporting").checked,
  };
  startProgressPolling();
  await act($("apply"), () => call("POST", "/api/apply", body));
  if (state && state.phase.state === "working") startProgressPolling();
});

call("GET", "/api/state").then((s) => {
  render(s);
  if (s.waiting_for_signin) startSigninPolling();
  // Reloaded during or just after installing Plex: pick up where it was.
  if (s.install.state === "running") followInstall();
  else if (s.install.state === "done" && !s.plex && s.signed_in) linkPlex(s);
  // Signed in but no Plex yet: look again now, so a Plex opened since is found on reload.
  else if (s.signed_in && !s.plex && s.phase.state !== "working" && s.phase.state !== "done") {
    act(null, () => call("POST", "/api/plex/check"));
  }
  if (s.phase.state === "working") startProgressPolling();
}).catch((e) => showError(e.message));

// ---- The Done screen: where Arr is, and a button that opens it already signed in.

let doneShown = false;
let appLink = null; // in memory only: it's used by one click and never put in the page

function showDone(s) {
  if (doneShown) return;
  doneShown = true;
  $("app-local").textContent = s.app.url;
  const remote = !!s.app.public_url;
  $("remote-yes").hidden = !remote;
  $("remote-no").hidden = remote;
  if (remote) {
    $("app-public").textContent = s.app.public_url;
    $("app-qr").replaceChildren(qrSvg(s.app.public_url));
  }
  // The Windows installer takes no options: it reads ARR_REMOTE instead.
  if (s.platform === "windows") {
    $("remote-how").textContent = "run the install line again after";
    $("remote-cmd").textContent = "$env:ARR_REMOTE = '1'";
  }
  const mac = /Mac|iPhone|iPad/.test(navigator.platform || navigator.userAgent);
  $("bookmark-hint").textContent = `Once Arr is open, bookmark it: press ${mac ? "⌘D" : "Ctrl+D"}. On a phone: Share → Add to Home Screen.`;
  // Arr runs on this port once the installer starts it, so this page can ask it, as itself.
  const here = ["localhost", "127.0.0.1", "[::1]"].includes(location.hostname);
  $("open-elsewhere").hidden = here;
  $("open-box").hidden = !here;
  if (here) prepareOpen();
  else call("POST", "/api/finish").catch(() => {});
}

// Makes the sign-in link while this setup page is still running, lets it go, then waits for
// the app to answer on the same address before enabling the button.
async function prepareOpen() {
  try { appLink = (await call("POST", "/api/app-link")).url; } catch (_) { appLink = null; }
  call("POST", "/api/finish").catch(() => {});
  const status = $("open-status");
  if (!appLink) {
    $("open-box").hidden = true;
    return;
  }
  const started = Date.now();
  const look = async () => {
    let up = false;
    try {
      const r = await fetch("/api/v1/health", { cache: "no-store" });
      up = r.ok;
    } catch (_) { /* the setup page has gone and Arr isn't up yet */ }
    if (up) {
      status.textContent = "Arr is running.";
      $("open-arr").disabled = false;
      return;
    }
    if (Date.now() - started > 90000) status.textContent = "Arr hasn't started yet. Look at the terminal window: the installer may be waiting for an answer.";
    setTimeout(look, 1000);
  };
  look();
}

// Opens Arr signed in. The link is spent here, by a POST to the app (as its own page, so
// the browser stores the session for this address), and the new tab then goes to the app's
// front page: no token ever sits in a tab's address or history.
$("open-arr").addEventListener("click", async () => {
  if (!appLink) return;
  const button = $("open-arr"), status = $("open-status");
  // Opened now, while the click still counts, so the browser doesn't block it.
  const tab = window.open("", "_blank");
  if (tab) { try { tab.opener = null; } catch (_) { /* fine */ } }
  const link = appLink;
  const token = new URL(link).searchParams.get("t");
  appLink = null;
  button.disabled = true;
  status.textContent = "Signing in…";
  let ok = false;
  let answered = false;
  try {
    const r = await fetch("/app/link", {
      method: "POST",
      redirect: "manual",
      headers: { "content-type": "application/x-www-form-urlencoded" },
      body: new URLSearchParams({ t: token }),
    });
    answered = true;
    ok = r.type === "opaqueredirect" || r.status === 303;
  } catch (_) { /* Arr isn't answering: restarting, say */ }
  if (ok) {
    if (tab) tab.location.href = `${location.origin}/`;
    else window.open(`${location.origin}/`, "_blank");
    status.textContent = "Opened in a new tab.";
    return;
  }
  if (tab) tab.close();
  // Nothing answered, so the link wasn't used: keep it for the next click.
  if (!answered) {
    appLink = link;
    status.textContent = "Arr didn't answer. Wait a moment, then click Open Arr again.";
    button.disabled = false;
    return;
  }
  // Expired or already used: a new one, if this page can still ask for it.
  try {
    appLink = (await call("POST", "/api/app-link")).url;
    status.textContent = "That sign-in didn't work. Click Open Arr to try again.";
    button.disabled = false;
  } catch (_) {
    status.textContent = `That sign-in link has expired. Look in the terminal window for the sign-in link the installer printed when it finished, and open it on this computer. Arr is at ${location.origin}.`;
  }
});

for (const b of document.querySelectorAll("button.copy")) {
  b.addEventListener("click", async () => {
    const node = $(b.dataset.from), note = b.nextElementSibling;
    let ok = false;
    try { await navigator.clipboard.writeText(node.textContent); ok = true; } catch (_) { /* select it instead */ }
    if (!ok) {
      const range = document.createRange();
      range.selectNodeContents(node);
      const sel = getSelection(); sel.removeAllRanges(); sel.addRange(range);
      try { ok = document.execCommand("copy"); } catch (_) { /* by hand */ }
    }
    note.textContent = ok ? "Copied" : "Press Ctrl+C to copy";
  });
}

// The code is always dark on white, so a phone can scan it in either theme.
function qrSvg(text) {
  const NS = "http://www.w3.org/2000/svg", pad = 3;
  const q = qrcodegen.QrCode.encodeText(text, qrcodegen.QrCode.Ecc.MEDIUM);
  const svg = document.createElementNS(NS, "svg"), bg = document.createElementNS(NS, "rect"), path = document.createElementNS(NS, "path");
  let d = "";
  for (let y = 0; y < q.size; y++) for (let x = 0; x < q.size; x++) if (q.getModule(x, y)) d += `M${x + pad},${y + pad}h1v1h-1z`;
  svg.setAttribute("viewBox", `0 0 ${q.size + pad * 2} ${q.size + pad * 2}`);
  svg.setAttribute("role", "img");
  svg.setAttribute("aria-label", `QR code for ${text}`);
  svg.setAttribute("shape-rendering", "crispEdges");
  bg.setAttribute("width", "100%"); bg.setAttribute("height", "100%"); bg.setAttribute("fill", "#fff");
  path.setAttribute("d", d); path.setAttribute("fill", "#000");
  svg.append(bg, path);
  return svg;
}
