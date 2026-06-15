//! Upload protocols — the build-time upload classes the design routes through
//! `bugsee-cli`.
//!
//!   - `build` — artefact upload (single-PUT): register the build
//!     (`POST /v2/apps/<token>/builds`), PUT the normalized upload ZIP
//!     (artefact + zstd mapping), and drive the build-info bundle from the same
//!     registration.
//!   - `chunked` — artefact upload (chunked) for large artefacts: the BUILDS
//!     chunked protocol (`GET /builds/chunk-options` → SHA-1 slice →
//!     `POST /builds/chunks/check` → PUT missing chunks → `POST /builds/chunked`).
//!     The server stitches the chunks server-side via S3 UploadPartCopy.
//!   - `build_info` — the per-build metadata bundle (deps/timings sidecars) as
//!     one zstd ZIP; self-contained or pre-signed registration.
//!   - `presigned` — legacy two-stage symbol flow (`POST /apps/:app_id/symbols`
//!     → presigned PUT). Keyed by debug-id on the server.
//!
//! `http` is the shared client + retry/backoff + telemetry layer the build-time
//! classes build on (the design's "bugsee-cli as the common origin" guarantee:
//! one HTTP implementation, tested in one place). `presigned` predates it and
//! keeps its own single-shot client for now; folding it in is deferred to the
//! symbols-consolidation pass.

pub mod build;
pub mod build_info;
pub mod chunked;
pub mod http;
pub mod presigned;
