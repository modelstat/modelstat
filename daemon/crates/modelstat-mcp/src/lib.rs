//! Embedded stdio MCP bridge + `wire`/`--heal` + the compiled-in tool catalog
//! and widget asset (§12). A port of the daemon-relevant behavior of
//! `packages/mcp/`.
//!
//! - [`wire`]  — drop the `{command:<abs modelstat>, args:["mcp"]}` server entry
//!   into every detected AI tool (idempotent, non-destructive), plus `--heal`
//!   self-healing that respects a user who removed our entry.
//! - [`catalog`] — the compiled-in static 8-tool fallback catalog.
//! - [`bridge`] — the stdio JSON-RPC bridge that forwards `tools/call` to
//!   `POST /v1/mcp/call` and serves `tools/list` from the cache/static catalog.

pub mod bridge;
pub mod catalog;
pub mod runtime;
pub mod wire;
