import { strict as assert } from "node:assert";
import { test } from "node:test";
import { extractToolAction } from "./index.js";

test("shell command → executable + masked param_shape + redacted command", () => {
  const a = extractToolAction({
    server: "builtin",
    name: "Bash",
    input: { command: "kubectl rollout restart deploy/payments-api -n prod" },
    cwd: "/repo",
  });
  assert.equal(a.surface, "shell");
  assert.equal(a.executable, "kubectl");
  // param_shape masks the ARGS (command minus the leading program).
  assert.equal(a.param_shape, "§ § § -n §");
  // Infra names are not secrets → the redacted command keeps them.
  assert.ok(a.command_redacted?.includes("kubectl"));
  assert.ok(a.command_redacted?.includes("prod"));
  // SPEC 0004: the wire is structural-only — no semantic fields at all; the
  // server derives the whole operation frame from command_redacted.
  assert.ok(!("action" in a), "no semantic fields on the wire");
  assert.equal(a.extractor, "shell.v3");
});

test("script invocation → basename is the executable", () => {
  const a = extractToolAction({ server: "builtin", name: "Bash", input: { command: "./deploy.sh --now" } });
  assert.equal(a.surface, "shell");
  assert.equal(a.executable, "deploy.sh");
  assert.equal(a.param_shape, "--now");
});

test("mcp tool → surface mcp, executable = operation, no command", () => {
  const a = extractToolAction({ server: "mcp:github", name: "create_pr", input: { title: "x" } });
  assert.equal(a.surface, "mcp");
  assert.equal(a.executable, "create_pr");
  assert.equal(a.command_redacted, null);
  assert.equal(a.param_shape, null);
  assert.equal(a.extractor, "mcp.v1");
});

test("builtin tool → surface builtin, executable = name", () => {
  const a = extractToolAction({ server: "builtin", name: "Read", input: { file_path: "/x.ts" } });
  assert.equal(a.surface, "builtin");
  assert.equal(a.executable, "Read");
  assert.equal(a.command_redacted, null);
  assert.equal(a.extractor, "builtin.v1");
});

test("codex argv command (action.command array form) is handled", () => {
  const a = extractToolAction({
    server: "builtin",
    name: "shell",
    input: { command: ["git", "commit", "-m", "fix"] },
  });
  assert.equal(a.surface, "shell");
  assert.equal(a.executable, "git");
});

test("secrets are stripped from the redacted command (no-leak guarantee)", () => {
  const secret = "Bearer aB3xZ9qLmN7pQ2rT5vW8yC1dF4gH6jK0sUvWxYz";
  const a = extractToolAction({
    server: "builtin",
    name: "Bash",
    input: { command: `curl -H 'Authorization: ${secret}' https://api.example.com/v1/deploy` },
  });
  assert.ok(a.command_redacted, "a redacted command is produced");
  assert.ok(
    !a.command_redacted.includes("aB3xZ9qLmN7pQ2rT5vW8yC1dF4gH6jK0sUvWxYz"),
    "the secret token must be redacted out of command_redacted",
  );
  // The non-sensitive structure survives.
  assert.equal(a.executable, "curl");
});
