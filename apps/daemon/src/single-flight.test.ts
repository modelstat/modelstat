import { strict as assert } from "node:assert";
import { test } from "node:test";
import { createCoalescingRunner } from "./single-flight.js";

/** A promise plus its resolver, so a test can hold a run open and
 * release it on demand. */
function deferred(): { promise: Promise<void>; resolve: () => void } {
  let resolve!: () => void;
  const promise = new Promise<void>((r) => {
    resolve = r;
  });
  return { promise, resolve };
}

/** Let every pending microtask settle (a macrotask hop). Used instead
 * of counting `await Promise.resolve()` hops so assertions don't depend
 * on the runner's internal microtask depth. */
const drain = (): Promise<void> => new Promise((r) => setTimeout(r, 0));

test("never overlaps; mid-run triggers coalesce into one follow-up; latest arg wins", async () => {
  const argsSeen: string[] = [];
  let active = 0;
  let maxActive = 0;
  const gates: Array<ReturnType<typeof deferred>> = [];

  const runner = createCoalescingRunner<string>(async (arg) => {
    active++;
    maxActive = Math.max(maxActive, active);
    argsSeen.push(arg);
    const g = deferred();
    gates.push(g);
    await g.promise; // hold the run open until the test releases it
    active--;
  });

  // Start run #1 — it parks on gate[0].
  const first = runner.trigger("a");
  await drain();
  assert.equal(runner.isRunning(), true);
  assert.deepEqual(argsSeen, ["a"]);

  // Fire a burst while run #1 is parked — none of these may start a
  // concurrent run; they collapse into a single pending follow-up.
  runner.trigger("b");
  runner.trigger("c");
  runner.trigger("d");
  await drain();
  assert.deepEqual(argsSeen, ["a"], "no concurrent run started");
  assert.equal(runner.isPending(), true);

  // Release run #1 → exactly ONE follow-up runs, with the latest arg.
  gates[0]!.resolve();
  await drain();
  assert.deepEqual(argsSeen, ["a", "d"], "one coalesced follow-up, latest arg wins");
  assert.equal(runner.isPending(), false);

  // Release the follow-up → runner goes idle.
  gates[1]!.resolve();
  await first;
  await drain();
  assert.equal(runner.isRunning(), false);
  assert.equal(maxActive, 1, "task never ran concurrently with itself");
});

test("triggers again after going idle", async () => {
  const argsSeen: string[] = [];
  const runner = createCoalescingRunner<string>(async (arg) => {
    argsSeen.push(arg);
  });

  await runner.trigger("one");
  assert.equal(runner.isRunning(), false);
  await runner.trigger("two");

  assert.deepEqual(argsSeen, ["one", "two"]);
});

test("a throwing task still drains the coalesced follow-up and never rejects", async () => {
  const argsSeen: string[] = [];
  const gate = deferred();
  let calls = 0;

  const runner = createCoalescingRunner<string>(async (arg) => {
    argsSeen.push(arg);
    calls++;
    if (calls === 1) {
      await gate.promise;
      throw new Error("boom"); // first run fails after a follow-up is queued
    }
  });

  const chain = runner.trigger("a"); // parks on the gate, will throw
  await drain();
  runner.trigger("b"); // queued behind the throwing run
  assert.equal(runner.isPending(), true);

  gate.resolve();
  await assert.doesNotReject(chain, "task throw is swallowed, chain resolves");
  await drain();
  assert.deepEqual(argsSeen, ["a", "b"], "follow-up ran despite the throw");
  assert.equal(runner.isRunning(), false);
});

test("idle() resolves only after the active run AND its coalesced follow-up drain", async () => {
  const argsSeen: string[] = [];
  const gates: Array<ReturnType<typeof deferred>> = [];
  const runner = createCoalescingRunner<string>(async (arg) => {
    argsSeen.push(arg);
    const g = deferred();
    gates.push(g);
    await g.promise;
  });

  // Idle when nothing has ever run → resolves immediately.
  await assert.doesNotReject(runner.idle());

  runner.trigger("a"); // parks on gate[0]
  await drain();
  runner.trigger("b"); // coalesced follow-up
  await drain();

  let settled = false;
  const idle = runner.idle().then(() => {
    settled = true;
  });
  await drain();
  assert.equal(settled, false, "idle() must not resolve while a run is in flight");

  gates[0]!.resolve(); // run #1 done → follow-up "b" starts
  await drain();
  assert.equal(settled, false, "idle() still pending until the follow-up drains");

  gates[1]!.resolve(); // follow-up done → runner idle
  await idle;
  assert.equal(settled, true, "idle() resolves once the runner is fully drained");
  assert.deepEqual(argsSeen, ["a", "b"]);
});

test("idle() never rejects even when the active task throws", async () => {
  const runner = createCoalescingRunner<string>(async () => {
    throw new Error("boom");
  });
  runner.trigger("a");
  await assert.doesNotReject(runner.idle(), "idle() swallows task errors like the chain");
});

test("a burst of concurrent triggers never overlaps (the backfill OOM invariant)", async () => {
  let active = 0;
  let maxActive = 0;
  let runs = 0;

  const runner = createCoalescingRunner<number>(async () => {
    active++;
    maxActive = Math.max(maxActive, active);
    await new Promise((r) => setTimeout(r, 1));
    active--;
    runs++;
  });

  // 100 triggers issued synchronously — all but the first land while
  // run #1 is parked, so they coalesce into a single follow-up.
  const ps: Array<Promise<void>> = [];
  for (let i = 0; i < 100; i++) ps.push(runner.trigger(i));
  await Promise.all(ps);
  await drain();

  assert.equal(maxActive, 1, "at most one scan in flight — bounds daemon memory");
  assert.equal(runs, 2, "100 triggers coalesced into 2 runs (initial + one follow-up)");
});
