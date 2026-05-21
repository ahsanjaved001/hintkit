#!/usr/bin/env node
//
// Build-time spec ingestion (SPEC §7 Phase 4).
//
// Reads `withfig/autocomplete`'s compiled JS specs from
// node_modules/@withfig/autocomplete/build/, translates each curated
// spec into our `.hintkitspec.json` schema, and writes the result to
// crates/hintkit-specs-bundled/data/ where SpecDb picks it up at the
// next `cargo build`.
//
// Critical safety property: this script NEVER evaluates or executes
// any generator function from a Fig spec. Generators in Fig specs are
// arbitrary JS that often shells out (`make -qp`, `git branch`, etc.)
// — exactly what SPEC §3 commitment #2 forbids from running at the
// runtime hot path without explicit allowlisting. The translator drops
// every `generators` field and only honors a small allowlist of
// `template` strings (filepaths → file_path, folders → dir_path).
// Untranslated dynamic completions become empty for v0.1; Phase 5 will
// wire up our own native generators (git_branches via `git branch`,
// package_json_scripts via reading package.json, etc.) and bind them
// to specific spec contexts via a manual mapping.

import { promises as fs } from "node:fs";
import path from "node:path";
import url from "node:url";

const __dirname = path.dirname(url.fileURLToPath(import.meta.url));

const FIG_BUILD = path.join(
  __dirname,
  "node_modules",
  "@withfig",
  "autocomplete",
  "build",
);
const OUTPUT_DIR = path.resolve(
  __dirname,
  "..",
  "..",
  "crates",
  "hintkit-specs-bundled",
  "data",
);

// Templates fig uses for common completion kinds. Map only the ones our
// runtime knows how to execute safely (file/dir path enumeration is
// pure local-filesystem read; nothing else is allowlisted).
const TEMPLATE_TO_GENERATOR = {
  filepaths: "file_path",
  folders: "dir_path",
};

// Fig uses custom JS generators for dynamic completions like "list git
// branches" or "list package.json scripts" — we drop those at ingest
// time because we can't safely evaluate arbitrary JS. But the *concept*
// of "this arg slot completes to local branches" is something our
// native generators implement, so we re-attach them here by spec name +
// subcommand + arg index. Manually curated, narrow, auditable.
//
// Path format: ["subcommand", "subcommand", …, argIndex] or just
// [argIndex] for a top-level arg. The last entry is the integer arg
// index inside the resolved CommandSpec.
const KNOWN_GENERATOR_BINDINGS = {
  git: [
    { path: ["checkout", 0], generator: "git_branches" },
    { path: ["switch", 0], generator: "git_branches" },
    { path: ["branch", 0], generator: "git_branches" },
    { path: ["merge", 0], generator: "git_branches" },
    { path: ["rebase", 0], generator: "git_branches" },
  ],
  npm: [{ path: ["run", 0], generator: "package_json_scripts" }],
  pnpm: [{ path: ["run", 0], generator: "package_json_scripts" }],
  bun: [{ path: ["run", 0], generator: "package_json_scripts" }],
  // yarn invokes scripts as a top-level positional: `yarn <script>`.
  yarn: [{ path: [0], generator: "package_json_scripts" }],
};

// Curated v0.1 spec set (SPEC §7 Phase 4 step 4). Mostly the SPEC
// recommendation verbatim; deviations get a one-line "why" comment
// next to the entry.
const CURATED = [
  // Source control + remote work
  "git",
  "gh",
  "ssh",
  "scp",
  // Build tools
  "make",
  "just",
  "cargo",
  // Package managers / runtimes
  "npm",
  "yarn",
  "pnpm",
  "bun",
  "node",
  "python",
  "pip",
  // Network
  "curl",
  "wget",
  // Cloud / infrastructure
  "docker",
  "kubectl",
  "aws",
  "terraform",
  // Search / filesystem (replacements + classics)
  "find",
  "grep",
  "rg",
  "fd",
  // POSIX file basics
  "tar",
  "ls",
  "cp",
  "mv",
  "rm",
  // `cd` is a shell builtin (no executable) — included anyway since
  // fig ships a spec and our completion engine still matches the
  // typed command name, not an on-disk binary.
  "cd",
];

class IngestError extends Error {
  constructor(specName, message) {
    super(`[${specName}] ${message}`);
    this.spec = specName;
  }
}

function asArray(x) {
  if (x === undefined || x === null) return [];
  return Array.isArray(x) ? x : [x];
}

function translateArg(a, specName) {
  if (typeof a !== "object" || a === null) {
    throw new IngestError(
      specName,
      `arg is not an object: ${JSON.stringify(a)}`,
    );
  }
  const out = {};
  // Fig `name` can be missing on args that are positionals defined only
  // by their position; default to a generic placeholder.
  out.name = String(a.name ?? "value");
  if (a.description) out.description = String(a.description);

  if (a.template) {
    const templates = asArray(a.template);
    for (const t of templates) {
      if (TEMPLATE_TO_GENERATOR[t]) {
        out.generator = TEMPLATE_TO_GENERATOR[t];
        break;
      }
      // Unknown template kind → no generator (silently drop, common case).
    }
  }
  // `generators` field is intentionally not honored — see header.
  return out;
}

function translateOption(o, specName) {
  const names = asArray(o.name).map(String).filter(Boolean);
  if (names.length === 0) {
    throw new IngestError(specName, "option has no name");
  }
  const out = { names };
  if (o.description) out.description = String(o.description);
  const args = asArray(o.args).map((a) => translateArg(a, specName));
  if (args.length > 0) out.args = args;
  return out;
}

function translateCommand(spec, specName) {
  const names = asArray(spec.name).map(String).filter(Boolean);
  if (names.length === 0) {
    throw new IngestError(specName, "spec has no name");
  }
  const out = { name: names[0] };
  if (spec.description) out.description = String(spec.description);
  if (spec.subcommands) {
    const subs = asArray(spec.subcommands).map((sc) =>
      translateCommand(sc, specName),
    );
    if (subs.length > 0) out.subcommands = subs;
  }
  if (spec.options) {
    const opts = asArray(spec.options).map((o) => translateOption(o, specName));
    if (opts.length > 0) out.options = opts;
  }
  if (spec.args) {
    const args = asArray(spec.args).map((a) => translateArg(a, specName));
    if (args.length > 0) out.args = args;
  }
  return out;
}

async function ingestOne(cmd) {
  const specPath = path.join(FIG_BUILD, `${cmd}.js`);
  try {
    await fs.access(specPath);
  } catch {
    throw new IngestError(cmd, `no built spec at ${specPath}`);
  }

  const mod = await import(url.pathToFileURL(specPath).href);
  const spec = mod.default;
  if (!spec) {
    throw new IngestError(cmd, "no default export");
  }

  const translated = translateCommand(spec, cmd);
  // Pin the canonical name to the lookup key (some fig specs declare
  // aliases first; the SpecDb filename has to match exactly).
  translated.name = cmd;

  applyKnownGeneratorBindings(translated, cmd);

  const outPath = path.join(OUTPUT_DIR, `${cmd}.hintkitspec.json`);
  await fs.writeFile(outPath, JSON.stringify(translated, null, 2) + "\n");
  console.log(`  ${cmd} → ${path.relative(process.cwd(), outPath)}`);
}

function applyKnownGeneratorBindings(spec, cmd) {
  const bindings = KNOWN_GENERATOR_BINDINGS[cmd];
  if (!bindings) return;
  for (const { path: bindingPath, generator } of bindings) {
    if (!attachGenerator(spec, bindingPath, generator)) {
      console.warn(
        `  WARN: ${cmd} binding path ${JSON.stringify(bindingPath)} did not match — upstream spec may have changed`,
      );
    }
  }
}

/// Walk `spec` along the binding path (subcommand names then a final
/// arg index), set `.generator` on the arg at the terminus. Returns
/// `true` if the binding succeeded, `false` if the path didn't resolve.
function attachGenerator(spec, bindingPath, generator) {
  let cursor = spec;
  for (let i = 0; i < bindingPath.length - 1; i++) {
    const subName = bindingPath[i];
    if (!cursor.subcommands) return false;
    const sub = cursor.subcommands.find((s) => s.name === subName);
    if (!sub) return false;
    cursor = sub;
  }
  const argIndex = bindingPath[bindingPath.length - 1];
  if (!cursor.args || !cursor.args[argIndex]) return false;
  cursor.args[argIndex].generator = generator;
  return true;
}

async function main() {
  await fs.mkdir(OUTPUT_DIR, { recursive: true });
  console.log(
    `Ingesting ${CURATED.length} spec${CURATED.length === 1 ? "" : "s"} from withfig/autocomplete`,
  );
  let ok = 0;
  let failed = 0;
  for (const cmd of CURATED) {
    try {
      await ingestOne(cmd);
      ok++;
    } catch (e) {
      console.error(`  FAILED: ${e.message}`);
      failed++;
    }
  }
  console.log(
    `\n${ok} ingested, ${failed} failed${failed > 0 ? " — see errors above" : ""}`,
  );
  if (failed > 0) process.exit(1);
}

main().catch((e) => {
  console.error(e);
  process.exit(1);
});
