//! Async orchestration of one tick, generic over the ports (spec §11.3).
//!
//! notification recovery -> discover -> claim -> fetch -> decode -> parse ->
//! normalize -> plausibility -> filter -> diff -> persist -> notify -> archive
//! -> tick close.
//!
//! **May depend on:** `jobmon-core`, `jobmon-ports`, `jobmon-adapters`,
//! `jobmon-errors`.
//! **Must not know about:** the AWS SDK, reqwest, Telegram.
//!
//! Populated in Phase 3.
