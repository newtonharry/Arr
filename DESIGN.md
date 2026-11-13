---
name: Arr
description: A warm, rounded home-cinema feed where teal means arrival, coral means "needs you", and a cream page holds white cards with warm borders.
colors:
  cream: "#fff8ef"
  card: "#ffffff"
  ink: "#23302f"
  muted-ink: "#566563"
  warm-line: "#eadfce"
  teal: "#1c8576"
  teal-ink: "#17766a"
  on-teal: "#ffffff"
  coral: "#ff7a5c"
  coral-ink: "#a8331a"
  sun-tint: "#fff1c9"
  sun-ink: "#6b4c00"
  focus-ring: "#0b5fff"
  selection: "#bfe9e1"
  night: "#161d1d"
  night-card: "#202a2a"
  night-ink: "#f1ece2"
  night-muted-ink: "#b4c0bd"
  night-line: "#334141"
  night-teal: "#4fd1bd"
  night-on-teal: "#0f1f1d"
  night-coral-ink: "#ff9a82"
  night-sun-tint: "#3b3316"
  night-sun-ink: "#ffd97a"
  night-focus-ring: "#8ab4ff"
  night-selection: "#1f5c53"
typography:
  brand:
    fontFamily: "Nunito, ui-rounded, system-ui, -apple-system, 'Segoe UI', Roboto, sans-serif"
    fontSize: "24px"
    fontWeight: 800
    lineHeight: 1.4
  headline:
    fontFamily: "Nunito, ui-rounded, system-ui, sans-serif"
    fontSize: "28px"
    fontWeight: 800
    lineHeight: 1.15
  title:
    fontFamily: "Nunito, ui-rounded, system-ui, sans-serif"
    fontSize: "16px"
    fontWeight: 800
    lineHeight: 1.3
  body:
    fontFamily: "Nunito, ui-rounded, system-ui, sans-serif"
    fontSize: "15px"
    fontWeight: 500
    lineHeight: 1.4
  meta:
    fontFamily: "Nunito, ui-rounded, system-ui, sans-serif"
    fontSize: "14px"
    fontWeight: 500
    lineHeight: 1.4
  label:
    fontFamily: "Nunito, ui-rounded, system-ui, sans-serif"
    fontSize: "13px"
    fontWeight: 800
    lineHeight: 1.4
    letterSpacing: "0.06em"
rounded:
  tile: "10px"
  idea-tile: "12px"
  button: "12px"
  filter: "14px"
  card: "18px"
  pill: "999px"
spacing:
  xs: "6px"
  sm: "10px"
  md: "12px"
  lg: "18px"
  gutter: "20px"
  stack: "22px"
components:
  search-field:
    backgroundColor: "{colors.card}"
    textColor: "{colors.ink}"
    rounded: "{rounded.card}"
    height: "56px"
    padding: "0 16px"
  item-row:
    backgroundColor: "{colors.card}"
    textColor: "{colors.ink}"
    rounded: "{rounded.card}"
    padding: "12px"
  item-row-arrived:
    backgroundColor: "{colors.card}"
    textColor: "{colors.ink}"
    rounded: "{rounded.card}"
  title-tile:
    textColor: "#ffffff"
    rounded: "{rounded.tile}"
    width: "46px"
    height: "68px"
  button-primary:
    backgroundColor: "{colors.teal}"
    textColor: "{colors.on-teal}"
    rounded: "{rounded.button}"
    padding: "0 14px"
    height: "44px"
  button-primary-night:
    backgroundColor: "{colors.night-teal}"
    textColor: "{colors.night-on-teal}"
    rounded: "{rounded.button}"
  button-needs-you:
    backgroundColor: "transparent"
    textColor: "{colors.coral-ink}"
    rounded: "{rounded.button}"
    padding: "0 14px"
    height: "44px"
  button-ghost:
    backgroundColor: "transparent"
    textColor: "{colors.ink}"
    rounded: "{rounded.button}"
    height: "44px"
  button-done:
    backgroundColor: "transparent"
    textColor: "{colors.teal-ink}"
    rounded: "{rounded.button}"
    height: "44px"
  tab-bar:
    backgroundColor: "{colors.card}"
    textColor: "{colors.muted-ink}"
  tab-current-desktop:
    backgroundColor: "{colors.teal}"
    textColor: "{colors.on-teal}"
    rounded: "{rounded.button}"
    height: "44px"
  date-chip:
    backgroundColor: "{colors.sun-tint}"
    textColor: "{colors.sun-ink}"
    rounded: "{rounded.button}"
    width: "46px"
    height: "52px"
  segmented-pill:
    textColor: "{colors.ink}"
    rounded: "{rounded.pill}"
    height: "44px"
    padding: "0 16px"
  segmented-pill-selected:
    backgroundColor: "{colors.teal}"
    textColor: "{colors.on-teal}"
    rounded: "{rounded.pill}"
---

# Design System: Arr

## Overview

**Creative North Star: "The Home Cinema Foyer"**

Arr's web app is one calm feed on a warm cream page: white rounded cards, ink-green text, a single teal for action and arrival, a friendly round typeface, and the mascot as the brand mark. It is the same world as the Arr website (`site/style.css`, the brand source), carried into a phone-first, single-column app. It reads like a household noticeboard for a TV room, not an admin console: nothing shows a queue table, a speed or a setting, and nothing imitates a wall of posters.

The page answers one question, "is my thing coming?". Search is pinned at the top; below it the feed deals out sections (what arrived, what is on its way, what needs you, what is coming up). Each row is a title tile or date chip, a title, and one next fact. Colour carries meaning, not decoration: teal says "it is here or you can act", coral says "this needs you", sun-yellow says "a date". Everything else is cream, white, ink and warm line.

Density is relaxed: 44px minimum targets, 18px card corners, 2px borders, 10px between rows, 22px between sections. Dark mode is the website's night palette, not an inversion.

**Key Characteristics:**
- Cream page, white cards, every card outlined in a 2px warm line.
- Teal is the only action colour; coral is reserved for "Needs you".
- Nunito (self-hosted) at weights 500 to 800; bold, rounded, no thin weights.
- Single centred column, max 680px; bottom tab bar on phones, top-right tabs on desktop.
- Title tiles (coloured initials) stand in for artwork; no posters yet.
- The four-stop journey line is the signature element.
- Built with Preact (vendored in `src/webapp/vendor/`, written with `h`, no JSX, no build step); the look lives in `app.css` alone.

## Colors

A warm, food-and-lamplight palette: cream and white grounds, deep green-black ink, one sea-teal, one coral, one pale sun yellow. Defined once as custom properties on `:root` in `src/webapp/app.css`; dark values replace them under `prefers-color-scheme: dark`. Frontmatter holds light values by plain name and dark values with a `night-` prefix. Where the dark value of a token is unchanged (coral, white tile text) there is no `night-` entry.

### Primary
- **Sea Teal** (`teal`, #1c8576; dark `night-teal` #4fd1bd): fills for the primary button, the selected pill, the journey fill and arrived-row border; the search glyph; `accent-color` and caret. White text on it in light mode, deep teal-black (`night-on-teal` #0f1f1d) in dark.
- **Teal Ink** (`teal-ink`, #17766a; dark is the same as `night-teal`): teal used as text or small glyph on cards and cream (links, tags like "In Plex", the current tab, done-state buttons, success messages). Darker than Sea Teal so it holds contrast on white.

### Secondary
- **Needs-You Coral** (`coral`, #ff7a5c, same in dark): the border of the outlined "Needs you" button only.
- **Coral Ink** (`coral-ink`, #a8331a; dark `night-coral-ink` #ff9a82): the text of problem lines, error messages and the coral button label. Coral is a signal, never a fill and never decoration.

### Tertiary
- **Sun Tint** (`sun-tint`, #fff1c9; dark `night-sun-tint` #3b3316) with **Sun Ink** (`sun-ink`, #6b4c00; dark `night-sun-ink` #ffd97a): the date chip and the inline banner. Always a soft tint with matching dark text. (The website's saturated sun #ffc94d is used for its pills and is not used in the app.)

### Neutral
- **Cream** (`cream`, #fff8ef; dark `night` #161d1d): the page.
- **Card White** (`card`, #ffffff; dark `night-card` #202a2a): rows, the search field, the tab bar, filter input.
- **Ink** (`ink`, #23302f; dark `night-ink` #f1ece2): headings and body.
- **Muted Ink** (`muted-ink`, #566563; dark `night-muted-ink` #b4c0bd): meta lines, section labels, inactive tabs, placeholders.
- **Warm Line** (`warm-line`, #eadfce; dark `night-line` #334141): every 2px border, the search field's drop, the journey track, scrollbar thumbs.
- **Focus Ring** (`focus-ring`, #0b5fff; dark `night-focus-ring` #8ab4ff): 3px focus outline, 3px offset. Deliberately not a brand colour.
- **Selection** (`selection` #bfe9e1; dark `night-selection` #1f5c53): text selection.

Title-tile gradients (eight fixed pairs, `.t0` to `.t7`: teal, coral-red, ochre, slate-blue, violet, green, berry, grey) are a decorative set used only on tiles, with white initials; they are not part of the semantic palette.

### Named Rules
**The Coral Means Needs-You Rule.** Coral appears only on rows in "Needs you" and error messages. If an item is not waiting on a person, it is not coral.

**The Two Teals Rule.** Sea Teal fills; Teal Ink writes. Never set small teal text in `teal` on a white or cream ground.

**The Dates Are Sun Rule.** A date is a pale sun chip. Sun is not used for status, warnings or buttons.

## Typography

**Display / Body Font:** Nunito (self-hosted woff2, latin and latin-ext, variable weight 500 to 800) with `ui-rounded, system-ui, -apple-system, "Segoe UI", Roboto, sans-serif`.

**Character:** one round, friendly family at heavy weights; hierarchy comes from weight and size, not from a second face. Tabular numerals are on for the whole page so dates, percentages and times line up.

### Hierarchy
- **Brand** (800, 24px, 1.4): the "Arr" word beside the mascot.
- **Headline** (800, 28px, 1.15): sign-in headline; empty-state heading is 22px/800 in ink.
- **Title** (800, 16px, 1.3): item titles, one line with ellipsis (two-line clamp for arrived rows and ideas at 14px).
- **Body** (500, 15px, 1.4): default text; search input is 17px/600.
- **Meta** (500, 14px, muted ink): the one next fact under a title, single line with ellipsis; 12px/800 for tags and tab labels.
- **Label** (800, 13px, +0.06em, uppercase, muted ink): section headings in the feed, with an optional right-aligned link in sentence case 14px/800.

### Named Rules
**The Heavy Round Rule.** Titles and controls are 800; supporting text is 500 to 600. No light weights.

**The Plain Words Rule.** Copy is household English: "Found, getting ready", "Downloading", "Checking the file", "In Plex", "Ready to watch". No indexer, grab, queue, quality or seeder vocabulary anywhere a household member reads.

## Layout

A single centred column, `max-width: 680px`, 20px side gutters, on every viewport. Page structure: brand row, search field, banner (only when non-empty: "Can't reach Arr right now" or "Arr is still being set up"; Arr asks again every 5s on any screen and the banner goes as soon as the answer is good), then the update bar (only when there is something to say), then the feed in `main`, then a quiet footer holding only the data credits. Who is signed in and Sign out are in Settings. Sections stack with 22px between them; rows in a list are 10px apart and each row is a full-width card. The ideas section is the one horizontal scroller: 132px-wide cards with proximity snap, bleeding to the page edge (`margin: 0 -20px`).

Responsive: under 760px the tab bar is fixed to the bottom (68px plus safe-area inset, 56px tall targets, icon over 12px label) and the page pads for it. From 760px the same nav moves to the top right on the brand row as 44px pills; the current tab fills teal. Under 420px the coral button in a Needs-you row collapses to its icon. Under 360px journey labels drop to 11px. Safe-area insets are respected top and bottom.

Rows align centre, except search results and library entries which top-align. Targets are 44px minimum.

## Elevation & Depth

Flat and tonal: depth is made with white cards on cream and 2px warm borders, not blur. The one lift is the search field's drop, a solid offset in the warm line colour with no blur: `box-shadow: 0 5px 0 var(--line)` on the search field (the website's install card uses the same device at 8px). It makes the search read as the pressable heart of the page. No other element in the app has a shadow. State depth is a 1px press (`translateY(1px)`) and a `brightness(1.08)` lift on hover.

### Named Rules
**The One Drop Rule.** Only the search field carries a drop. Rows, buttons and tabs sit flat on their border.

## Shapes

Soft, generous corners with a consistent grammar: 18px for cards, rows and the search field; 12px for buttons, date chips, the banner and idea tiles; 14px for the filter input; 10px for list title tiles; full pill (999px) for the segmented filter buttons. All containers are outlined in a 2px `warm-line` border; focus outlines follow an 8px radius. Teal marks arrival by recolouring the row border (arrived rows only). Journey stops are 11px circles with a 2px teal ring. Icons are inline SVG line icons: 22px, 2.2 stroke, round caps and joins (14 to 18px inside tags and buttons).

## Components

### Search Field
Full width, 56px tall, white, 2px warm border, 18px radius, the solid 5px drop, a teal magnifier glyph and a 17px/600 input with muted placeholder "Find a movie or show". Focus shows the 3px blue ring around the whole field, not the input.

### Item Rows
White card, 2px warm border, 18px radius, 12px padding, 12px gap: lead tile or date chip, a text column (title, one meta line), and a trailing action when needed.
- **Arrived:** teal border, an "In Plex" tag (teal ink, check glyph, 12px/800) above the title. There is no Watch button: people watch on their TV, so Arrived rows and the movie page just say "In Plex" and "Ready to watch".
- **On its way:** title, status line ("42% downloaded", "Paused"; with a rate, "42% · about 12 min left", "less than a minute left", "about 2 h left"; nothing about time when it is unknown, paused or stalled), then the journey line.
- **Needs you:** neutral border (the card stays calm), a coral-ink problem line with an alert glyph saying in one plain sentence what went wrong, and one coral outline button under it that fixes it ("Try again", or "Choose a copy yourself", which opens the title with its copy list showing). A kind the app does not know keeps the server's own sentence and "Try again".
- **Coming up / looking:** a sun date chip as the lead, "Out 14 Nov. Arr will get it then."

### Scores
A calm line of plain-text labels, "IMDb 7.8 · Critics 96% · Audience 87%" (14px muted, the numbers in ink at 800), showing only the scores Plex gave. It sits under the meta line on Ideas and Search rows, under the title on the show and movie sheets, and stacks one per line on the narrow Ideas cards. Next to it, a small outlined pill gives the verdict ("Likely good", "Critics’ pick", "Crowd-pleaser", "Mixed reviews", "Probably skip"). The pill is neutral (teal ink only for "Likely good"); "Probably skip" is never coral, because it is advice, not an error. Its `title` and screen-reader text give the reason ("Critics 96%, audience 87%: both high"). No third-party logos. Plex scores replace the TVmaze star; with none, the star stays.

### Title Tiles
46x68px portrait block, 10px radius, vertical two-stop gradient picked deterministically from the title (8 sets), white initials at 16px/800. In ideas they widen to a 132x92 block with 28px initials. They are honest placeholders for artwork and are used wherever a poster would sit.

### Buttons
- **Primary (teal):** 44px tall, 12px radius, 0 14px padding, 2px teal border and fill, white text (dark mode: teal-black on light teal), 14px/800, optional 18px icon. Hover brightens; active presses 1px. Used for Add and Get.
- **Needs-you (coral outline):** transparent fill, 2px coral border, coral-ink label, same size. Used only inside Needs-you rows ("Try again" style actions).
- **Done:** after adding, the primary button becomes transparent with warm border and teal-ink label ("Added", "In Plex", "Following").
- **Ghost:** transparent, warm border, ink label, border turns teal on hover.
- **Link button:** no fill, teal-ink, 800, underlined, 44px tall ("see all", Use Arr on your phone).
- Busy is 70% opacity with progress cursor.

### Switch
- **Notifications:** a 52x32px pill (2px warm border, cream fill) with a 22px knob. On, it fills teal and the knob turns on-teal and slides right. It sits in the "You" section of Settings beside its plain state in muted text ("On", "Off", "Blocked in your browser settings", "Add Arr to your Home Screen first"), with a 13px muted privacy line below. Disabled is 50% opacity. The tap target is padded to 44px.

### Update Bar, Settings and Health
- **Update bar:** one line in a card just under the header, above the feed, so it shows without scrolling with a 2px teal border ("Arr 0.12.1 is available · Update now"; coral border for "The update didn't work"). Its action is a link button; it is a polite status, or an alert when something failed.
- **Settings:** the first section is "You": who is signed in, Use Arr on your phone, notifications and a ghost-style Sign out button. It never waits on the settings, so Sign out works while they load, fail or Arr restarts. Then sections as plain h2 labels over Item Rows. On/off settings use the Switch row (title, a muted line saying what the state means). Values are an Item Row with a label, a muted line, and a 44px input beside a teal Save button. Changing anything shows the mascot with "Restarting…" until Arr answers again.
- **Health:** one Item Row per check with a 28px round mark: teal check for OK; sun-yellow ring with "!" for needs a look; coral with "!" for a problem. The word ("OK", "Needs a look", "Problem") is in the row for screen readers, so colour is never the only signal. "Recent problems" is a details list of cards: muted time and component, then the message.

### Journey Line
The signature. A 36px-tall four-stop track under any item on its way: Found, Downloading, Checking, In Plex. A 3px warm-line rail with a teal fill that scales from the left (`scaleX`, 0.9s `cubic-bezier(0.16, 1, 0.3, 1)`) as Arr works; stops are 11px circles, hollow until reached, solid teal when on; stop one is left-aligned, stop four right-aligned, two and three centred at one third and two thirds. The current stop's label is ink 800, others muted 600 at 12px. Paused turns the fill muted grey. The last stop, In Plex, is reached only when the row moves to Arrived.

### Toast
The note after Add or Get ("Added **Title**", "Following **Title**", and "Undone. ..." after Undo). Inverse of the page (ink fill with cream text; cream fill with ink text in dark mode), 14px radius, no border and no shadow, one line, with a 44px "Undo" link button when an existing route fully reverses the add (only for a title Arr did not track at all before: Undo removes it from Arr, which cancels downloads that have not finished and never touches files already in Plex; if a download is nearly done it says "Too late to undo: it's nearly downloaded." and changes nothing). No Undo when the title was already tracked or in Plex. It sits at the bottom, centred, above the tab bar on phones, over a title sheet. It is an `aria-live="polite"` region, lasts about 6s, waits while hovered or focused, is reachable by Tab (it is after the list, before the footer, and inside the sheet's focus loop), Z undoes, Escape closes it.

### Tabs
Home / Ideas / Library / Settings, icon plus label (a ring with eight short rays for Settings). Phone: bottom bar, white, 2px warm top border, inactive muted 12px/800, current teal ink. Desktop: top-right pills, inactive muted with teal on hover, current filled teal with on-teal text.

### Date Chips
46x52px, 12px radius, sun tint with sun ink text: a 12px uppercase month or day name over a 20px day number. Used as the lead for anything with a date.

### Segmented Pills and Filter
Pill buttons (999px, 2px warm border, 44px tall, 800) for choices like Movies / Shows; selected is teal filled with on-teal text. The library filter input is 44px, white, 2px warm border, 14px radius; the sort select next to it matches it and wraps below on a phone.

### Drive Meter and Confirm Dialog
The drive meter is a text line ("Movies drive: 1.2 TB free of 4 TB") over a 10px pill track, teal fill. Under 10% free the line and fill turn coral ink and add "Running low on space"; it is the warning colour, not sun. A destructive question (Remove from Arr) is a centred white card with a 2px warm border over the scrim, inside the sheet; Cancel is first and holds the focus when it opens, Remove is a filled coral-ink button. Tab stays in the card and Escape closes it. "Delete files…" (a coral-ink link beside Remove from Arr; on a season and an episode as "Delete this season's files…" and "Delete…") asks the same way: the card first says it is looking, then how many files and how much space move to the bin for 7 days, any shared download that stays, and a "Get a new copy afterwards" checkbox (off). Delete is the filled coral-ink button and stays inactive until the preview has loaded. After it, the toast says "Deleted **Title**. Undo within 7 days." with Undo. Settings shows a "Bin" section (hidden when empty) with Restore and "Delete now" (which asks "Delete for good?" first).

### Ideas, Search and Title-page Extras
- **Genre chips:** the segmented pill style above, one row that scrolls sideways on a phone: "All" plus the six genres most ideas share. One is selected at a time and nothing is saved.
- **Not interested:** a quiet 13px/700 muted link button under an idea; it hides the show for everyone using this Arr and raises the toast. Shows already in the library or Plex are left out of Ideas unless "Show ones I have" (a link button with a pressed state) is on.
- **Toast:** one small pill above the tab bar, ink fill with page-coloured text and a link-button action ("Hidden · Undo"), gone after seven seconds. No shadow.
- **Search before typing:** tapping the search field opens Search with "Recent" (the last eight searches on this device, each removable, plus "Clear") and "Popular now" (the same ideas row as Home).
- **Owned badge:** the "In Plex" tag style (teal ink, check glyph) under a search result's meta line: "In Plex", "On its way" or "Tracking". A movie already in Plex shows the Done button instead of a second badge.
- **People in search:** a person who matches the search (their block with their work) or a row of names (chips, "People") has one place, the top of the results, above Results. Rows that arrive later go below it and never push it out of sight. The first draw waits up to 150 ms for the people (not at all once they have answered, or failed, or found nothing); people that arrive after that glide in and the rows below glide down. While typing on, the names or the person already shown that still fit the new query stay until the new answer replaces them; drawing the same query again keeps them.
- **Title pages:** under the summary (four lines, then "More"), genres as small pills (999px, 2px warm border, 13px/800 muted); at the end of the page a "Cast" section of up to six names with the character in muted text, two columns on a phone. Names only, no photos.

### Sport
Schedules only: the words never promise a download and nothing shows how a game went (no scores, no winners; an event is a name and a time).
- **Event row:** an Item Row with the sun date chip as the lead (the day the event starts), the event's name (title, two-line clamp, one line is not enough for "Brighton & Hove Albion v Manchester United"), then the time in the viewer's own clock as the meta line: "Today 7:30 pm", "Sat 7:30 pm" within the week, "Sat 14 Nov 7:30 pm" after that. A tournament that runs for days shows its span and no time ("Thu 12 – Sun 15 Nov", "Under way, until Sun 15 Nov"). A postponed event shows its day and a small outlined "Postponed" pill (neutral, never coral: nothing needs the person). On Home a small 12px/800 uppercase muted label above the name gives the league; the whole row opens the league's page. On the league page the second meta line holds the round and venue when known.
- **League and team rows** (search, Library): a title tile with the name's initials, the name, "League · Soccer" or "Team · Premier League" in the meta style, and a Follow button. Search shows six at first and "See all N" for the rest. Follow is the primary button; Following is the done style. It changes the moment it is tapped and goes back, shaking, with a plain line under the row if it fails. Unfollowing from search or the league page raises the toast with Undo. In Library > Sport the button is a ghost "Unfollow" (the row goes at once, Undo puts it back) and a row adds "Next Sat 14 Nov 7:30 pm" and the event's name. Under 380px the button drops under the text so the names are not squeezed.
- **League page:** a sheet like a title's: the initials tile, the name, the sport; a switch row "Follow Premier League" ("Its games show up under Coming up on Home."; "fights" and "tournaments" for MMA and golf); a quiet line when the schedule is being read ("Updating the schedule…") or could not be ("Couldn’t update the schedule just now. Showing what Arr had."); "Coming up" (five events, "See all N" for the rest, or "Nothing is scheduled right now. It may be the off season."); "Teams" for leagues that have them, each a compact row with Follow (a filter input "Find a team" and eight rows when there are more than eight, "Show all N teams" for the rest; a team the person has followed or just tapped never leaves the list under their finger); and "Earlier", five names and days.
- **Library choice:** "Sport" is a third segmented pill next to Movies and Shows, and appears only where sport is switched on.

### Empty and Sign-in
Centred mascot (96px empty, 120px sign-in), a heading, one muted line (max 30ch) and at most one teal button. The mascot (`src/webapp/mascot.svg`) is the brand mark at 34px beside "Arr" and as the favicon (a `<link rel="icon">` on every page, and `/favicon.ico` redirects to it for browsers that ask anyway).

### Motion
Motion explains what just happened and never makes you wait. Only `transform` and `opacity` move. Interface changes take about 300ms or less; a playful spring (up to about 600ms) is kept for the moments that are good news: an add, an arrival, a restore. No animation is ever awaited by an action, delays an input, or blocks a tap. No animation may make the page wider than the screen, even for a frame: a phone zooms the whole screen out to fit it, so the page clips sideways overflow and anything that travels further flies in a fixed layer. Easings: expo-out `cubic-bezier(.16,1,.3,1)` for most things, a spring `cubic-bezier(.34,1.56,.64,1)` for pops, a softer spring `cubic-bezier(.3,1.25,.5,1)` for things that glide and settle. The mascot never animates in the app.

What animates:
- **Screens:** the tab bar has a pill that slides behind the current tab (springy, phones only) and the tapped icon bounces; the page slides in 28px the way the tabs sit in the bar, or rises 8px for any other change. Opening a title grows the tapped poster into the sheet's big one (a flying copy, 440ms) while the sheet rises over a fading scrim; closing reverses it. On phones the sheet can be pulled down by its handle: past 80px it closes, under that it springs back. Only the handle moves it, so the page inside still scrolls.
- **Loading:** shimmering placeholders shaped like the rows (the words stay for screen readers in a status line). A 3px teal line runs across the top while the screen shows its last answer ("Updating…").
- **Add and Get:** a squish on tap, a spinner while it works, a drawn tick and a pop when it lands, a shake beside the error when it fails.
- **Lists:** rows rise in one after another (60ms apart, first ten) the first time a screen is drawn, never on a poll or a later redraw. Rows that move (a download arriving, a card going, a gap closing) glide from where they were. A dismissed card shrinks away first.
- **Toasts:** slide up, slide out. Undo has a ring that drains over the toast's real time and starts again when that time does.
- **Progress:** the percentage counts up between updates; a download that is moving has a sheen along its bar, a paused one does not. The journey fill keeps its 0.9s expo-out.
- **Arrived:** when a title lands in Plex while Arr is open, a sweep of light crosses its poster, the card hops and confetti bursts (about 34 pieces). It plays once per item (remembered in this browser, so a reload does not repeat it), never for what had already arrived when Arr opened, and from another tab it pulses the Home tab instead. A successful Add throws a smaller burst. Never more than two bursts at once.
- **Small things:** the search bar lifts a little when focused; switches snap with a small overshoot; in the Bin a deleted row drops away, a restored one pops back out and an entry that turns up pops in.

Everything is off under `prefers-reduced-motion`: the stylesheet turns off every transition and animation, and the script does not start any animation, flying poster, count-up or confetti.

## Do's and Don'ts

### Do:
- **Do** keep the cream page, white cards, ink text and 2px `warm-line` borders on every container; dark mode swaps to the `night-` values.
- **Do** use Sea Teal for fills and arrival (primary button, journey, arrived border) and Teal Ink for teal text.
- **Do** use coral only for things that need a person, as the outlined button, its border and the coral-ink problem or error text.
- **Do** give each item a title tile (or date chip when it has a date) and one next fact; let the journey line show progress instead of numbers and states.
- **Do** write plain household English; say what happened to the person's film ("In Plex", "Out 14 Nov. Arr will get it then.").
- **Do** keep 44px minimum targets, the 3px ring focus outline, and visible hover/pressed/busy/done states.
- **Do** use Nunito at 500 to 800 and 18px/12px/pill radii as listed.

### Don't:
- **Don't** use coral as a fill, an accent, a badge or decoration; if nothing needs the person, nothing is coral.
- **Don't** add posters, poster grids or backdrops until artwork is sourced; use title tiles. Never fake a poster with a stock image.
- **Don't** put jargon, speeds, seeders, quality, indexer, grab or queue rows, tables, charts or settings on a feed screen.
- **Don't** add a shadow to anything but the search field, and do not apply the drop to rows, buttons or tabs.
- **Don't** use saturated sun yellow (#ffc94d) in the app; dates use the pale sun tint.
- **Don't** set small teal text in Sea Teal on white or cream; use Teal Ink.
- **Don't** introduce a second typeface, light weights, or new radii outside the existing set.
- **Don't** treat `src/wizard/app.css` as this system (see below).

### Known drift (not canonized, not repaired)
- The setup page `src/wizard/app.css` is an older, plainer look: cool grey ground (#f4f4f1), amber accent (#c47f12 / #e5a00d), system sans, square-ish cards. It should be brought into this system later; do not copy its values.
- The website (`site/style.css`) is the brand source but differs by name and a few values: `--accent`/`--surface`/`--warm` for the app's `--teal`/`--card`/`--coral`, a saturated sun, a code-dark panel, a 24px install card with an 8px drop, 20px step cards, and no teal-ink split. The app is the authority for app UI.
- The search field's drop is a hard offset (0 blur) in the line colour, where the direction contract calls it a "soft drop". It is recorded as built and limited to search; future surfaces should not treat a hard offset shadow as a general pattern.
