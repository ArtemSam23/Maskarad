//! Maskarad core: detection, masking and demasking of personal data.
//!
//! The crate is pure computation without I/O so it can be tested and
//! benchmarked in isolation. The HTTP service lives in the `maskarad` crate.

pub mod config;
pub mod demask;
pub mod detect;
pub mod dict;
pub mod engine;
pub mod error;
pub mod inflect;
pub mod mask;
pub mod normalize;
pub mod registry;
pub mod resolve;
pub mod sensitive;
pub mod types;

pub use config::CoreConfig;
pub use dict::Dictionaries;
pub use engine::{CompiledSystem, Engine};
pub use error::{CoreError, Result};
pub use mask::MaskState;
pub use sensitive::Sensitive;
pub use types::{Candidate, Entity, MaskEntry, MaskResult, PiiType, Span};
