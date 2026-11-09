---
version: 1
slug: "src-webapp-index-html"
primary_target: "src/webapp/index.html"
related_targets: []
---

# Arr web app (src/webapp/)

Mode: operate. Phone-first, also desktop. Served by Arr on its own port and the remote (ngrok) address.

## Job
Owner and household (non-technical), at home or away, find a movie or show, add it with one tap, and follow it until it's in Plex. Also: what's coming up, ideas to browse, the library (each item hands off to Plex to watch).

## Decisions (confirmed 2026-11-09)
- Sign-in: the existing Google sign-in (remote access) everywhere; at home without remote access, the owner shares a one-time link.
- Adding a show: latest season plus every new episode; earlier seasons from the show's page.
- Artwork: Plex art for owned items, TVmaze for shows, TMDB for movies (with TMDB logo attribution), a title tile otherwise.
- Ideas: TVmaze schedule for shows plus movies related to ones added; TMDB trending only if TMDB's terms allow.
- No release picking, quality, speeds, seeders, or jargon (indexer, grab, queue).

## Direction contract
THESIS: One calm feed that answers "is my thing coming?", with search pinned on top. It refuses the *arr dashboard (tables of queue rows, stats, settings) and the Plex clone (walls of posters).
OWN-WORLD: The website's world: cream ground #fff8ef, white surfaces, ink #23302f, teal #1c8576 for action and arrival, coral #ff7a5c only for "needs you", sun #ffc94d tint for dates; rounded type (Nunito, self-hosted); 2px warm borders, 16-18px radii; the mascot as the brand mark. Dark mode from the site's dark palette.
STORY: The visitor sees what arrived (Watch), what's on its way and when, what needs them, what's coming; types a title, taps Add, watches it join the feed.
FIRST VIEWPORT: Brand row (mascot + Arr), full-width search field with the site's soft drop, then the feed: section headings, rows of poster + title + one next fact; arrived rows carry a teal Watch button; bottom bar Home / Ideas / Library (top on desktop, single centred column max ~640px).
FORM: The feed, position 6 of 7 on the ranked list, dealt as THE ROLL; seed:01431190f5aae1cf. Signature move: the four-stop journey line (Found, Downloading, Checking, In Plex) under each item on its way, filling as Arr works.
FINISH: unreviewed and undocumented is unfinished; this build ends with the finish review, the verdict, DESIGN.md, and every shipping raster carrying its provenance
