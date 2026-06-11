//! Chunked upload protocol — implementation pending.
//!
//! Flow:
//!   1. `GET /v2/files/chunk-upload` → server caps `{url, chunk_size, chunks_per_request,
//!      max_file_size, max_request_size, concurrency, hash_algorithm, compression, accept}`.
//!   2. Slice file → sha1 each chunk.
//!   3. `POST {url}` with missing chunks only (multipart, filename = sha1).
//!   4. `POST /v2/files/dif/assemble` with `{checksum, chunks: [sha1, ...]}` to commit.
