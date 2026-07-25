//! Timbang — a debate engine that refuses to conclude.
//!
//! Two models argue opposite sides of one claim, a third moderates, and the
//! result is an argument map rather than a verdict. The point is not to find out
//! who won; it is to see which arguments were thrown and never answered.
//!
//! Read `CLAUDE.md` before changing anything here. §1 in particular describes
//! constraints that look like missing features and are not.

pub mod config;
pub mod engine;
pub mod llm;
pub mod phase;
pub mod render;
pub mod transcript;
pub mod view;
