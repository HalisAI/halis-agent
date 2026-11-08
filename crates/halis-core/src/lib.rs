//! Core building blocks for the Halis agent: the unified message model, the
//! embedded models.dev catalog, provider clients, a tool registry, and the
//! agent loop that ties them together.

pub mod catalog;
pub mod config;
pub mod error;
pub mod message;
pub mod prompt;
pub mod resolver;
pub mod sse;
pub mod util;
