//! Routines: scheduled jobs the owner sets up ("order dinner every
//! Saturday", "rent reminder on the 1st", "gift ideas on Oct 29 at 8").
//! They are separate from the built-in heartbeat and feed jobs (`jobs.rs`).
//!
//! - The owner creates and edits routines in the app; those changes apply
//!   at once.
//! - Iris proposes routines (and changes to them) through the `iris` MCP
//!   bridge. A proposal does nothing until the owner taps its chat card;
//!   pending proposals are stored, so they survive restarts.
//! - A due routine runs as a fresh hidden job conversation (`run.rs`), the
//!   owner's own individual use: it never involves a shared conversation.
//!
//! Plain SQL shared by the HTTP API, the scheduler, and the MCP bridge.

pub mod schedule;
