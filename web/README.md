# The operator UI bundle

The browser half of `kanban serve`, as one built artefact that the Rust binary
embeds. Built here, committed here, served from the executable's own bytes
(ADR-048 §1, `docs/specs/spa.md` SPA-01, SPA-02).

## What this is, in four files

| Path | What it is |
| --- | --- |
| `src/main.tsx` | the entry point: mounts `App` into the shell's `#root` |
| `src/app.tsx` | the application root — `<main data-testid="app-root">` with the live line and the `Needs you` heading, and nothing else in the first wave |
| `src/app.css` | the stylesheet, whose `:root` block is ADR-046's token block byte for byte |
| `build.mjs` | the deterministic build into `dist/` |

`dist/` is **committed**. `bun install` writes `node_modules/`, which is not.

## The decisions, and why (OQ-1, resolved 2026-09-19)

**React 19.2.0, TypeScript 5.9.3 in `strict` mode.** React because ADR-048
chose it and the deck is a client-state problem — a card in flight, a draft
reply, a projection swap under the reader — that a template renderer makes
harder, not easier. 19.x because it is the current line and `createRoot` is
the API the mount seam (`wait_for_app_root`) was designed against. TypeScript
with `strict`, `noUncheckedIndexedAccess` and `exactOptionalPropertyTypes`
because the projection the client will read (`t-88814b7a`) is a wire format,
and a wire format with optional fields is exactly what an unchecked index
gets wrong.

**esbuild 0.25.12 as the bundler, run by Bun.** The requirement that decides
this is not speed, it is REPRODUCIBILITY: `dist/` is committed, so the build
has to produce the same bytes on the gate host as it did on the machine that
committed them. esbuild is one pinned dependency in `bun.lock` with no plugin
tree and no config DSL, so "the same inputs" is a checkable claim. Bun's own
bundler was the obvious alternative and was rejected for one reason: its
version is the version of whatever `bun` is on `PATH`, which the lockfile
does not pin, so a bun upgrade would change the artefact without changing a
single tracked file. Bun stays as the package manager and script runner,
where its version cannot reach the output.

**Biome 2.3.14 as the linter, alongside `tsc --noEmit`.** The type check is
the first half of the lint: with `strict` on, most of what a type-aware
ESLint ruleset is installed for is already a compile error. What is left is
correctness-adjacent lint and formatting, and Biome does both in one pinned
binary. The ESLint alternative is four packages (`eslint`,
`@typescript-eslint/parser`, `@typescript-eslint/eslint-plugin`, a config)
plus a second formatter, for rules this tree does not have a use for yet.

**The bundle is committed and the Rust build embeds it.** The release is
compiled in a pinned Rust container with no Node in it, and the serving hosts
(`hax`, `hig`) have no Node-shaped runtime at all and must not need one
(ADR-047's `r-cf2b2b9f`, SPA-02). So the release path stays cargo-only and
offline (ADR-044): `cargo build --release` must succeed with `node` and `bun`
nowhere on `PATH`. The only way to have a bundled UI under that constraint is
for the built bytes to be IN the repository, which is what `dist/` is.
`build.rs` reads `web/dist`, writes the `include_bytes!` table, checks each
file's bytes against the content hash in its name, and stamps the bundle
fingerprint into the binary.

**And therefore: the gate proves the committed artefact.** A committed build
output is only worth what the check on it is worth, so `check-reproducible.sh`
installs from the frozen lockfile, rebuilds into a temporary directory and
`cmp`s every file both ways. Determinism is a build-script obligation, not a
hope: no sourcemaps, no banners, no legal comments, no timestamps, the output
directory emptied first, and the content hash computed here from the output
bytes rather than by a bundler setting whose algorithm could change.

## Working on it

```sh
bun install            # node_modules, from the frozen lockfile
bun run build          # rebuild dist/ -- COMMIT the result
./gate.sh              # typecheck + lint + reproducibility, the web tree's own gate
./check-reproducible.sh
```

A change to `src/` that is not followed by `bun run build` and a commit of
`dist/` fails `check-reproducible.sh`, which is the point.

## What the binary does with it

`GET /app` returns the shell — a document holding `#root`
(`data-testid=app-root-shell`), the stylesheet link and the module script, and
one literal `</head>` because the hax edge injects WebMCP with nginx
`sub_filter '</head>'`. `GET /assets/<name>` serves the embedded bytes with
the extension's `Content-Type` and
`Cache-Control: public, max-age=31536000, immutable`; the name carries the
content hash, so a name the binary does not have is a 404 and never a
filesystem read. `kanban --version` prints the bundle fingerprint as its
second line, which is what the release receipt records as `bundleSha256` and
what the installer compares the installed binary against.
