//! HTTP-facing layer: Microsoft/Xbox auth, token storage, xCloud API client,
//! Game Pass catalog hydration and the per-session signaling exchange.

pub mod auth;
pub mod catalog;
pub mod client;
pub mod error;
pub mod session;
pub mod token_store;
pub mod transport;

pub use error::{Error, Result};
