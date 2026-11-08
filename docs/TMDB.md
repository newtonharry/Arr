# Getting a TMDB key

Arr finds TV shows through TVmaze, which needs no key. To find **movies you don't own yet**,
it needs a free key from [TMDB](https://www.themoviedb.org) (The Movie Database), which tells
Arr each movie's exact title, year, other titles, runtime and release dates. Without a key,
everything else works: your Plex library, TV shows, and the movies you already have.

It takes about five minutes, on a computer rather than a phone (TMDB's form isn't made for
phones).

## Steps

1. **Create a free account** at <https://www.themoviedb.org/signup> (Google, Apple or email),
   and confirm your email address if TMDB asks you to.
2. **Request a key** on your account's API page, <https://www.themoviedb.org/settings/api>.
   Accept TMDB's terms. If the form asks about your application, it's personal, non-commercial
   use; these answers are fine:
   - Application name: `Arr`
   - Application URL: `https://github.com/newtonharry/Arr`
   - Application summary: `Personal media server that finds movies for my Plex library`
3. **Copy the key.** The API page shows an **API Read Access Token** (long) and an **API Key**
   (short). Either works.

Arr checks every key with TMDB before saving it, so a mistyped key is caught straight away.

## Adding it to Arr

- **During setup:** paste it into the TMDB box on the setup page, or answer the installer's
  question in a terminal install.
- **Later**, after Arr is set up: run the install command again. If movie search is still off,
  it asks for the key. To give it straight away:

  ```sh
  curl -fsSL https://raw.githubusercontent.com/newtonharry/Arr/main/install.sh | sh -s -- --tmdb-key YOUR_KEY
  ```

  On Windows, run `$env:ARR_TMDB_KEY = 'YOUR_KEY'` in PowerShell, then the install line.
- **By hand:** `printf %s YOUR_KEY | arr tmdb-key --config <state folder>/config.toml`, then
  restart Arr. It checks the key, saves it and turns movie search on.

## For AI agents installing Arr

Ask the user for the key (it needs their TMDB login, so they create it themselves, following
the steps above), then pass it with `--tmdb-key` (or `ARR_TMDB_KEY`). A fresh install stops
with an `error:` line if TMDB doesn't accept the key; a re-run of an existing install warns and
carries on without it. `arr tmdb-key --check` (key on stdin) checks a key without saving it.

When movie search isn't set up, Arr's answers to your assistant say so and point here, so the
assistant can walk the user through these steps.

## If TMDB doesn't accept the key

- Copy the whole value, with nothing before or after it.
- A brand-new key can take a few minutes to start working.
- Check that the key is still listed on <https://www.themoviedb.org/settings/api>.

TMDB's data is provided under its own terms: Arr uses TMDB and the TMDB APIs but is not
endorsed, certified or otherwise approved by TMDB.
