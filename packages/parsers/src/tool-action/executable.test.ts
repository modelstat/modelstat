import { strict as assert } from "node:assert";
import { test } from "node:test";
import { OTHER_BUCKET, extractExecutable } from "./executable.js";

/**
 * Cases drawn from the real `shell.v1` garbage in production ClickHouse — each
 * input is (a redaction of) an actual command whose v1 executable was wrong.
 * `eq(cmd, want)` asserts the normalized executable.
 */
const eq = (cmd: string, want: string) =>
  assert.equal(extractExecutable(cmd), want, JSON.stringify(cmd));

test("cd masking — the real program follows cd (was the #1 garbage, ~52%)", () => {
  eq('cd [REDACTED:abs-path] && NODE_AUTH_TOKEN="" pnpm --filter web exec tsc', "pnpm");
  eq("cd ; git pull --ff-only origin main 2>&1 | tail -1", "git");
  eq("cd [REDACTED:abs-path] && flyctl deploy --app acme-web --remote-only", "flyctl");
  eq("cd ~/.claude/projects/x && jq -r 'select(.type)' file.jsonl", "jq");
  eq("cd \npython3 - <<'PY'\nimport json", "python3");
  eq("cd \nbash -n docker/writer/rehearse.sh && echo ok", "bash");
  eq("cd ./acme-deploys && cargo build --release", "cargo");
});

test("cd masking — multi-line scripts skip cd/source/echo to the real program", () => {
  eq(
    'cd ~/Documents/prism\nsource "$HOME/.cargo/env" 2>/dev/null\necho "=== cargo build ==="\ncargo build',
    "cargo",
  );
  eq("cd \nnohup env SKIP_BASE_BUILD=1 SKIP_FRESH_SNAPSHOT=1 bash deploy/fly/x.sh", "bash");
});

test("a bare cd / echo stays itself (legit single-builtin call)", () => {
  eq("cd ~/Projects/modelstat", "cd");
  eq('echo "hello world"', "echo");
  eq("cd /tmp && cd /var", "cd"); // all cd → fallback cd
});

test("wrapper & keyword heads — peel to the real program", () => {
  eq('eval "$(/opt/homebrew/bin/brew shellenv)" 2>/dev/null\ncd ~/x\ngit add apps/web', "git");
  eq("set -euo pipefail\ncargo build --release", "cargo");
  eq('export PATH="$HOME/.fly/bin:/opt/homebrew/bin:$PATH"\nflyctl deploy', "flyctl");
  eq("for i in 1 2 3; do curl -s http://localhost; done", "curl");
  eq('echo "=== working tree status ==="; git status --short', "git");
  eq("set +e\ncat > /tmp/state.sh <<'EOF'", "cat");
});

test("exec wrappers (sudo/env/time/nohup) peel to the real program", () => {
  eq("sudo systemctl restart nginx", "systemctl");
  eq("time cargo build", "cargo");
  eq("env FOO=bar deploy.sh", "deploy.sh");
  eq("nohup ./server &", "server");
  eq("ls /tmp | xargs rm -f", "ls"); // first program in the pipeline
});

test("env-var assignment prefixes — peel to the program (was high-cardinality noise)", () => {
  eq("AWS_PROFILE=dev terraform -chdir=./x plan -input=false", "terraform");
  eq("GIT_PAGER=cat git log --oneline", "git");
  eq("M=2862d21ce1d618; APP=acme-dash; flyctl machine restart $M -a $APP", "flyctl");
  eq("CHAIN_ID=42220 node scripts/probe.js", "node");
});

test("command-substitution assignments resolve to the inner program", () => {
  eq('WT=$(ssh root@host "uptime")', "ssh");
  eq("TOK=$(cat /tmp/token)", "cat");
  eq("TOKEN=$(gh auth token); curl -H \"Authorization: Bearer $TOKEN\" https://api", "gh");
  eq("B=$(flyctl apps list | grep build)", "flyctl");
});

test("arithmetic assignments are NOT command substitutions (no `seconds+560` junk)", () => {
  eq("END=$((SECONDS+560)); while [ $SECONDS -lt $END ]; do sleep 5; done", "sleep");
  eq("prev=-1; ok=0; flyctl status", "flyctl");
});

test("SECRET LEAK — secrets in assignment prefixes never reach executable", () => {
  const stripe = "sk_live_examplefake0000stripe0000";
  const tg = "1000000000:examplefake0000tgtoken0000";
  const gs = "xk_edge_examplefake0000key0000";

  const a = extractExecutable(`CK="${stripe}"; curl -s https://api.clerk.com/v1/instance`);
  assert.equal(a, "curl");
  assert.ok(!a.includes("sk_live"), "stripe key must not leak into executable");

  const b = extractExecutable(`TOKEN="${tg}"; curl -s "https://api.telegram.org/bot$TOKEN/getMe"`);
  assert.equal(b, "curl");
  assert.ok(!b.includes("1000000000"), "telegram token must not leak");

  // all-assignment script with no program → bucket, secret still gone
  const c = extractExecutable(`SECRET="${gs}"\nBLK="0x23e5910"\nTXH="0xb3101e"`);
  assert.equal(c, OTHER_BUCKET);
  assert.ok(!c.includes("gs_edge"), "secret must not leak even when bucketed");
});

test("comments run to end of line — a `;` inside a comment yields no phantom program", () => {
  eq("# scrub tokenized upstream; point at clean origin\ngit remote set-url origin https://x", "git");
  eq("ls -la # list, then nothing", "ls");
});

test("shell functions, subshells, and brackets peel to the inner program", () => {
  eq("probe() {\n  fly ssh console -a x\n}", "fly");
  eq("raw() { curl -s http://x; }\nraw", "curl");
  eq("(rg -n 'pattern' src)", "rg");
  eq("{ git fetch; git rebase; }", "git");
});

test("paths are basenamed and case-folded", () => {
  eq("/usr/bin/python3 script.py", "python3");
  eq("./deploy.sh --now", "deploy.sh");
  eq("/opt/homebrew/bin/DOCKER ps", "docker");
});

test("unparseable fragments → the generic bucket, never a raw fragment", () => {
  eq('"', OTHER_BUCKET);
  eq("[REDACTED:hi-entropy] --flag", OTHER_BUCKET); // redaction token is not a program
  eq("2>&1", OTHER_BUCKET);
  eq("", OTHER_BUCKET);
});

test("hostnames / data-file fragments are not mistaken for programs", () => {
  eq("acme-svm-test-gs.fly.dev", OTHER_BUCKET); // ≥2 dots ⇒ hostname
  eq("run-b8o4ln0wm.output", OTHER_BUCKET); // data-file extension
  eq("localhost:4012", OTHER_BUCKET); // ':' is not a program char
});

test("clean single programs are unchanged (no regression)", () => {
  eq("git status", "git");
  eq("ssh user@host 'uptime'", "ssh");
  eq("npx prettier --write .", "npx");
  eq("kubectl rollout restart deploy/payments-api -n prod", "kubectl");
  eq("pkill -f modelstat", "pkill");
  eq("grep -rn TODO src", "grep");
});
