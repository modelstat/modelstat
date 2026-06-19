#!/usr/bin/env node
// Decide, with zero human input, which publishable packages need a new release
// on this push to main and what each new version is — from the Conventional
// Commits since each package's last release tag.
//
// Why this exists: every merge to main should publish exactly the packages it
// changed, at a version the commits imply (feat -> minor, fix -> patch,
// !/BREAKING -> major). No release_type input, no OTP, no changeset files.
//
// Design notes:
//   * SOURCE OF TRUTH for "last released version" is the git TAG (agent-v*,
//     mcp-v*, agent-sdk-v*), NOT package.json — package.json has drifted from
//     the tags before, and the tag is what npm actually shipped against.
//   * MONOREPO-AWARE: a package is "affected" if its own dir OR any of its
//     transitive workspace dependencies changed. So a fix in packages/core
//     republishes both `modelstat` and `@modelstat/agent-sdk` (both depend on
//     it) but leaves `@modelstat/mcp` (no workspace deps) alone.
//   * PRE-1.0 CLAMP: while a package is 0.x its API is unstable, so a breaking
//     change bumps the MINOR (0.1.3 -> 0.2.0), never auto-jumping to 1.0.0.
//   * The publishable SET is discovered generically (every workspace package
//     with private !== true), so a new public package is picked up with no
//     edit here. Only the tag-prefix alias below is hand-maintained.
//
// No external deps — Node built-ins + git only. Run from the repo root.
//
//   node .github/scripts/release-plan.mjs
//
// Writes the full plan to release-plan.json (uploaded as an artifact) and
// prints a slim matrix include array to stdout for the release job to fan out.

import { execFileSync } from "node:child_process";
import fs from "node:fs";
import path from "node:path";

// Packages NOT to auto-publish yet. `@modelstat/agent-sdk` is being renamed to
// `@modelstat/daemon-sdk` (chore/agent-to-daemon) — we never ship an
// "agent"-named package (that word is reserved for the user's AI tool), so the
// rename publishes it as daemon-sdk. Drop this entry once the old name is gone.
const SKIP_PUBLISH = new Set(["@modelstat/agent-sdk"]);

// Packages that must build on macOS: the daemon CLI (`modelstat`) bakes a
// universal, ad-hoc-signed ModelstatTray.app into its tarball, which needs full
// Xcode. Everything else is pure JS and builds on ubuntu.
const MACOS_PACKAGES = new Set(["modelstat"]);

function git(args) {
  return execFileSync("git", args, { encoding: "utf8", maxBuffer: 64 * 1024 * 1024 });
}

// Workspace package dirs, read from pnpm-workspace.yaml so a rename (e.g.
// agent-dev -> daemon) needs no change here. Handles the simple quoted-entry
// form this repo uses; expands a trailing /* glob one level if ever added.
function workspaceDirs() {
  const yaml = fs.readFileSync("pnpm-workspace.yaml", "utf8");
  const lines = yaml.split("\n");
  const out = [];
  let inPackages = false;
  for (const line of lines) {
    if (/^packages:\s*$/.test(line)) { inPackages = true; continue; }
    if (inPackages && /^\S/.test(line)) break; // dedent ends the block
    const m = line.match(/^\s*-\s*["']?([^"'\s]+)["']?\s*$/);
    if (inPackages && m) {
      const entry = m[1];
      if (entry.endsWith("/*")) {
        const base = entry.slice(0, -2);
        if (fs.existsSync(base)) {
          for (const d of fs.readdirSync(base)) {
            const full = path.join(base, d);
            if (fs.existsSync(path.join(full, "package.json"))) out.push(full);
          }
        }
      } else {
        out.push(entry);
      }
    }
  }
  return out;
}

// name -> { dir, version, private, deps:Set<workspace dep names> }
function workspaceGraph(dirs) {
  const byName = {};
  const meta = {};
  for (const dir of dirs) {
    const p = path.join(dir, "package.json");
    if (!fs.existsSync(p)) continue;
    const j = JSON.parse(fs.readFileSync(p, "utf8"));
    meta[j.name] = { name: j.name, dir, version: j.version, private: j.private === true, raw: { ...j.dependencies, ...j.devDependencies, ...j.optionalDependencies, ...j.peerDependencies } };
    byName[j.name] = meta[j.name];
  }
  const names = new Set(Object.keys(meta));
  for (const m of Object.values(meta)) {
    m.deps = new Set(Object.entries(m.raw)
      .filter(([dn, dv]) => names.has(dn) && String(dv).startsWith("workspace:"))
      .map(([dn]) => dn));
  }
  return meta;
}

// All dirs whose changes affect `name`: its own dir + every transitive
// workspace dependency's dir.
function affectedDirs(name, graph) {
  const seen = new Set();
  const stack = [name];
  while (stack.length) {
    const n = stack.pop();
    if (seen.has(n) || !graph[n]) continue;
    seen.add(n);
    for (const d of graph[n].deps) stack.push(d);
  }
  return [...seen].map((n) => graph[n].dir);
}

function tagPrefix(name) {
  // Prefix = the package's unscoped name (modelstat -> modelstat-v,
  // @modelstat/mcp -> mcp-v). No aliases: the legacy agent-v* tags are dead, and
  // the modelstat-v lineage is seeded at the same commit as agent-v0.1.3.
  const unscoped = name.includes("/") ? name.split("/")[1] : name;
  return `${unscoped}-v`;
}

const SEMVER = /^(\d+)\.(\d+)\.(\d+)$/;

// Highest released version for a prefix, from tags like `agent-v1.2.3`.
function lastReleased(prefix) {
  let tags = [];
  try {
    tags = git(["tag", "--list", `${prefix}*`]).split("\n").map((s) => s.trim()).filter(Boolean);
  } catch { /* no tags */ }
  let best = null;
  for (const t of tags) {
    const v = t.slice(prefix.length);
    const m = v.match(SEMVER);
    if (!m) continue;
    const tuple = [Number(m[1]), Number(m[2]), Number(m[3])];
    if (!best || cmp(tuple, best.tuple) > 0) best = { tag: t, version: v, tuple };
  }
  return best;
}

function cmp(a, b) {
  for (let i = 0; i < 3; i++) if (a[i] !== b[i]) return a[i] - b[i];
  return 0;
}

// Conventional-commit bump level: 0 none, 1 patch, 2 minor, 3 breaking.
const PATCH_TYPES = new Set(["fix", "perf", "refactor", "revert"]);
function commitLevel(subject, body) {
  const m = subject.match(/^(\w+)(\([^)]*\))?(!)?:/);
  if (m && m[3]) return 3; // type!: ...
  if (/(^|\n)BREAKING[ -]CHANGE/.test(body) || /(^|\n)BREAKING[ -]CHANGE/.test(subject)) return 3;
  if (!m) return 0;
  const type = m[1];
  if (type === "feat") return 2;
  if (PATCH_TYPES.has(type)) return 1;
  return 0; // chore, docs, ci, test, style, build, ...
}

// Bump level + commit list for the range `lastTag..HEAD`, restricted to the
// package's affected dirs.
function analyze(range, dirs) {
  const SEP = "\x1e";
  const UNIT = "\x1f";
  let raw = "";
  try {
    raw = git(["log", range, "--no-merges", `--pretty=format:%H${UNIT}%s${UNIT}%b${SEP}`, "--", ...dirs]);
  } catch {
    return { level: 0, commits: [] };
  }
  let level = 0;
  const commits = [];
  for (const rec of raw.split(SEP)) {
    const r = rec.trim();
    if (!r) continue;
    const [hash, subject = "", body = ""] = r.split(UNIT);
    const l = commitLevel(subject, body);
    if (l > level) level = l;
    commits.push({ hash: hash.slice(0, 8), subject, level: l });
  }
  return { level, commits };
}

function bumpVersion(version, level) {
  const m = version.match(SEMVER);
  if (!m) throw new Error(`not semver: ${version}`);
  let [maj, min, pat] = [Number(m[1]), Number(m[2]), Number(m[3])];
  if (level === 3) {
    if (maj === 0) { min += 1; pat = 0; }      // pre-1.0 clamp: breaking -> minor
    else { maj += 1; min = 0; pat = 0; }
  } else if (level === 2) { min += 1; pat = 0; } // feat -> minor
  else if (level === 1) { pat += 1; }            // fix/perf/refactor -> patch
  return `${maj}.${min}.${pat}`;
}

const LEVEL_NAME = { 0: "none", 1: "patch", 2: "minor", 3: "major" };

// Human-readable release notes from the commits, grouped by kind.
function notes(commits) {
  const feats = [], fixes = [], other = [];
  for (const c of commits) {
    const line = `- ${c.subject}`;
    if (c.level === 2 || c.level === 3) feats.push(line);
    else if (c.level === 1) fixes.push(line);
    else other.push(line);
  }
  const parts = [];
  if (feats.length) parts.push(`### Features\n${feats.join("\n")}`);
  if (fixes.length) parts.push(`### Fixes\n${fixes.join("\n")}`);
  if (other.length) parts.push(`### Other\n${other.join("\n")}`);
  return parts.join("\n\n") || "Maintenance release.";
}

function main() {
  const dirs = workspaceDirs();
  const graph = workspaceGraph(dirs);
  const plan = [];
  const skipped = [];

  for (const pkg of Object.values(graph)) {
    if (pkg.private) continue; // only publishable packages
    if (SKIP_PUBLISH.has(pkg.name)) { skipped.push(pkg.name); continue; }
    const prefix = tagPrefix(pkg.name);
    const last = lastReleased(prefix);
    const dirsFor = affectedDirs(pkg.name, graph);
    const runner = MACOS_PACKAGES.has(pkg.name) ? "macos-14" : "ubuntu-latest";

    if (!last) {
      // Never tagged -> ship the current package.json version as the first
      // release (publish is idempotent, so re-runs are safe).
      plan.push({
        name: pkg.name, dir: pkg.dir, tagPrefix: prefix, runner,
        isDaemon: MACOS_PACKAGES.has(pkg.name),
        fromVersion: null, newVersion: pkg.version, bump: "initial",
        tag: `${prefix}${pkg.version}`,
        notes: `First published release of \`${pkg.name}\`.`,
      });
      continue;
    }

    const { level, commits } = analyze(`${last.tag}..HEAD`, dirsFor);
    if (level === 0) continue; // nothing release-worthy changed
    const newVersion = bumpVersion(last.version, level);
    plan.push({
      name: pkg.name, dir: pkg.dir, tagPrefix: prefix, runner,
      isDaemon: MACOS_PACKAGES.has(pkg.name),
      fromVersion: last.version, newVersion, bump: LEVEL_NAME[level],
      tag: `${prefix}${newVersion}`,
      notes: notes(commits),
    });
  }

  // Full plan -> artifact; slim matrix -> stdout for the release job.
  fs.writeFileSync("release-plan.json", JSON.stringify(plan, null, 2));
  const slim = plan.map((r) => ({
    name: r.name, dir: r.dir, tagPrefix: r.tagPrefix, tag: r.tag,
    runner: r.runner, newVersion: r.newVersion, isDaemon: r.isDaemon, bump: r.bump,
  }));
  process.stdout.write(JSON.stringify(slim));

  if (process.env.GITHUB_STEP_SUMMARY) {
    const md = plan.length
      ? plan.map((r) => `- **${r.name}** ${r.fromVersion ? `${r.fromVersion} → ` : ""}**${r.newVersion}** (${r.bump}) on \`${r.runner}\``).join("\n")
      : "_No publishable package changed — nothing to release._";
    const skip = skipped.length ? `\n\n_Skipped (SKIP_PUBLISH): ${skipped.join(", ")}._` : "";
    fs.appendFileSync(process.env.GITHUB_STEP_SUMMARY, `### Release plan\n${md}${skip}\n`);
  }
}

main();
