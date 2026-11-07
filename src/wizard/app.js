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

  $("step-server").hidden = !s.signed_in && !s.plex;
  renderServers(s);

  const ready = !!s.plex;
  $("step-libraries").hidden = !ready;
  $("step-options").hidden = !ready;
  if (ready && !formFilled) fillLibraries(s.plex);
}

function renderServers(s) {
  const box = $("servers");
  box.replaceChildren();
  if (s.plex) {
    $("connected").hidden = false;
    $("connected").textContent = `Connected to ${s.plex.name}.`;
    return;
  }
  $("connected").hidden = true;
  if (!s.signed_in) return;
  if (s.servers.length === 0) {
    const p = document.createElement("p");
    p.textContent = "Arr didn't find a Plex server on this account. Check that your server is on and signed in to Plex, then look again, or connect another way below.";
    const again = document.createElement("button");
    again.type = "button";
    again.textContent = "Look again";
    again.addEventListener("click", () => act(again, () => call("POST", "/api/plex/check")));
    box.append(p, again);
    $("manual").open = true;
    return;
  }
  const p = document.createElement("p");
  p.textContent = s.servers.length === 1 ? "Your server:" : "Which server should Arr use?";
  box.append(p);
  for (const srv of s.servers) {
    const b = document.createElement("button");
    b.type = "button";
    b.textContent = `Use ${srv.name}`;
    b.addEventListener("click", () => act(b, () => call("POST", "/api/plex/server", { id: srv.id })));
    box.append(b);
  }
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

$("manual-btn").addEventListener("click", (ev) =>
  act(ev.target, () => call("POST", "/api/plex/manual", {
    url: $("manual-url").value, token: $("manual-token").value,
  })));

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
