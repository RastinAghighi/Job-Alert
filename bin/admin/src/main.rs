//! Source-registration CLI (spec §19, §20).
//!
//! Package name is `admin` (not `jobmon-admin`) so that the runbook command in
//! §19 works verbatim:
//!
//! ```text
//! cargo run -p admin -- add-source --company "Cohere" --adapter greenhouse \
//!     --board cohere --criticality standard --interval 10m \
//!     --bootstrap relevant_summary
//! ```
//!
//! Populated in Phase 6.

fn main() {}
