# Privacy and error reports

Arr runs on your computer and talks to Plex, your torrent sources and, only if you add a key, TMDB.
Movie searches and lookups go to Plex's metadata service with your Plex sign-in; TV searches go to
TVmaze; a movie's details go to TMDB only if you've added a TMDB key. It doesn't send
anything about you or your library to its developer, with one optional exception: error reports.

## Error reports are off unless you turn them on

Setup asks once: the tick-box "Send error reports to help fix Arr" on the setup page, or the
question "Send error reports to help fix Arr?" in a terminal install. It starts unticked, and the
answer is saved in Arr's config as:

```toml
[reporting]
enabled = true
```

Without that section, Arr sends nothing. An agent install (`--yes`) leaves it off unless it
also passes `--error-reports`. On Windows, `$env:ARR_ERROR_REPORTS = '1'` before the install
line starts the setup page with the box ticked, and on an existing install turns reports on
(`'0'` turns them off).

Reports leave out other programs' output and the Plex import's own error details, since those
can name titles and folders; you still see them on the setup page.

## What a report contains

A report is sent only when something stops working:

- setup fails, or installing Plex fails;
- the installer stops with an error;
- an automatic update doesn't start and Arr goes back to the old version;
- Arr itself stops with an error, or crashes.

Each report holds:

- the error message, with the parts below removed;
- which part of Arr it came from (`setup`, `installer`, `server`, `updater`, ...);
- Arr's version, the operating system and its version, and the Rust version Arr was built with;
- for a crash, where in Arr's code it happened.

## What's removed before it leaves your computer

Arr cleans every report itself, before sending it:

- web addresses, email addresses, IP addresses and host names;
- folder and file paths (Windows, macOS and Linux), so no user name or folder name;
- anything in quotes, which is where titles and file names appear in Arr's messages;
- passwords, tokens, keys, cookies and sign-in codes, and any long run of letters and digits
  that could be one;
- the computer's name, the user, and anything Arr was doing before the error.

Reports never contain your config, your Plex token, your library, what you've searched for or
downloaded, or your logs. Messages are cut off after 2,000 characters. The cleaning rules are in
[`src/report.rs`](../src/report.rs), with tests that check them.

## Where reports go

To Arr's project on [Sentry](https://sentry.io), stored in Sentry's EU region (Germany). Only
Arr's developer can read them.

Arr doesn't put an IP address in a report and tells Sentry not to add personal data, but Sentry
sees the address a report comes from, as any website does.

## Turning reports off (or on) later

Run `arr set` with Arr's config (`config.toml` in Arr's state folder; the installer printed its
path as `Config:`):

```sh
arr set --config <state folder>/config.toml error-reports off   # or: on
```

It sets `enabled` under `[reporting]`. Then restart Arr: `sudo systemctl restart arr` (Linux,
installed with `sudo`), `systemctl --user restart arr` (Linux, installed without),
`launchctl kickstart -k gui/$(id -u)/io.github.newtonharry.arr` (macOS), or
`Stop-ScheduledTask Arr; Start-ScheduledTask Arr` in PowerShell (Windows). Running the install
command again with `--error-reports` or `--no-error-reports` (on Windows,
`$env:ARR_ERROR_REPORTS = '1'` or `'0'`) does both steps.

## Sending reports somewhere else

Set `ARR_SENTRY_DSN` in Arr's environment to send reports to your own Sentry or
[GlitchTip](https://glitchtip.com) server instead. Setting it to an empty value turns reporting
off completely, whatever the config says, and setup won't ask.
