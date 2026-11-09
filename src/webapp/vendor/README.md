# Vendored Preact

The web app (`../app.js`) is written with Preact, using its `h` function directly: no JSX, no
htm, no build step. Preact is copied here so the binary stays self-contained and the content
security policy stays `default-src 'self'` (nothing is loaded from a CDN).

- Package: `preact` **11.0.2** (MIT), published 2026-10-23.
- Tarball: <https://registry.npmjs.org/preact/-/preact-11.0.2.tgz>
  (`npm pack preact@11.0.2`; sha256 of the tarball
  `b9dca8d7eb0e758a65fb2f9e41cf284142065b6d6337a3367d15c906a6f892dc`, npm shasum
  `c7ec85ec0d495ef5cdb83a16f9d0f31edae21ace`).

| File here | Taken from the tarball | sha256 of the file as committed |
| --- | --- | --- |
| `preact.mjs` | `package/dist/preact.mjs` | `4d2617f3f082550223d37f781114b5f723362fc4a337459715a7083664dc10c1` |
| `hooks.mjs` | `package/hooks/dist/hooks.mjs` | `135707f109d2f55347b5b5a66ca5e616034a613980d2bffe09d121c47226d107` |
| `LICENSE-preact` | `package/LICENSE` | `1fe6958409c8c257a70c587a18b6f7f412b179b456630790d30b2ec9a8e4b7d4` |

`preact.mjs` and `LICENSE-preact` are byte-for-byte the files in the tarball.

## The one change

`hooks.mjs` starts with `import{options as n}from"preact"`. A browser cannot resolve the bare
specifier `"preact"`, and the content security policy forbids the inline import map that would
normally map it, so that one import path is rewritten to the served file:

```
-import{options as n}from"preact";
+import{options as n}from"/app/vendor/preact.mjs";
```

Nothing else differs from the tarball's `hooks.mjs` (the trailing `sourceMappingURL` comments
of both files are left as published; the maps are not shipped).

## Serving

`src/webapp.rs` embeds the three files and serves them at `/app/vendor/preact.mjs`,
`/app/vendor/hooks.mjs` and `/app/vendor/LICENSE-preact`, with the same headers as `app.js`.
`tests/webapp.rs` checks they are served as JavaScript and match the committed bytes.

## Updating

Unpack the new tarball somewhere outside the repository, copy the same three files, redo the
one import rewrite above, and update the version, the hashes and the date in this file.
