import assert from "node:assert/strict";
import { test } from "node:test";
import type { ToolCallDraft } from "@modelstat/daemon-core/queue";
import type { ToolAction } from "@modelstat/core";
import type { LocalToolContext } from "@modelstat/parsers";
import { defaultRoots, type EnrichScriptsDeps, enrichToolCallScripts } from "./enrich-scripts.js";

/** A wire-shaped ToolAction with everything null/empty except command_redacted. */
function action(command_redacted: string | null): ToolAction {
  return {
    surface: "shell",
    executable: "bash",
    action: null,
    object: null,
    qualifiers: [],
    param_shape: null,
    keywords: [],
    abstract: null,
    command_redacted,
    scripts: [],
    confidence: 0,
    extractor: "shell.v1",
  };
}

/** A draft carrying just the fields enrichment reads. */
function draft(id: string, a: ToolAction | null): ToolCallDraft {
  return { external_call_id: id, action: a } as unknown as ToolCallDraft;
}

function ctx(id: string, command: string, cwd: string | null): LocalToolContext {
  return { external_call_id: id, command, cwd };
}

/** Deps backed by an in-memory filesystem; `summarize` echoes a label by default. */
function deps(
  files: Record<string, string>,
  over: Partial<EnrichScriptsDeps> = {},
): EnrichScriptsDeps & { reads: string[] } {
  const reads: string[] = [];
  return {
    reads,
    exists: (p) => Object.hasOwn(files, p),
    readFile: async (p) => {
      reads.push(p);
      return files[p] ?? "";
    },
    summarize: async ({ ref }) => `does ${ref}`,
    ...over,
  };
}

test("detects + summarises every script in a command, in order, with linkable tokens", async () => {
  const cmd = "bash ./deploy.sh && python scripts/migrate.py";
  const a = action(cmd);
  const d = deps({
    "/repo/deploy.sh": "#!/bin/bash\nkubectl rollout restart deploy/api",
    "/repo/scripts/migrate.py": "import db; db.migrate()",
  });
  await enrichToolCallScripts([draft("c1", a)], [ctx("c1", cmd, "/repo")], d);

  assert.deepEqual(a.scripts, [
    { token: "./deploy.sh", summary: "does ./deploy.sh" },
    { token: "scripts/migrate.py", summary: "does scripts/migrate.py" },
  ]);
  // Every token is a substring of command_redacted → backend can zip deterministically.
  for (const s of a.scripts) assert.ok(a.command_redacted?.includes(s.token));
});

test("resolves the longest / most-absolute existing candidate first", async () => {
  const cmd = "bash build.sh";
  const a = action(cmd);
  // A bare `build.sh` exists both at the repo root and under scripts/; the
  // deeper (longer) path must win.
  const d = deps({
    "/repo/build.sh": "echo root",
    "/repo/scripts/build.sh": "echo scripts",
  });
  await enrichToolCallScripts([draft("c1", a)], [ctx("c1", cmd, "/repo")], d);

  assert.equal(a.scripts.length, 1);
  assert.equal(a.scripts[0]!.token, "build.sh");
  assert.deepEqual(d.reads, ["/repo/scripts/build.sh"]);
});

test("relativises an in-repo absolute path and keeps the token linkable", async () => {
  // The parser would have redacted the home-relative absolute path → ./..., so
  // command_redacted holds the relative form; the token must match THAT, not the
  // raw absolute path.
  const a = action("bash ./scripts/deploy.sh");
  const d = deps(
    { "/Users/alice/repo/scripts/deploy.sh": "echo deploy" },
    { summarize: async () => "deploys the app" },
  );
  await enrichToolCallScripts(
    [draft("c1", a)],
    [ctx("c1", "bash /Users/alice/repo/scripts/deploy.sh", "/Users/alice/repo")],
    d,
  );

  assert.deepEqual(a.scripts, [{ token: "./scripts/deploy.sh", summary: "deploys the app" }]);
});

test("skips a script whose path was masked to a [REDACTED] placeholder", async () => {
  // Out-of-repo absolute path → redact() masks it; there's no stable, unique
  // token to anchor to, so the (potentially sensitive) file is left unsummarised.
  const a = action("bash [REDACTED:abs-path]");
  const d = deps({ "/Users/alice/secret/deploy.sh": "echo secret" });
  await enrichToolCallScripts(
    [draft("c1", a)],
    [ctx("c1", "bash /Users/alice/secret/deploy.sh", "/repo")],
    d,
  );

  assert.deepEqual(a.scripts, []);
  assert.deepEqual(d.reads, []); // never even read it
});

test("does nothing when there is no redacted command to anchor to", async () => {
  const a = action(null);
  const d = deps({ "/repo/x.sh": "echo x" });
  await enrichToolCallScripts([draft("c1", a)], [ctx("c1", "bash ./x.sh", "/repo")], d);
  assert.deepEqual(a.scripts, []);
});

test("dedupes repeated refs and caps at the wire limit of 8", async () => {
  const refs = Array.from({ length: 12 }, (_, i) => `./s${i}.sh`);
  const cmd = `${refs.join(" && ")} && ./s0.sh`; // s0 repeated
  const a = action(cmd);
  const files: Record<string, string> = {};
  for (const r of refs) files[`/repo/${r.slice(2)}`] = `echo ${r}`;
  await enrichToolCallScripts([draft("c1", a)], [ctx("c1", cmd, "/repo")], deps(files));

  assert.equal(a.scripts.length, 8); // capped
  assert.equal(new Set(a.scripts.map((s) => s.token)).size, 8); // all distinct
});

test("best-effort: null summaries, unreadable files, and missing context never throw", async () => {
  const a1 = action("bash ./a.sh"); // summarize → null
  const a2 = action("bash ./b.sh"); // readFile throws
  const a3 = action("bash ./c.sh"); // no matching context
  const a4 = action("bash ./d.sh"); // action present, file missing on disk
  const aNull = draft("c0", null); // action is null — left untouched

  const d: EnrichScriptsDeps = {
    // Allowlist: only a.sh and b.sh exist; d.sh is missing entirely.
    exists: (p) => p === "/repo/a.sh" || p === "/repo/b.sh",
    readFile: async (p) => {
      if (p === "/repo/b.sh") throw new Error("EACCES");
      return "echo hi";
    },
    summarize: async ({ ref }) => (ref === "./a.sh" ? null : `does ${ref}`),
  };

  await assert.doesNotReject(
    enrichToolCallScripts(
      [aNull, draft("c1", a1), draft("c2", a2), draft("c3", a3), draft("c4", a4)],
      [
        ctx("c1", "bash ./a.sh", "/repo"),
        ctx("c2", "bash ./b.sh", "/repo"),
        ctx("c4", "bash ./d.sh", "/repo"),
      ],
      d,
    ),
  );
  assert.deepEqual(a1.scripts, []); // null summary skipped
  assert.deepEqual(a2.scripts, []); // unreadable skipped
  assert.deepEqual(a3.scripts, []); // no context skipped
  assert.deepEqual(a4.scripts, []); // missing file skipped
});

test("NO LEAK: a secret in script CONTENT never survives into the shipped summary", async () => {
  const secret = "sk-ant-api03-AAAABBBBCCCCDDDDEEEEFFFFGGGGHHHHIIIIJJJJ";
  const cmd = "bash ./deploy.sh";
  const a = action(cmd);
  const d = deps(
    { "/repo/deploy.sh": `#!/bin/bash\nexport ANTHROPIC_API_KEY=${secret}\ncurl ...` },
    // Worst case: the model echoes the file contents verbatim, secret and all.
    { summarize: async ({ content }) => content },
  );
  await enrichToolCallScripts([draft("c1", a)], [ctx("c1", cmd, "/repo")], d);

  assert.equal(a.scripts.length, 1);
  assert.ok(!a.scripts[0]!.summary.includes(secret), "secret leaked into summary");
  assert.ok(a.scripts[0]!.summary.includes("[REDACTED"), "summary should be redacted");
});

test("NO LEAK: the raw command is never copied onto the draft", async () => {
  // The script IS summarised (path relativises against cwd), yet the raw
  // absolute home path must never end up serialised on the draft.
  const rawSecretPath = "/Users/alice/private/run.sh";
  const a = action("bash ./run.sh");
  const d = draft("c1", a);
  await enrichToolCallScripts(
    [d],
    [ctx("c1", `bash ${rawSecretPath}`, "/Users/alice/private")],
    deps({ [rawSecretPath]: "echo hi" }, { summarize: async () => "runs the thing" }),
  );
  assert.deepEqual(a.scripts, [{ token: "./run.sh", summary: "runs the thing" }]);
  const serialised = JSON.stringify(d);
  assert.ok(!serialised.includes(rawSecretPath), "raw absolute path leaked onto draft");
  assert.ok(!serialised.includes("/Users/"), "a /Users/ path leaked onto draft");
});

test("defaultRoots is liberal: cwd, ancestors, and conventional script subdirs", () => {
  const roots = defaultRoots("/a/b/c/d");
  assert.ok(roots.includes("/a/b/c/d")); // cwd
  assert.ok(roots.includes("/a/b/c")); // ancestor
  assert.ok(roots.includes("/a/b/c/d/scripts")); // subdir
  assert.ok(roots.includes("/a/b/c/d/.github/scripts"));
  assert.deepEqual(defaultRoots(null), []);
});
