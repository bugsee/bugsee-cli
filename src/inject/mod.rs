//! Debug-ID injection for source maps.
//!
//! Implements the deterministic-UUID scheme: each JS bundle gets a UUIDv5 derived from
//! the file's content (so re-bundling identical code produces the same id), appended
//! as `//# debugId=<uuid>` plus a runtime stub that registers the id with
//! `globalThis._bugseeDebugIds` keyed by `Error().stack`. The matching `.map` file is
//! rewritten to embed `"debug_id": "<uuid>"` and `"debugId": "<uuid>"` (both keys for
//! downstream tooling compatibility).
//!
//! Stage placement: `inject` runs after the bundler completes (Metro, webpack, vite,
//! rollup output) and BEFORE upload. For RN, this is at the Metro serializer step so
//! the debug id ends up in the bundle the device runs. For web, this is a post-build
//! CI step.
