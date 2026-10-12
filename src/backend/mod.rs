//! What a browser instance can do, and the errors shared by both protocols.
//!
//! Tools are protocol-neutral entries of one table; each instance runs one
//! browser for its whole life, and the table checks the instance's
//! [`Capabilities`] before dispatching to that browser's implementation, so
//! a tool an instance cannot serve fails before anything is sent.

pub mod bidi;
pub mod capabilities;
pub mod error;

pub use capabilities::{BrowserKind, Capabilities, Capability};
pub use error::BackendError;
