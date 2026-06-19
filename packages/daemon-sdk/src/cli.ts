#!/usr/bin/env node
/**
 * modelstat-daemon CLI — sanitisation pipelines on the command line.
 *
 *   modelstat-daemon redact <file>                 # reads JSON, prints redacted JSON to stdout
 *   modelstat-daemon compact <file>                # reads JSON, prints compacted JSON
 *   modelstat-daemon pipe <file>                   # redact + compact together
 *   modelstat-daemon stats <file>                  # show what would change, don't transform
 *
 * Flags:
 *   --policy <name>          one of: none, secrets-only, strict-pii-v2, paranoid
 *   --max-string <bytes>     truncation cap for generic strings (default 8192)
 *   --max-tool-output <b>    truncation cap for tool stdout/stderr (default 4096)
 *   --no-collapse-repeats    don't collapse runs of identical tool calls
 *   --no-drop-blobs          don't drop binary-looking base64 blobs
 *   --pretty                 pretty-print JSON output
 *
 * Reads from stdin if <file> is "-".
 */
import { readFile } from "node:fs/promises";
import { redact, type PolicyName, POLICY_VERSIONS } from "./redact.js";
import { compact, DEFAULT_COMPACT, type CompactOptions } from "./compact.js";
import { pipe } from "./index.js";

type Args = {
  command: string;
  file: string;
  policy: PolicyName;
  compactOpts: Partial<CompactOptions>;
  pretty: boolean;
};

const HELP = `\
modelstat-daemon — privacy-first session sanitisation

USAGE
  modelstat-daemon redact   <file>     redact PII + secrets
  modelstat-daemon compact  <file>     truncate large fields, drop blobs
  modelstat-daemon pipe     <file>     redact + compact together
  modelstat-daemon stats    <file>     show what would change without transforming
  modelstat-daemon policies            list available redaction policies

  Use "-" for <file> to read from stdin.

FLAGS
  --policy <name>            none | secrets-only | strict-pii-v2 (default) | paranoid
  --max-string <bytes>       truncation cap for generic strings (default 8192)
  --max-tool-output <bytes>  truncation cap for stdout/stderr (default 4096)
  --no-collapse-repeats      don't collapse runs of identical tool calls
  --no-drop-blobs            don't drop binary-looking base64 blobs
  --pretty                   pretty-print JSON output

EXAMPLES
  cat session.json | modelstat-daemon pipe - --pretty > clean.json
  modelstat-daemon stats session.json
  modelstat-daemon redact session.json --policy paranoid > redacted.json
`;

function parseArgs(argv: readonly string[]): Args {
  const args: Args = {
    command: argv[0] ?? "",
    file: argv[1] ?? "",
    policy: "strict-pii-v2",
    compactOpts: {},
    pretty: false,
  };
  for (let i = 2; i < argv.length; i++) {
    const a = argv[i];
    switch (a) {
      case "--policy":
        args.policy = (argv[++i] ?? "strict-pii-v2") as PolicyName;
        break;
      case "--max-string":
        args.compactOpts.maxStringLength = Number.parseInt(argv[++i] ?? "8192", 10);
        break;
      case "--max-tool-output":
        args.compactOpts.maxToolOutput = Number.parseInt(argv[++i] ?? "4096", 10);
        break;
      case "--no-collapse-repeats":
        args.compactOpts.collapseRepeats = false;
        break;
      case "--no-drop-blobs":
        args.compactOpts.dropBinaryBlobs = false;
        break;
      case "--pretty":
        args.pretty = true;
        break;
      default:
        if (a?.startsWith("--")) {
          process.stderr.write(`unknown flag: ${a}\n${HELP}`);
          process.exit(2);
        }
    }
  }
  return args;
}

async function readInput(file: string): Promise<unknown> {
  let text: string;
  if (file === "-") {
    const chunks: Buffer[] = [];
    for await (const c of process.stdin) chunks.push(c as Buffer);
    text = Buffer.concat(chunks).toString("utf8");
  } else {
    text = await readFile(file, "utf8");
  }
  try {
    return JSON.parse(text);
  } catch (e) {
    process.stderr.write(`failed to parse JSON from ${file}: ${(e as Error).message}\n`);
    process.exit(1);
  }
}

function emit(data: unknown, pretty: boolean): void {
  process.stdout.write(pretty ? `${JSON.stringify(data, null, 2)}\n` : `${JSON.stringify(data)}\n`);
}

async function main(): Promise<void> {
  const argv = process.argv.slice(2);
  if (argv.length === 0 || argv[0] === "--help" || argv[0] === "-h") {
    process.stdout.write(HELP);
    return;
  }

  if (argv[0] === "policies") {
    const policies = Object.entries(POLICY_VERSIONS).map(([name, ver]) => `  ${name.padEnd(14)} v${ver}`);
    process.stdout.write(`available policies:\n${policies.join("\n")}\n`);
    return;
  }

  const args = parseArgs(argv);
  if (!args.file) {
    process.stderr.write(`${HELP}`);
    process.exit(2);
  }

  const input = await readInput(args.file);

  switch (args.command) {
    case "redact": {
      const r = redact(input, args.policy);
      process.stderr.write(`✓ ${r.redactionsApplied} redactions applied (${r.policy} v${r.policyVersion})\n`);
      emit(r.data, args.pretty);
      return;
    }
    case "compact": {
      const c = compact(input, { ...DEFAULT_COMPACT, ...args.compactOpts });
      process.stderr.write(`✓ ${c.changesApplied} changes, ${c.bytesSaved} bytes saved\n`);
      emit(c.data, args.pretty);
      return;
    }
    case "pipe": {
      const p = pipe(input, { policy: args.policy, compact: args.compactOpts });
      process.stderr.write(
        `✓ ${p.processing.redactions_applied} redactions, ${p.processing.changes_applied} compactions, ` +
          `${p.processing.original_size_bytes - p.processing.uploaded_size_bytes} bytes saved\n`,
      );
      emit({ data: p.data, processing: p.processing }, args.pretty);
      return;
    }
    case "stats": {
      const r = redact(input, args.policy);
      const c = compact(r.data, { ...DEFAULT_COMPACT, ...args.compactOpts });
      const before = JSON.stringify(input).length;
      const after = JSON.stringify(c.data).length;
      process.stdout.write(
        [
          `policy:               ${r.policy} (v${r.policyVersion})`,
          `redactions:           ${r.redactionsApplied}`,
          `compaction changes:   ${c.changesApplied}`,
          `original size:        ${before} bytes`,
          `cleaned size:         ${after} bytes`,
          `reduction:            ${((1 - after / before) * 100).toFixed(1)}%`,
        ].join("\n") + "\n",
      );
      return;
    }
    default:
      process.stderr.write(`unknown command: ${args.command}\n${HELP}`);
      process.exit(2);
  }
}

main().catch((e) => {
  process.stderr.write(`modelstat-daemon: ${(e as Error).message}\n`);
  process.exit(1);
});
