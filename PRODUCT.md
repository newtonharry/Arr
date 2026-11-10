# Product

<!-- impeccable:product-schema 1 -->

## Platform

web

## Users

- **The person who installed Arr**: runs Plex on a computer at home, set Arr up with the
  installer, and wants movies and TV to "just turn up" in Plex.
- **Their household**: people who watch Plex but did not set anything up and have no
  technical knowledge. They ask for things to watch; they do not manage downloads.

Both use Arr from anywhere: at home on the same network, or away from home on a phone,
through the same public address (ngrok) and Google sign-in that ChatGPT already uses.

## Product Purpose

Arr turns a home computer into a complete movie and TV setup in one program. You ask for a
movie or a season; Arr searches, downloads it with its built-in BitTorrent engine, checks the
file, files it into the Plex library and tells Plex to refresh, so it's ready on the TV. It
replaces the usual Sonarr, Radarr, Prowlarr and qBittorrent stack.

Until now the only ways in were an AI assistant over MCP ("add season 2 of Severance") and the
JSON API. The web interface adds a direct way to find and add movies and shows, see what's on
its way, see what's coming up, and browse ideas and the library, without an assistant. Ideas leave out what the household already has and can drop a show with "Not interested" (for everyone using that Arr); search shows recent and popular titles before typing and marks what is already in Plex; show and movie pages carry the summary, genres and top cast. The library sorts by recently added, title or
size, shows how much space the drive has left (and says so when it runs low), shows who added
each title, and lets a person make Arr stop tracking a title. That last one never deletes files:
anything already in Plex stays, and deleting is left to Plex.

Success: someone who has never seen Arr finds a film, taps once, and later finds it in Plex,
without learning any media-server vocabulary.

## Positioning

One program, not a stack of five, and decisions are Arr's job: it picks the release, quality
and file, so the person only says *what* they want. The *arr ecosystem exposes every knob;
Arr's interface exposes the wish and the outcome.

## Operating Context

- Arr runs as a background service on the Plex computer; the interface is served by Arr itself.
- Remote access: an ngrok HTTPS domain plus Google sign-in limited to listed accounts
  (docs/REMOTE.md). The same address and sign-in serve the web interface.
- Plex is where people watch. Arr's interface is where they ask; it should hand off to Plex,
  not compete with it. People watch on their TV, so there is no "Watch" button in Arr.
- The AI-assistant route (ChatGPT/Claude over MCP) stays; the interface is an alternative,
  not a replacement.

## Capabilities and Constraints

- Movies are identified with TMDB (needs the user's own free key; without one, movie search
  is off). TV uses TVmaze (no key). TMDB's API terms apply: attribution with the TMDB logo,
  no caching TMDB data longer than 6 months, and an open question about §1.C (AI-connected
  apps) that TMDB has not answered.
- One tap to add: Arr chooses release and quality. No release picking in the interface (a
  person who wants to can choose a copy under Download options, or from a Needs-you item).
- Every add says what happened: a short "Added Title" note with Undo when Undo is safe. A
  download that needs the person says what went wrong in one plain sentence and offers the one
  button that helps. Downloads on their way say how long is left, when Arr can tell.
- Existing JSON API covers search/discover, add to library, wanted, calendar, queue
  (pause/resume/cancel/retry), history, attention and library listing.
- Shipped as a single Rust binary with embedded static assets (like the setup page); no
  separate web server, no build toolchain for users. The web app is Preact, vendored in the
  repository and written without JSX, so there is no build step for developers either.
- **Undecided**: where artwork (posters) comes from, given TMDB's terms; what source feeds
  "ideas to browse"; whether household members get any restrictions compared with the owner.

## Brand Commitments

- Name: Arr. Plain, friendly English; no media-server jargon (no "indexer", "grab", "quality
  profile") in anything a household member reads.
- Existing setup page and website exist (src/wizard/, site/); their look is evidence for the
  visual world, not yet a documented design system.
- Opt-in only for anything that phones home (error reports, auto-updates).
- Phone notifications are opt-in on each device. They use standard Web Push, so a message
  goes through the browser maker's push service (Google, Apple or Mozilla); it is encrypted
  for the device and carries only a title ("*Title* is ready to watch" or "*Title* needs
  you"). On iPhone they work only once Arr is added to the Home Screen (iOS 16.4 or later).

## Evidence on Hand

- Real product: the binary, API (src/api.rs), setup page (src/wizard/), website (site/),
  mascot (site/mascot.svg).
- No testimonials, user counts or press exist; do not invent any.

## Product Principles

1. Say what you want, not how to get it: Arr makes the technical decisions.
2. Calm by default: show what matters now; detail only when something needs a person.
3. Plex is the destination: every flow ends with "it's in Plex".
4. Works the same at home and away, on a phone first.
5. Nothing happens without consent: no surprise downloads, no hidden phoning home.
