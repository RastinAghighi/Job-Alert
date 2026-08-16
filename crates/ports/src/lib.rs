//! Trait definitions ONLY (spec §17.1).
//!
//! Native `async fn` in traits, **static dispatch via generics** — no
//! `#[async_trait]`, no `Box<dyn>`. There is exactly one implementation per
//! port in production and one in tests, so monomorphisation costs nothing and
//! dyn-compatibility problems never arise.
//!
//! **May depend on:** `jobmon-core`, `jobmon-errors`.
//! **Must not know about:** any concrete implementation.
//!
//! Populated in Phase 3.
