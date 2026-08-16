//! All business logic. **100% synchronous, zero I/O** (spec §4 D11).
//!
//! Making this crate sync is what lets `cargo test` cover every business rule
//! with no runtime, no mocks and no network — and means the `Send`/`!Send`
//! question never reaches the domain model.
//!
//! **May depend on:** `jobmon-errors`.
//! **Must not know about:** ports, AWS, HTTP, tokio, async.
//!
//! Module set is frozen by §17. Populated in Phase 1 — write the event-key
//! repeat-transition regression test FIRST (§38).

pub mod diff;
pub mod event_key;
pub mod filter;
pub mod health;
pub mod model;
pub mod normalize;
pub mod plausibility;
pub mod schedule;
pub mod shape;
