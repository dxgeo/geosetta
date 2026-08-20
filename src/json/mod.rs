//! Minimal standard-library-only JSON support: a value model and a parser.

mod parser;
mod value;

pub(crate) use parser::Parser;
pub use parser::{parse, raw_at};
pub use value::{escape_into, JsonValue};
