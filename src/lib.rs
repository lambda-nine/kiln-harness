//! A small provider-neutral agent harness that executes tools only through kilnd.

pub mod api;
pub mod audit;
pub mod engine;
pub mod journal;
pub mod provider;
pub mod replay;
pub mod tools;

/// Rust types and clients generated from the pinned `kilnd.v1` schema.
pub mod pb {
    #![allow(clippy::all, clippy::pedantic)]
    tonic::include_proto!("kilnd.v1");
}
