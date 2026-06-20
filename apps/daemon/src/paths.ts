/**
 * The daemon's single home directory — the one stable, deterministic place for
 * ALL daemon state on a machine: identity, runtime state (file cursors, …),
 * the singleton lockfile, downloaded models, logs, and the staged binary.
 *
 * Default: `~/.modelstat`. Override with `MODELSTAT_HOME` to relocate everything
 * at once — e.g. `MODELSTAT_HOME=/opt/modelstat` for a system-wide, exactly-one-
 * per-server install.
 *
 * This deliberately replaces the old per-subsystem `conf` store, whose path
 * varied by OS (`~/Library/Preferences/<name>-nodejs` on macOS vs `~/.config`
 * on Linux) AND by project name (a rename from `modelstat-agent` →
 * `modelstat-daemon`, or a `-dev` build, silently orphaned state into a second
 * directory). One function, one location, identical on every platform — so state
 * can never fork again.
 */

import { homedir } from "node:os";
import { join } from "node:path";

/** Absolute path to the daemon home. `MODELSTAT_HOME` wins; else `~/.modelstat`. */
export function modelstatHome(): string {
  const override = process.env.MODELSTAT_HOME?.trim();
  return override && override.length > 0 ? override : join(homedir(), ".modelstat");
}

/** A path inside the daemon home, e.g. `homePath("state.json")`. */
export function homePath(...segments: string[]): string {
  return join(modelstatHome(), ...segments);
}
