//! The conformance cases, grouped by contract. Each is a plain `pub fn` over
//! `&dyn Backend` that panics on violation — the per-backend runners macro
//! them into `#[test]`s.

mod add_recall;
mod heads_forks;
mod oplog_import;
mod supersede_forget;

pub use add_recall::*;
pub use heads_forks::*;
pub use oplog_import::*;
pub use supersede_forget::*;
