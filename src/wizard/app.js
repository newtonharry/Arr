"use strict";

const $ = (id) => document.getElementById(id);
let state = null;
let polling = null;
let formFilled = false;

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
  $("step-libraries").hidden = !ready;
  $("step-options").hidden = !ready;
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
  unreadable: [
    "Arr found Plex on this computer but couldn't read its libraries. If Plex is still starting, wait a minute, then look again.",
    "", "", "",
  ],
};

function renderServer(s) {
  $("connected").hidden = !s.plex;
  if (s.plex) $("connected").textContent = `Found ${s.plex.name} on this computer.`;
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
  $("no-libraries").hidden = !none;
  $("apply").disabled = none;
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
    sel.addEventListener("change", () => libraryChanged(kind));
    $(`${kind}-folder`).addEventListener("change", () => checkFolder(`${kind}-folder`, `${kind}-hint`, false));
    libraryChanged(kind);
  }
  $("downloads").addEventListener("change", () => checkFolder("downloads", "downloads-hint", true));
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
  const parent = first.replace(/\/+$/, "").replace(/\/[^/]*$/, "") || "/";
  dl.value = `${parent === "/" ? "" : parent}/arr-downloads`;
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
  if (s.phase.state === "working") startProgressPolling();
}).catch((e) => showError(e.message));
