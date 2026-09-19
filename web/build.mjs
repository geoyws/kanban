/**
 * Build the operator UI bundle into `web/dist`, deterministically.
 *
 * Deterministic is a requirement rather than a nicety: `web/dist` is
 * COMMITTED (see `web/README.md`), the Rust build embeds those exact bytes,
 * and `web/check-reproducible.sh` rebuilds into a temporary directory and
 * `cmp`s every file against the committed one. So nothing here may depend on
 * the clock, the machine, the absolute path of the checkout or the order a
 * directory happens to be read in:
 *
 * - no sourcemap (it carries paths), no banner, no legal comments;
 * - `absWorkingDir` is this directory, so any path esbuild could write is
 *   relative to it;
 * - the asset name carries a content hash computed here, from the output
 *   bytes, rather than by a bundler setting whose hash length or algorithm
 *   could change under us;
 * - the output directory is emptied first, so a stale file cannot survive a
 *   rename and be embedded forever.
 */
import { createHash } from "node:crypto";
import { mkdir, rm, writeFile } from "node:fs/promises";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { build } from "esbuild";

const root = dirname(fileURLToPath(import.meta.url));
// `web/check-reproducible.sh` rebuilds into a temporary directory and
// compares it with the committed one, so the destination is an input. It is
// the ONLY thing about this build a caller may vary.
const dist = process.env.KANBAN_WEB_DIST
  ? resolve(process.env.KANBAN_WEB_DIST)
  : join(root, "dist");

/** The content hash that goes in the file name: 16 hex of sha256. */
function contentHash(bytes) {
  return createHash("sha256").update(bytes).digest("hex").slice(0, 16);
}

function onlyOutput(result, what) {
  if (result.outputFiles.length !== 1) {
    throw new Error(
      `${what}: expected one output file, got ${result.outputFiles.map((file) => file.path).join(", ")}`,
    );
  }
  return result.outputFiles[0].contents;
}

const script = onlyOutput(
  await build({
    absWorkingDir: root,
    entryPoints: ["src/main.tsx"],
    bundle: true,
    format: "esm",
    target: ["es2022"],
    platform: "browser",
    jsx: "automatic",
    minify: true,
    legalComments: "none",
    sourcemap: false,
    define: { "process.env.NODE_ENV": '"production"' },
    write: false,
  }),
  "the script",
);

// The stylesheet is bundled but NOT minified: the token block is read back by
// `rust/serve.rs`'s contrast proofs (SPA-56), which parse declarations, and a
// minifier is free to rewrite a colour into a form that is the same paint and
// a different string. The file is under two kilobytes; there is nothing to
// win here and a proof to lose.
const stylesheet = onlyOutput(
  await build({
    absWorkingDir: root,
    entryPoints: ["src/app.css"],
    bundle: true,
    minify: false,
    legalComments: "none",
    sourcemap: false,
    write: false,
  }),
  "the stylesheet",
);

await rm(dist, { recursive: true, force: true });
await mkdir(dist, { recursive: true });

for (const [extension, bytes] of [
  ["js", script],
  ["css", stylesheet],
]) {
  const name = `app.${contentHash(bytes)}.${extension}`;
  await writeFile(join(dist, name), bytes);
  process.stdout.write(`${join(dist, name)} ${bytes.byteLength} bytes\n`);
}
