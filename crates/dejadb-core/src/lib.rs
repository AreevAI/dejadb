//! dejadb-core — OMS grain types, the .mg binary format, canonical
//! serialization, and content addressing. Licensed under MIT OR Apache-2.0.

pub mod error;
pub mod format;
pub mod types;

pub use error::{Hash, DejaDbError, Result};
pub use format::*;
pub use types::*;
