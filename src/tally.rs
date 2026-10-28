//! Counting read-side warnings per cause — `plans/lossy-conversion-warnings.org`
//! § 2b.
//!
//! A read-time loss is observed once per occurrence, and a real file can hold
//! thousands of occurrences of one cause — a GPS export is thousands of
//! tracks. One line per occurrence would bury every other warning in the run,
//! so a reader counts its causes here and says each one once, with the count.
//! `main.rs`'s `collect_layer_warnings` de-duplicates identical messages; this
//! counts occurrences before there is a message to de-duplicate.

use std::collections::BTreeMap;

/// Occurrences of each read-time loss in one read, keyed by a cause type the
/// reader defines. Causes report in their `Ord` order, so a run's warnings come
/// out in the same order every time.
pub(crate) struct Tally<K: Ord> {
    counts: BTreeMap<K, usize>,
}

impl<K: Ord> Default for Tally<K> {
    fn default() -> Self {
        Tally { counts: BTreeMap::new() }
    }
}

impl<K: Ord> Tally<K> {
    pub fn add(&mut self, cause: K) {
        *self.counts.entry(cause).or_insert(0) += 1;
    }

    /// Append one message per cause seen, rendered from the cause and its
    /// count. A cause never added says nothing.
    pub fn report(self, warnings: &mut Vec<String>, message: impl Fn(&K, usize) -> String) {
        warnings.extend(self.counts.iter().map(|(cause, &n)| message(cause, n)));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn says_each_cause_once_with_its_count_in_cause_order() {
        let mut tally = Tally::default();
        for cause in ["b", "a", "b", "b"] {
            tally.add(cause);
        }
        let mut out = Vec::new();
        tally.report(&mut out, |cause, n| format!("{cause} x{n}"));
        assert_eq!(out, vec!["a x1".to_string(), "b x3".to_string()]);
    }

    #[test]
    fn an_empty_tally_says_nothing() {
        let mut out = vec!["already here".to_string()];
        Tally::<u8>::default().report(&mut out, |_, _| unreachable!());
        assert_eq!(out, vec!["already here".to_string()]);
    }
}
