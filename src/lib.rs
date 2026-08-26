//! nuthatch - be your own indexer.
//!
//! The crate's modules are exposed as a library so a second front-end - notably `nuthatch-node`,
//! the colocated reth ExEx build (RFC-0003) - can reuse the *same* indexing core (decode → hot
//! store → seal → IVM → serve) rather than fork it. The `nuthatch` binary (`main.rs`) is one such
//! front-end; a reth-driven one is another, and both drive the pipeline through the `Source` trait.

pub mod abi;
pub mod alerts;
pub mod allowlist;
pub mod analytics;
pub mod audit;
pub mod authored_entity_spike;
pub mod bench;
pub mod blob;
pub mod calldata;
pub mod calls;
pub mod chains;
pub mod check;
pub mod chunker;
pub mod cid;
pub mod cli;
pub mod config;
/// The control-plane HTTP API (RFC-0022 §3): declare what the fleet should run.
#[cfg(feature = "postgres-store")]
pub mod control_api;
/// The control plane for scaled mode (RFC-0022 §3): desired state and the worker registry.
#[cfg(feature = "postgres-store")]
pub mod controlplane;
pub mod distribution;
pub mod doctor;
pub mod effectful;
pub mod entities;
pub mod entity_expr;
pub mod entity_lower;
pub mod entity_plan;
pub mod entity_row;
pub mod entity_view;
pub mod exposure;
pub mod factory;
pub mod flags;
pub mod graft;
pub mod health;
pub mod help;
pub mod indexer;
pub mod ipfs;
pub mod labels;
pub mod lifecycle;
pub mod lists;
pub mod mcp;
pub mod metadata;
pub mod metrics;
pub mod migrate;
pub mod pack;
/// The Postgres hot store (RFC-0022 slice 2). Feature-gated so the default build stays a single
/// binary with no database in its dependency tree.
#[cfg(feature = "postgres-store")]
pub mod pgstore;
pub mod progress;
pub mod project;
pub mod prune;
pub mod recipes;
/// The reconcile loop for scaled mode (RFC-0022 §2/§3): plan in, leases out.
#[cfg(feature = "postgres-store")]
pub mod reconcile;
pub mod registry;
pub mod rpc;
pub mod runtime;
/// Cursor placement for scaled mode (RFC-0022 §2) - pure decision logic, no I/O.
pub mod scheduler;
pub mod screen;
pub mod seal;
pub mod semantic;
pub mod serve;
pub mod skill;
pub mod source;
pub mod sql_errors;
pub mod store;
pub mod subgraph_import;
pub mod tape;
pub mod transform;
pub mod velocity;
pub mod views;
pub mod webhooks;
/// The writer-worker role for scaled mode (RFC-0022 §2): the reconcile loop, running.
#[cfg(feature = "postgres-store")]
pub mod worker;
