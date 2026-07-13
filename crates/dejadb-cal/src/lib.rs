//! dejadb-cal — CAL (Context Assembly Language) for DejaDB.
//!
//! CAL implementation: lexer, parser, AST, executor,
//! ASSEMBLE engine, templates, saved queries, executed against the
//! embedded Turso store through `DejaDbFacade`. The Postgres facade and
//! recursive-SQL graph module are intentionally not ported.

pub mod assemble;
pub mod ast;
pub mod errors;
pub mod executor;
pub mod facade;
pub mod humanize;
pub mod json;
pub mod json_build;
pub mod lexer;
pub mod dejadb_facade;
pub mod parser;
pub mod queries;
pub mod relations;
pub mod store_types;
pub mod templates;

pub use executor::{CalExecutor, CalExecutorConfig};
pub use facade::CalStoreFacade;
pub use dejadb_facade::DejaDbFacade;
pub use parser::parse;
