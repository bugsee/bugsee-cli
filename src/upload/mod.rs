//! Upload protocols.
//!
//! Two paths exist:
//!   - `chunked` — modern protocol: `GET /v2/files/chunk-upload` returns server caps
//!     (chunk size, max parallelism, accepted compression set), client computes sha1
//!     of each chunk and uploads only missing ones, then calls an assemble endpoint
//!     with `{checksum, chunks: [sha1, ...]}`. Default for all new uploads.
//!   - `presigned` — legacy two-stage flow: `POST /apps/:app_id/symbols` with metadata
//!     JSON, server returns a presigned PUT URL, client PUTs the binary. Retained for
//!     backward compatibility with deployments that haven't picked up the chunked
//!     endpoint yet.
//!
//! Both paths key uploads by debug-id on the server (see `symbols::debug_id`).
//! The presigned path also accepts the older `(uuid, version, build, hash)` tuple
//! during the migration window.

pub mod chunked;
pub mod presigned;
