pub mod config;
pub mod error;
pub mod runtime;
pub mod service;
pub mod workspace;

// Tonic-generated code does not follow our lint policy. Scope the allows
// to this module so user-written code still gets strict checking.
#[allow(
    clippy::all,
    clippy::pedantic,
    clippy::nursery,
    missing_docs,
    missing_debug_implementations,
    rust_2018_idioms,
    unused_qualifications
)]
pub mod pb {
    tonic::include_proto!("scriptorium.v1");
}

pub use error::{Error, Result};
