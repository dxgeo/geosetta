//! The kinds of value a key holds across a table: what a tabular writer's
//! column type is inferred from (`plans/columnar.org` D-C5).
//!
//! The row IR's inference merges a lattice value by value, and its result is
//! a function of which kinds it met, never of their order or count. So a
//! block can answer for a typed column without visiting a cell, and blocks
//! combine by union.

use super::Value;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct Kinds(u8);

impl Kinds {
    pub(crate) const NONE: Kinds = Kinds(0);
    pub(crate) const BOOL: Kinds = Kinds(1);
    pub(crate) const INT: Kinds = Kinds(2);
    pub(crate) const FLOAT: Kinds = Kinds(4);
    pub(crate) const STR: Kinds = Kinds(8);
    pub(crate) const BINARY: Kinds = Kinds(16);
    /// An array or object, which infers as a string.
    pub(crate) const NESTED: Kinds = Kinds(32);

    pub(crate) fn of(value: Value<'_>) -> Kinds {
        match value {
            Value::Null => Kinds::NONE,
            Value::Bool(_) => Kinds::BOOL,
            Value::Int(_) => Kinds::INT,
            Value::Float(_) => Kinds::FLOAT,
            Value::Str(_) => Kinds::STR,
            Value::Binary(_) => Kinds::BINARY,
            Value::Json(_) => Kinds::NESTED,
        }
    }

    pub(crate) fn is_empty(self) -> bool {
        self == Kinds::NONE
    }

    /// Whether every kind held is among `kinds`.
    pub(crate) fn within(self, kinds: Kinds) -> bool {
        self.0 & !kinds.0 == 0
    }
}

impl std::ops::BitOr for Kinds {
    type Output = Kinds;
    fn bitor(self, other: Kinds) -> Kinds {
        Kinds(self.0 | other.0)
    }
}

impl std::ops::BitOrAssign for Kinds {
    fn bitor_assign(&mut self, other: Kinds) {
        self.0 |= other.0;
    }
}
