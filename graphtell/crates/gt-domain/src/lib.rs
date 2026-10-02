//! `gt-domain` — GraphTell's domain kernel (the innermost layer of the hexagonal architecture).
//!
//! This crate only describes "what things are" and "what capabilities are needed" (port traits); it
//! **contains no IO implementation**: no filesystem, no database, no tree-sitter. Every external capability is
//! injected backwards through the traits defined in [`port`].
//!
//! Layering convention:
//! * [`model`] — entities and value objects (language-agnostic, framework-agnostic)
//! * [`port`] — outbound / inbound ports
//! * [`error`] — domain errors

pub mod error;
pub mod model;
pub mod port;

pub use error::{DomainError, Result};
