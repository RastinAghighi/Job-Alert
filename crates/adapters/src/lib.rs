//! ATS-family parsers: `&[u8] -> Vec<RawJob>`, **pure** (spec §18).
//!
//! One adapter per ATS *family*, never one per company (§4 D10): twenty
//! Greenhouse companies share one contract and differ only by
//! `endpoint_config`.
//!
//! **May depend on:** `jobmon-core`, `jobmon-errors`.
//! **Must not know about:** HTTP clients, tokio, AWS.
//! **Adapters never perform networking** and are tested purely on bytes.
//!
//! Populated in Phase 2 (Greenhouse + Lever). Fixtures live in
//! `<workspace-root>/tests/fixtures/<adapter>/`.
