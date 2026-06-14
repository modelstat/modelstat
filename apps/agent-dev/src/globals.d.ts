/**
 * Build-time macros injected by tsup's `define` (see tsup.config.ts).
 *
 * `__MODELSTAT_VERSION__` resolves to a string like "agent-0.0.33"
 * — the agent CLI's package.json version with an "agent-" prefix —
 * baked in at bundle time. Three modules used to hardcode this and
 * silently drift across releases (cli.ts said "0.0.1", scan.ts said
 * "0.0.23", daemon.ts walked the disk for package.json which broke
 * once the bundle was copied to `~/.modelstat/bin/`). Centralising
 * via a build-time substitution makes the version impossible to
 * forget.
 */
declare const __MODELSTAT_VERSION__: string;
