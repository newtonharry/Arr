"use strict";

const $ = (id) => document.getElementById(id);
let state = null;
let polling = null;
let formFilled = false;
let listening = false;
let linking = false;

async function call(method, path, body) {
  const opts = { method, headers: { "x-arr-setup": "1" } };
  if (body !== undefined) {
    opts.headers["content-type"] = "application/json";
    opts.body = JSON.stringify(body);
  }
  const resp = await fetch(path, opts);
  let data = null;
  try { data = await resp.json(); } catch (_) { /* not JSON */ }
  if (!resp.ok) throw new Error((data && data.error) || `Something went wrong (${resp.status})`);
  return data;
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
    $("done-downloading").textContent = s.phase.downloading
      ? "Ask for a movie or show and Arr will download it into Plex."
      : "Downloading is off for now. Run the installer again or edit Arr's config to turn it on.";
    return;
  }
  if (phase === "failed") showError(s.phase.message);

  $("signin").hidden = s.signed_in;
  $("signed-in").hidden = !s.signed_in;
  $("signin-wait").hidden = !s.waiting_for_signin;

  $("step-server").hidden = !s.signed_in;
  renderServer(s);

  const ready = !!s.plex;
  const noLibraries = ready && s.plex.movies.length === 0 && s.plex.tv.length === 0;
  $("step-libraries").hidden = !ready;
  $("step-options").hidden = !ready || noLibraries;
  if (ready && !formFilled) fillLibraries(s.plex);
}

const MISSING = {
  not_installed: [
    "Arr couldn't find Plex on this computer. Arr puts movies and shows where Plex looks, so it has to run on the computer that runs Plex. ",
    "Install Plex Media Server", "https://www.plex.tv/media-server-downloads/",
    ", start it, then look again.",
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

function renderServer(s) {
  $("connected").hidden = !s.plex;
  if (s.plex) $("connected").textContent = `Found ${s.plex.name} on this computer.`;
  const install = s.install.state;
  // No Plex here (or it's being installed): offer to install it.
  const busy = install === "running" || linking;
  const installing = !s.plex && s.signed_in &&
    (busy || install === "failed" || (s.missing === "not_installed" && install !== "done"));
  $("install").hidden = !installing;
  if (installing) {
    $("missing").hidden = true;
    $("install-progress").hidden = !busy;
    $("install-step").textContent = linking ? "Linking Plex to your account…" : (s.install.step || "");
    $("install-ask").hidden = busy || !s.can_install;
    $("install-manual").hidden = busy || s.can_install;
    $("install-cmd").textContent = s.install_command || "";
    if (install === "failed" && !busy) showError(s.install.message);
    return;
  }
  // Signed in but not connected: always offer to look again, even when the last look
  // failed before it could say why (plex.tv unreachable, say).
  $("missing").hidden = !s.signed_in || !!s.plex;
  if (s.plex || !s.signed_in) return;
  const [before, label, href, after] = MISSING[s.missing] ||
    ["Arr couldn't check your Plex account just now. Check this computer's internet connection, then look again.", "", "", ""];
  const parts = [before];
  if (label) {
    const a = document.createElement("a");
    a.href = href; a.target = "_blank"; a.rel = "noopener noreferrer"; a.textContent = label;
    parts.push(a, after);
  }
  $("missing-text").replaceChildren(...parts);
}

function fillLibraries(plex) {
  formFilled = true;
  const none = plex.movies.length === 0 && plex.tv.length === 0;
  $("new-libraries").hidden = !none;
  $("libraries-intro").hidden = none;
  for (const el of document.querySelectorAll(".library")) el.hidden = none;
  $("step-options").hidden = none;
  $("apply").disabled = none;
  if (none) {
    $("new-movies").value = state.media_movies;
    $("new-tv").value = state.media_tv;
    return;
  }
  for (const kind of ["movies", "tv"]) {
    const sel = $(`${kind}-lib`);
    sel.replaceChildren();
    for (const lib of plex[kind]) {
      const o = document.createElement("option");
      o.value = lib.id;
      o.textContent = plex[kind].filter((l) => l.title === lib.title).length > 1
        ? `${lib.title} (${lib.plex_path})` : lib.title;
      sel.append(o);
    }
    const skip = document.createElement("option");
    skip.value = "";
    skip.textContent = kind === "movies" ? "Don't add movies" : "Don't add TV shows";
    sel.append(skip);
    if (!listening) {
      sel.addEventListener("change", () => libraryChanged(kind));
      $(`${kind}-folder`).addEventListener("change", () => checkFolder(`${kind}-folder`, `${kind}-hint`, false));
    }
    libraryChanged(kind);
  }
  if (!listening) $("downloads").addEventListener("change", () => checkFolder("downloads", "downloads-hint", true));
  listening = true;
}

function libraryChanged(kind) {
  const lib = state.plex[kind].find((l) => l.id === $(`${kind}-lib`).value);
  const folder = $(`${kind}-folder`);
  folder.closest(".folder").hidden = !lib;
  $(`${kind}-hint`).textContent = "";
  if (!lib) { suggestDownloads(); return; }
  folder.value = lib.local_path || "";
  $(`${kind}-hint`).textContent = lib.local_path
    ? (lib.local_path === lib.plex_path ? "" : `Plex sees this folder as ${lib.plex_path}.`)
    : `Plex keeps this library in ${lib.plex_path}. Enter where that folder is on this computer.`;
  if (lib.local_path) checkFolder(`${kind}-folder`, `${kind}-hint`, false);
  suggestDownloads();
}

function suggestDownloads() {
  const dl = $("downloads");
  if (dl.dataset.edited) return;
  const first = ["movies", "tv"].map((k) => $(`${k}-lib`).value && $(`${k}-folder`).value).find(Boolean);
  if (!first) return;
  // Beside the library folder: /srv/media/Movies -> /srv/media/arr-downloads, and on
  // Windows D:\Media\Movies -> D:\Media\arr-downloads.
  const sep = /^([A-Za-z]:)?\\/.test(first) ? "\\" : "/";
  const trimmed = first.replace(/[\\/]+$/, "");
  const cut = Math.max(trimmed.lastIndexOf("/"), trimmed.lastIndexOf("\\"));
  dl.value = `${cut >= 0 ? trimmed.slice(0, cut) : trimmed}${sep}arr-downloads`;
}

async function checkFolder(inputId, hintId, create) {
  const path = $(inputId).value.trim();
  if (inputId === "downloads") $(inputId).dataset.edited = "1";
  if (!path) return;
  try {
    const r = await call("POST", "/api/folder", { path, create });
    const hint = $(hintId);
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

function startSigninPolling() {
  clearInterval(polling);
  polling = setInterval(async () => {
    try {
      const s = await call("POST", "/api/plex/check");
      if (s.signed_in || !s.waiting_for_signin) { clearInterval(polling); polling = null; }
      render(s);
    } catch (e) {
      clearInterval(polling); polling = null;
      showError(e.message);
      render(await call("GET", "/api/state"));
    }
  }, 2000);
}

function startProgressPolling() {
  clearInterval(polling);
  polling = setInterval(async () => {
    let s;
    try { s = await call("GET", "/api/state"); } catch (_) { return; }
    render(s);
    if (s.phase.state === "done") {
      clearInterval(polling); polling = null;
      call("POST", "/api/finish").catch(() => {});
    } else if (s.phase.state !== "working") {
      clearInterval(polling); polling = null;
    }
  }, 1000);
}

$("signin-btn").addEventListener("click", async () => {
  showError("");
  // Open the tab now, while the click still counts: browsers block pop-ups opened later.
  const tab = window.open("about:blank", "_blank");
  try {
    const r = await call("POST", "/api/plex/signin");
    $("signin-link").href = r.url;
    if (tab) tab.location.href = r.url; else window.open(r.url, "_blank");
    render(await call("GET", "/api/state"));
    startSigninPolling();
  } catch (e) {
    if (tab) tab.close();
    showError(e.message);
  }
});

$("look-again").addEventListener("click", (ev) =>
  act(ev.target, () => call("POST", "/api/plex/check")));
$("install-look").addEventListener("click", (ev) =>
  act(ev.target, () => call("POST", "/api/plex/check")));

// Install Plex, then link it to the account (the check does that), showing each step.
$("install-btn").addEventListener("click", async (ev) => {
  await act(ev.target, () => call("POST", "/api/plex/install"));
  if (state && state.install.state === "running") followInstall();
});

// Shows the install's steps until it ends, then links the new Plex (the check does that).
function followInstall() {
  clearInterval(polling);
  polling = setInterval(async () => {
    let s;
    try { s = await call("GET", "/api/state"); } catch (_) { return; }
    if (s.install.state === "running") { render(s); return; }
    clearInterval(polling); polling = null;
    if (s.install.state === "done") await linkPlex(s); else render(s);
  }, 1000);
}

async function linkPlex(s) {
  linking = true;
  render(s);
  try { render(await call("POST", "/api/plex/check")); }
  catch (e) { showError(e.message); }
  finally { linking = false; render(state); }
}

$("create-libs").addEventListener("click", async (ev) => {
  const body = { movies: $("new-movies").value, tv: $("new-tv").value, language: navigator.language || "" };
  formFilled = false;
  await act(ev.target, () => call("POST", "/api/plex/libraries", body));
  const warnings = (state && state.warnings) || [];
  $("libs-warning").textContent = warnings.join(" ");
  $("libs-warning").hidden = warnings.length === 0;
  if (state && state.plex && !formFilled) fillLibraries(state.plex);
});

$("form").addEventListener("submit", async (ev) => {
  ev.preventDefault();
  const lib = (kind) => {
    const id = $(`${kind}-lib`).value;
    return id ? { id, folder: $(`${kind}-folder`).value.trim() } : null;
  };
  const body = {
    movies: lib("movies"),
    tv: lib("tv"),
    downloads: $("downloads").value.trim(),
    tmdb: $("tmdb").value.trim(),
    downloading: $("downloading").checked,
    auto_update: $("auto-update").checked,
  };
  await act($("apply"), () => call("POST", "/api/apply", body));
  if (state && state.phase.state === "working") startProgressPolling();
});

call("GET", "/api/state").then((s) => {
  render(s);
  if (s.waiting_for_signin) startSigninPolling();
  // Reloaded during or just after installing Plex: pick up where it was.
  if (s.install.state === "running") followInstall();
  else if (s.install.state === "done" && !s.plex && s.signed_in) linkPlex(s);
  if (s.phase.state === "working") startProgressPolling();
}).catch((e) => showError(e.message));
