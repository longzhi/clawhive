//! Amazon Bedrock provider using the Converse API.
//!
//! Uses hand-rolled SigV4 signing (via `aws-sigv4`) and a hand-written
//! AWS event-stream decoder — no heavyweight AWS SDK dependencies.
//!
//! See `docs/plans/2026-04-17-bedrock-provider-design.md` for design rationale.

pub mod converse;
pub mod eventstream;
pub mod sigv4;
