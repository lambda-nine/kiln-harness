//! A small provider-neutral agent harness that executes tools only through kilnd.

// kilnd is reached over a Unix socket, and journals rely on Unix descriptors.
#[cfg(not(unix))]
compile_error!("kiln-harness supports only Unix hosts");

pub mod api;
pub mod audit;
pub mod engine;
pub mod eval;
pub mod files;
pub mod journal;
mod outputs;
pub mod provider;
pub mod providers;
pub mod replay;
pub mod tools;

/// Rust types and clients generated from the pinned `kilnd.v1` schema.
pub mod pb {
    #![allow(clippy::all, clippy::pedantic)]
    tonic::include_proto!("kilnd.v1");
}
