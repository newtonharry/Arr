# A TMDB key (optional)

Arr finds movies through your Plex sign-in, so you don't need a TMDB key. TV shows come from
TVmaze, which needs no key either.

A free key from [TMDB](https://www.themoviedb.org) (The Movie Database) is an optional backup.
With one, Arr asks TMDB whenever Plex can't answer, and when both answer it prefers TMDB's
original language, other titles and release dates. If you already added a key, it stays and
keeps working as the backup.

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

- **With the installer:** run the install command again with the key:

  ```sh
  curl -fsSL https://raw.githubusercontent.com/newtonharry/Arr/main/install.sh | sh -s -- --tmdb-key YOUR_KEY
  ```

  On Windows, run `$env:ARR_TMDB_KEY = 'YOUR_KEY'` in PowerShell, then the install line.
- **By hand:** `arr set --config <state folder>/config.toml tmdb-key`, then paste the key when
  it asks (or pipe it in: `printf %s YOUR_KEY | arr set --config ... tmdb-key`). It checks the
  key, saves it and turns the backup on. Then
  [restart Arr](../README.md#restarting-arr).

## For AI agents installing Arr

Don't ask the user for a TMDB key: movies work without one. If the user already has a key and
wants it used as the backup, pass it with `--tmdb-key` (or `ARR_TMDB_KEY`). If TMDB doesn't
accept the key, the installer stops with an `error:` line, and a re-run of an existing install
stops before changing anything. `arr tmdb-key --check` (key on stdin) checks a key without
saving it.

If movie search is reported as not set up, the install isn't connected to Plex: run the install
command again to sign in to Plex.

## If TMDB doesn't accept the key

- Copy the whole value, with nothing before or after it.
- A brand-new key can take a few minutes to start working.
- Check that the key is still listed on <https://www.themoviedb.org/settings/api>.

TMDB's data is provided under its own terms: Arr uses TMDB and the TMDB APIs but is not
endorsed, certified or otherwise approved by TMDB.
