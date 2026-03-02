//! arca-proto: S3 HTTP protocol adapter built on Axum.
//!
//! Provides the HTTP routing and handler layer for the Arca S3 server.

pub mod handlers;
pub mod router;
pub mod state;
pub mod xml;

pub use router::build_router;
pub use state::AppState;
