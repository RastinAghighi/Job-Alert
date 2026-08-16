//! Three-axis failure taxonomy (spec §9).
//!
//! `Stage` answers *where*, `FaultDomain` answers *whose fault*, `FailureKind`
//! answers *what*. Alert policy keys on `(domain, kind)`; the human-facing
//! message leads with stage.
//!
//! **May depend on:** nothing (std + serde).
//! **Must not know about:** AWS, HTTP, tokio.
//!
//! Populated in Phase 1.
