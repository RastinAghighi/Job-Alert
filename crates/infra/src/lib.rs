//! Concrete port implementations (spec §17).
//!
//! **May depend on:** everything above plus the AWS SDK and reqwest.
//! **Must not contain business rules** — those live in `jobmon-core`.
//!
//! Populated across Phases 3-6.

pub mod archive_s3;
pub mod fetch_reqwest;
pub mod heartbeat_http;
pub mod notify_telegram;
pub mod repo_dynamo;
pub mod repo_memory;
