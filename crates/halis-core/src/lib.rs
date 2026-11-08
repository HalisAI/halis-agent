//! Core building blocks for the Halis agent: the unified message model, the
//! embedded models.dev catalog, provider clients for the three major wire
//! protocols, a tool registry, and the agent loop that ties them together.

pub mod agent;
pub mod catalog;
pub mod config;
pub mod error;
pub mod message;
pub mod prompt;
pub mod provider;
pub mod resolver;
pub mod session;
pub mod sse;
pub mod tools;
pub mod util;
