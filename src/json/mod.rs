//! Minimal standard-library-only JSON support: a value model and a parser.

mod parser;
mod value;

pub use parser::parse;
pub(crate) use parser::Parser;
pub use value::{escape_into, JsonValue};
