// crw-browse — MCP server for interactive browser automation over CDP.

// `ErrorResponse` is not an error code we translate at the boundary, it IS the
// MCP payload we send back: code, message, retry hint, the pattern a policy
// allowed, the count of items done before the failure. Rust 1.98's clippy flags
// it as a large `Err` variant, and the mechanical answer is to box it. That
// trades one memcpy for a heap allocation on every error path, in a crate where
// each operation is a CDP round-trip measured in milliseconds, and it puts a
// `Box` in twenty-one public signatures for no reader's benefit.
//
// Scoped to this crate so every other lint stays on, and so the decision is
// visible here rather than buried in a CI flag.
#![allow(clippy::result_large_err)]

pub mod errors;
pub mod response;
pub mod server;
pub mod session;
pub mod snapshot;
pub mod tools;
