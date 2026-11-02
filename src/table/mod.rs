//! The columnar hub (`plans/columnar.org`): a collection's rows as a sequence
//! of blocks, each held as shaped rows or as columns.
//!
//! Every [`crate::FeatureCollection`] holds one (C5). Every guarantee in
//! § WHAT A ROW IR GIVES holds through `from_features` and the views. The
//! tabular readers fill a table a column per key ([`TableBuilder`],
//! [`Table::from_columns`]), the schemaless ones a row at a time
//! ([`RowTableBuilder`]).


#[cfg(test)]
pub(crate) mod arbitrary;
mod bitmap;
mod block;
mod column;
mod kinds;
mod value;

use crate::feature::Feature;
use crate::json::JsonValue;
use crate::geometry::Geometry;
use crate::native::Native;
use block::{Block, RowBuilder, Slot, TabularBlock};
pub(crate) use block::{Properties, Sizes};
pub(crate) use kinds::Kinds;
pub use value::Value;
use std::collections::HashMap;
use std::rc::Rc;

/// Which form a table's blocks take (`plans/columnar.org` D-C14).
///
/// A memory and time setting only: every layout writes byte-identical
/// output, so none of them preserves more than another.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Layout {
    /// Each block as its data suits: columns when it is dense and regular,
    /// shaped rows otherwise (D-C13).
    #[default]
    Auto,
    /// Every block as shaped rows. A wide tabular file held this way costs
    /// what the row IR did.
    Rows,
    /// Every block as columns. Sparse input held this way can cost far more
    /// than rows: a slot per key per row.
    Columns,
}

impl Layout {
    /// Parse a `--layout` value: `auto`, `rows` or `columns`.
    pub fn parse(value: &str) -> crate::Result<Layout> {
        match value {
            "auto" => Ok(Layout::Auto),
            "rows" => Ok(Layout::Rows),
            "columns" => Ok(Layout::Columns),
            other => Err(crate::Error::Usage(format!("unknown layout \"{other}\" (expected auto, rows or columns)"))),
        }
    }

    /// The layout as `--layout` spells it.
    pub fn as_str(self) -> &'static str {
        match self {
            Layout::Auto => "auto",
            Layout::Rows => "rows",
            Layout::Columns => "columns",
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct Table {
    blocks: Vec<Block>,
    /// Each block's first physical row.
    starts: Vec<usize>,
    /// D-C11: the physical rows the table holds, in order, once a `retain` or
    /// `permute` has run; `None` is every row in physical order.
    selection: Option<Vec<u32>>,
    /// What [`Self::gather`] seals new blocks under.
    layout: Layout,
    sizes: Sizes,
    /// The first block `push` has appended to since it was sealed: it and
    /// every block after it are unsealed rows.
    appended_from: Option<usize>,
}

impl Table {
    pub(crate) fn from_features(features: Vec<Feature>, layout: Layout) -> Table {
        Table::from_features_sized(features, layout, Sizes::DEFAULT)
    }

    pub(crate) fn from_features_sized(features: Vec<Feature>, layout: Layout, sizes: Sizes) -> Table {
        let mut builder = RowTableBuilder::sized(layout, sizes);
        builder.blocks.reserve(features.len().div_ceil(sizes.block_rows));
        for feature in features {
            builder.push(feature);
        }
        builder.finish()
    }

    /// A tabular reader's rows, given a column at a time: `geometry` is
    /// every row's, and `columns` yields key `i`'s value for every row when
    /// called with `i`. Each column is spread over the blocks and dropped
    /// before the next is asked for, so the reader never holds two.
    pub(crate) fn from_columns<E>(
        keys: Vec<Rc<str>>,
        geometry: Vec<Option<Geometry>>,
        mut columns: impl FnMut(usize) -> Result<Vec<JsonValue>, E>,
        layout: Layout,
        sizes: Sizes,
    ) -> Result<Table, E> {
        let mut blocks: Vec<TabularBlock> = Vec::with_capacity(geometry.len().div_ceil(sizes.block_rows));
        for (row, geometry) in geometry.into_iter().enumerate() {
            if row % sizes.block_rows == 0 {
                blocks.push(TabularBlock::new(&keys));
            }
            blocks.last_mut().expect("pushed above").push_geometry(geometry);
        }
        for i in 0..keys.len() {
            let mut values = columns(i)?.into_iter();
            for block in &mut blocks {
                let rows = block.len();
                block.column(i, values.by_ref().take(rows));
            }
        }
        let distinct = are_distinct(&keys);
        let blocks = blocks.into_iter().map(|b| b.seal(distinct, layout, sizes.intern_sample)).collect();
        Ok(Table::from_blocks(blocks, layout, sizes))
    }

    fn from_blocks(blocks: Vec<Block>, layout: Layout, sizes: Sizes) -> Table {
        let starts = blocks
            .iter()
            .scan(0, |next, b: &Block| {
                let start = *next;
                *next += b.len();
                Some(start)
            })
            .collect();
        Table { blocks, starts, selection: None, layout, sizes, appended_from: None }
    }

    /// Rows the table holds, after any `retain`.
    pub(crate) fn len(&self) -> usize {
        match &self.selection {
            Some(selection) => selection.len(),
            None => self.physical_len(),
        }
    }

    fn physical_len(&self) -> usize {
        self.blocks.iter().map(Block::len).sum()
    }

    /// The physical row behind logical row `i`.
    fn physical(&self, i: usize) -> usize {
        match &self.selection {
            Some(selection) => selection[i] as usize,
            None => i,
        }
    }

    /// The block holding physical row `row`, and the row's index within it.
    fn locate(&self, row: usize) -> (usize, usize) {
        let block = self.starts.partition_point(|&start| start <= row) - 1;
        (block, row - self.starts[block])
    }

    pub(crate) fn row(&self, i: usize) -> RowRef<'_> {
        let (block, row) = self.locate(self.physical(i));
        RowRef { block: &self.blocks[block], row }
    }

    pub(crate) fn rows(&self) -> impl Iterator<Item = RowRef<'_>> {
        (0..self.len()).map(|i| self.row(i))
    }

    /// Every key in the order rows first use it, each with the kinds of value
    /// it holds: what a tabular writer infers its columns from (D-C5), in the
    /// order it declares them (D-C4).
    ///
    /// Read block by block, and a typed column without visiting a cell, while
    /// the rows are as sealed. Once a selection reorders them or a patch
    /// changes them, row by row in logical order, which is what the row
    /// inference costs.
    pub(crate) fn key_kinds(&self) -> Vec<(&Rc<str>, Kinds)> {
        #[derive(Default)]
        struct Keys<'a> {
            index: HashMap<&'a str, usize>,
            keys: Vec<(&'a Rc<str>, Kinds)>,
        }
        impl<'a> Keys<'a> {
            fn note(&mut self, key: &'a Rc<str>, kinds: Kinds) {
                let next = self.keys.len();
                let at = *self.index.entry(&**key).or_insert(next);
                if at == next {
                    self.keys.push((key, Kinds::NONE));
                }
                self.keys[at].1 |= kinds;
            }
        }
        let mut keys = Keys::default();
        if self.selection.is_none() && !self.blocks.iter().any(Block::is_patched) {
            for block in &self.blocks {
                for key in block.first_seen() {
                    keys.note(key, Kinds::NONE);
                }
                block.kinds(&mut |key, kinds| keys.note(key, kinds));
            }
        } else {
            for row in self.rows() {
                for (key, value) in row.block.properties(row.row) {
                    keys.note(key, Kinds::of(value));
                }
            }
        }
        keys.keys
    }

    pub(crate) fn column<'a>(&'a self, key: &'a str) -> ColumnRef<'a> {
        self.columns(&[key]).pop().expect("one key, one column")
    }

    /// One [`ColumnRef`] for each of `keys`, located in every block in one
    /// pass over it, rather than a pass per key.
    pub(crate) fn columns<'a>(&'a self, keys: &[&'a str]) -> Vec<ColumnRef<'a>> {
        let index: HashMap<&str, usize> = keys.iter().enumerate().map(|(i, &k)| (k, i)).collect();
        let mut slots: Vec<Vec<Slot>> = keys.iter().map(|_| Vec::with_capacity(self.blocks.len())).collect();
        for block in &self.blocks {
            for (k, slot) in block.slots(&index).into_iter().enumerate() {
                slots[k].push(slot);
            }
        }
        keys.iter().zip(slots).map(|(&key, slots)| ColumnRef { table: self, key, slots }).collect()
    }

    /// Keep the rows `keep` says to, in order. Records a selection (D-C11);
    /// nothing moves until a gather.
    pub(crate) fn retain(&mut self, mut keep: impl FnMut(RowRef<'_>) -> bool) {
        let kept = (0..self.len()).filter(|&i| keep(self.row(i))).map(|i| self.physical(i) as u32).collect();
        self.selection = Some(kept);
    }

    /// Reorder the rows: row `i` becomes the one that was row `order[i]`.
    /// Composes with any selection already recorded, which costs what
    /// building a new one would.
    pub(crate) fn permute(&mut self, order: &[usize]) {
        assert_eq!(order.len(), self.len(), "a permutation names every row once");
        let permuted = order.iter().map(|&i| self.physical(i) as u32).collect();
        self.selection = Some(permuted);
    }

    /// Give row `i`'s first `key` the value `value`, in its place; a row
    /// without the key gains it last, as an edit to its `Vec` would (D-C12).
    pub(crate) fn set(&mut self, i: usize, key: &str, value: JsonValue) {
        let (b, row) = self.locate(self.physical(i));
        self.blocks[b].set(row, key, value);
    }

    /// Take every `key` out of row `i`.
    pub(crate) fn remove(&mut self, i: usize, key: &str) {
        let (b, row) = self.locate(self.physical(i));
        self.blocks[b].remove(row, key);
    }

    pub(crate) fn set_geometry(&mut self, i: usize, geometry: Option<Geometry>) {
        let (b, row) = self.locate(self.physical(i));
        self.blocks[b].set_geometry(row, geometry);
    }

    pub(crate) fn set_native(&mut self, i: usize, native: Option<Native>) {
        let (b, row) = self.locate(self.physical(i));
        self.blocks[b].set_native(row, native);
    }

    /// Drop each per-row native `drop` says to, wherever its row is.
    pub(crate) fn drop_natives(&mut self, mut drop: impl FnMut(&Native) -> bool) {
        for block in &mut self.blocks {
            block.drop_natives(&mut drop);
        }
    }

    /// Every row's geometry, in place, in physical order. A row a
    /// selection has left out is visited too, harmlessly: nothing reads it.
    pub(crate) fn for_each_geometry_mut(&mut self, mut f: impl FnMut(&mut Geometry)) {
        for block in &mut self.blocks {
            block.geometries_mut().iter_mut().flatten().for_each(&mut f);
        }
    }

    /// Append a row after the last (D-C12's tail block): into the last block
    /// while it is rows with room, else into a new one.
    pub(crate) fn push(&mut self, feature: Feature) {
        let physical = self.physical_len();
        let room = matches!(self.blocks.last(), Some(b @ Block::Rows(_)) if b.len() < self.sizes.block_rows);
        if !room {
            self.starts.push(physical);
            self.blocks.push(Block::Rows(Default::default()));
        }
        self.appended_from.get_or_insert(self.blocks.len() - 1);
        self.blocks.last_mut().expect("pushed above").push(feature);
        if let Some(selection) = &mut self.selection {
            selection.push(physical as u32);
        }
    }

    /// Row by row, `key` set to `Some` value or removed by `None`: the
    /// column-sized edit of D-C12. Built on the cell edits for now; replacing
    /// each block's buffer directly is the fast path a profile can ask for.
    pub(crate) fn set_column(&mut self, key: &str, values: impl IntoIterator<Item = Option<JsonValue>>) {
        let mut rows = 0;
        for (i, value) in values.into_iter().enumerate() {
            match value {
                Some(value) => self.set(i, key, value),
                None => self.remove(i, key),
            }
            rows += 1;
        }
        assert_eq!(rows, self.len(), "a column has a value or a removal for every row");
    }

    /// [`Self::set_column`] with each row's new value computed from its old.
    pub(crate) fn map_column(&mut self, key: &str, mut f: impl FnMut(usize, Option<Value<'_>>) -> Option<JsonValue>) {
        let column = self.column(key);
        let values: Vec<Option<JsonValue>> = (0..self.len()).map(|i| f(i, column.get(i))).collect();
        self.set_column(key, values);
    }

    pub(crate) fn drop_column(&mut self, key: &str) {
        for i in 0..self.len() {
            self.remove(i, key);
        }
    }

    #[cfg(test)]
    /// Rebuild the blocks edits have touched, each sealed again under the
    /// table's layout: the patch folded in, a tail that took appends given
    /// its form (D-C12). The selection stays; only [`Self::gather`] applies it.
    pub(crate) fn fold(&mut self) {
        let appended_from = self.appended_from.take().unwrap_or(usize::MAX);
        for (b, block) in self.blocks.iter_mut().enumerate() {
            if block.is_patched() || b >= appended_from {
                let taken = std::mem::replace(block, Block::Rows(Default::default()));
                *block = taken.fold(self.layout, self.sizes);
            }
        }
    }

    #[cfg(test)]
    /// Apply the selection and fold every edit: build new blocks from the
    /// rows in order, each choosing its form again (D-C11, D-C13).
    pub(crate) fn gather(&mut self) {
        if self.selection.is_some() || self.appended_from.is_some() || self.blocks.iter().any(Block::is_patched) {
            let (layout, sizes) = (self.layout, self.sizes);
            *self = Table::from_features_sized(std::mem::take(self).into_features(), layout, sizes);
        }
    }

    #[cfg(test)]
    pub(crate) fn into_features(self) -> Vec<Feature> {
        let mut out = Vec::with_capacity(self.physical_len());
        for block in self.blocks {
            block.into_features(&mut out);
        }
        match self.selection {
            None => out,
            Some(selection) => {
                let mut slots: Vec<Option<Feature>> = out.into_iter().map(Some).collect();
                selection.iter().map(|&r| slots[r as usize].take().expect("a selection names a row once")).collect()
            }
        }
    }
}

/// Builds a table a feature at a time, for a reader with no schema (C3):
/// each block fills as shaped rows and seals as `layout` says -- under
/// `Auto`, by D-C13's rule.
pub(crate) struct RowTableBuilder {
    layout: Layout,
    sizes: Sizes,
    blocks: Vec<Block>,
    block: RowBuilder,
}

impl RowTableBuilder {
    pub(crate) fn new(layout: Layout) -> RowTableBuilder {
        RowTableBuilder::sized(layout, Sizes::DEFAULT)
    }

    pub(crate) fn sized(layout: Layout, sizes: Sizes) -> RowTableBuilder {
        RowTableBuilder { layout, sizes, blocks: Vec::new(), block: RowBuilder::new(sizes.intern_sample) }
    }

    pub(crate) fn push(&mut self, feature: Feature) {
        self.block.push(feature);
        if self.block.len() == self.sizes.block_rows {
            let full = std::mem::replace(&mut self.block, RowBuilder::new(self.sizes.intern_sample));
            self.blocks.push(full.seal(self.layout));
        }
    }

    pub(crate) fn finish(mut self) -> Table {
        if self.block.len() > 0 {
            self.blocks.push(self.block.seal(self.layout));
        }
        Table::from_blocks(self.blocks, self.layout, self.sizes)
    }
}

/// Builds a table from a tabular reader's rows (C2): every row holds every
/// key, in declared order, so each block is filled a column per key and,
/// its keys distinct, sealed as columns without a row ever being built.
pub(crate) struct TableBuilder {
    keys: Vec<Rc<str>>,
    distinct: bool,
    layout: Layout,
    sizes: Sizes,
    blocks: Vec<Block>,
    block: TabularBlock,
}

impl TableBuilder {
    pub(crate) fn new(keys: Vec<Rc<str>>, layout: Layout) -> TableBuilder {
        TableBuilder::sized(keys, layout, Sizes::DEFAULT)
    }

    pub(crate) fn sized(keys: Vec<Rc<str>>, layout: Layout, sizes: Sizes) -> TableBuilder {
        let block = TabularBlock::new(&keys);
        TableBuilder { distinct: are_distinct(&keys), keys, layout, sizes, blocks: Vec::new(), block }
    }

    /// Append a row: its geometry, a value for each key in declared order,
    /// and its native.
    pub(crate) fn push(
        &mut self,
        geometry: Option<Geometry>,
        values: impl IntoIterator<Item = JsonValue>,
        native: Option<Box<Native>>,
    ) {
        self.block.push(geometry, values, native);
        if self.block.len() == self.sizes.block_rows {
            self.seal();
        }
    }

    fn seal(&mut self) {
        let full = std::mem::replace(&mut self.block, TabularBlock::new(&self.keys));
        self.blocks.push(full.seal(self.distinct, self.layout, self.sizes.intern_sample));
    }

    pub(crate) fn finish(mut self) -> Table {
        if self.block.len() > 0 {
            self.seal();
        }
        Table::from_blocks(self.blocks, self.layout, self.sizes)
    }
}

/// Whether no key is declared twice.
fn are_distinct(keys: &[Rc<str>]) -> bool {
    let mut seen = std::collections::HashSet::new();
    keys.iter().all(|k| seen.insert(&**k))
}

/// One row of a table (D-C8), read in place.
#[derive(Clone, Copy)]
pub(crate) struct RowRef<'a> {
    block: &'a Block,
    row: usize,
}

impl<'a> RowRef<'a> {
    pub(crate) fn geometry(self) -> Option<&'a Geometry> {
        self.block.geometry(self.row)
    }

    pub(crate) fn native(self) -> Option<&'a Native> {
        self.block.native(self.row)
    }

    /// The row's properties, in its own order (guarantee 2), repeats kept
    /// (guarantee 3).
    pub(crate) fn properties(self) -> Properties<'a> {
        self.block.properties(self.row)
    }

    /// The row's first `key`, or `None` when it has none.
    pub(crate) fn get(self, key: &str) -> Option<Value<'a>> {
        self.block.properties(self.row).find(|(k, _)| &***k == key).map(|(_, v)| v)
    }

    pub(crate) fn to_feature(self) -> Feature {
        Feature {
            geometry: self.geometry().cloned(),
            properties: self.block.properties(self.row).map(|(k, v)| (k.clone(), v.to_owned())).collect(),
            native: self.native().map(|n| Box::new(n.clone())),
        }
    }
}

/// One key down every row of a table (D-C8). A block's column for it may be
/// typed differently from the next block's, or absent, or the block may be
/// rows; this hides which.
#[derive(Debug, Clone)]
pub struct ColumnRef<'a> {
    table: &'a Table,
    key: &'a str,
    /// Per block, where it holds the key.
    slots: Vec<Slot>,
}

impl<'a> ColumnRef<'a> {
    /// Rows: every feature of the collection, whether it holds the key or not.
    pub fn len(&self) -> usize {
        self.table.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Row `i`'s first value for the key, or `None` when the row has none.
    /// Panics when `i` is not below [`Self::len`].
    pub fn get(&self, i: usize) -> Option<Value<'a>> {
        assert!(i < self.len(), "row {i} of a column of {}", self.len());
        let (b, row) = self.table.locate(self.table.physical(i));
        self.table.blocks[b].value_at(row, &self.slots[b], self.key)
    }

    /// [`Self::get`] for every row, in order: a block at a time where the
    /// table has no selection, so a cell costs neither a search for its
    /// block nor, in a block no edit has reached, a look at the patch.
    pub fn values(&self) -> ColumnValues<'_, 'a> {
        ColumnValues { column: self, i: 0, len: self.len(), block: 0, current: None, row: 0 }
    }
}

/// [`ColumnRef::values`].
#[derive(Clone)]
pub struct ColumnValues<'c, 'a> {
    column: &'c ColumnRef<'a>,
    /// The next logical row, and how many there are.
    i: usize,
    len: usize,
    /// Without a selection: the block being read, as its index, the block,
    /// where it holds the key and whether an edit has reached it; and the
    /// row in it next.
    block: usize,
    current: Option<(&'a Block, &'c Slot, bool)>,
    row: usize,
}

impl ExactSizeIterator for ColumnValues<'_, '_> {}

impl<'a> Iterator for ColumnValues<'_, 'a> {
    type Item = Option<Value<'a>>;

    fn next(&mut self) -> Option<Self::Item> {
        let column = self.column;
        let table = column.table;
        if self.i == self.len {
            return None;
        }
        self.i += 1;
        if table.selection.is_some() {
            return Some(column.get(self.i - 1));
        }
        loop {
            if let Some((block, slot, patched)) = self.current
                && self.row < block.len()
            {
                let row = self.row;
                self.row += 1;
                return Some(match patched {
                    true => block.value_at(row, slot, column.key),
                    false => block.sealed_value_at(row, slot),
                });
            }
            if self.current.is_some() {
                self.block += 1;
            }
            let block = &table.blocks[self.block];
            self.current = Some((block, &column.slots[self.block], block.is_patched()));
            self.row = 0;
        }
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let left = self.len - self.i;
        (left, Some(left))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geometry::{Geometry, Position};
    use crate::json::JsonValue;
    use crate::native::{Native, NativePayload};
    use crate::format::Format;
    use std::rc::Rc;

    fn feature(props: &[(&str, JsonValue)]) -> Feature {
        Feature {
            geometry: Some(Geometry::Point(Position::new(1.0, 2.0))),
            properties: props.iter().map(|(k, v)| (Rc::from(*k), v.clone())).collect(),
            native: None,
        }
    }

    fn every_layout_round_trips(features: Vec<Feature>) {
        for layout in [Layout::Auto, Layout::Rows, Layout::Columns] {
            let back = Table::from_features(features.clone(), layout).into_features();
            assert_eq!(back, features, "{layout:?}");
        }
    }

    #[test]
    fn the_seven_guarantees_survive_every_layout() {
        let mut odd = feature(&[("b", JsonValue::Int(1)), ("a", JsonValue::Float(1.0)), ("b", JsonValue::Null)]);
        odd.native = Some(Box::new(Native { format: Format::GeoJson, payload: NativePayload::GeoJson(vec![("id".into(), JsonValue::Int(7))]) }));
        every_layout_round_trips(vec![
            feature(&[("a", JsonValue::Int(1)), ("b", JsonValue::String("x".into()))]),
            // 1: absent is not null.
            feature(&[("a", JsonValue::Null)]),
            feature(&[]),
            // 2, 3, 6: its own order, a repeated key, a native.
            odd,
            // 4: a value's own type, per cell.
            feature(&[("a", JsonValue::String("1".into())), ("b", JsonValue::Int(i64::MIN))]),
            // 5: nested values.
            feature(&[(
                "a",
                JsonValue::Object(vec![("n".into(), JsonValue::Array(vec![JsonValue::Binary(vec![0, 255])]))]),
            )]),
            Feature::default(),
        ]);
    }

    #[test]
    fn regular_rows_seal_as_columns_and_ragged_ones_as_rows() {
        let rows = Sizes::DEFAULT.block_rows as i64;
        let regular: Vec<Feature> =
            (0..rows).map(|i| feature(&[("a", JsonValue::Int(i)), ("b", JsonValue::Bool(i % 2 == 0))])).collect();
        let table = Table::from_features(regular.clone(), Layout::Auto);
        assert!(matches!(table.blocks[..], [Block::Columns(_)]));
        assert_eq!(table.into_features(), regular);

        // Every row its own key: a dense table would hold a slot per row a row.
        let sparse: Vec<Feature> = (0..rows).map(|i| feature(&[(&*format!("k{i}"), JsonValue::Int(i))])).collect();
        let table = Table::from_features(sparse.clone(), Layout::Auto);
        assert!(matches!(table.blocks[..], [Block::Rows(_)]));
        assert_eq!(table.into_features(), sparse);
    }

    const LAYOUTS: [Layout; 3] = [Layout::Auto, Layout::Rows, Layout::Columns];

    /// Small blocks, so a few hundred rows cross several boundaries and a
    /// block is short enough for the interning judgement to run in it.
    const SMALL: Sizes = Sizes { block_rows: 64, intern_sample: 32 };

    /// What the property tests run: 300 seeds under [`SMALL`], and a few at
    /// the real size, each two full blocks and part of a third.
    fn cases() -> impl Iterator<Item = (u64, Sizes, Vec<Feature>, arbitrary::Rng)> {
        let small = (0..300).map(|seed| {
            let mut rng = arbitrary::Rng::new(seed);
            (seed, SMALL, arbitrary::collection(&mut rng, SMALL.block_rows), rng)
        });
        let real = (1_000..1_004).map(|seed| {
            let mut rng = arbitrary::Rng::new(seed);
            let rows = 2 * Sizes::DEFAULT.block_rows + rng.below(Sizes::DEFAULT.block_rows);
            (seed, Sizes::DEFAULT, arbitrary::collection_of(&mut rng, rows), rng)
        });
        small.chain(real)
    }

    /// C1's first property test: `into_features(from_features(x)) == x` for
    /// arbitrary collections, under every layout.
    #[test]
    fn arbitrary_collections_round_trip() {
        for (seed, sizes, features, _) in cases() {
            for layout in LAYOUTS {
                let back = Table::from_features_sized(features.clone(), layout, sizes).into_features();
                assert!(back == features, "seed {seed}, {sizes:?}, {layout:?}");
            }
        }
    }

    /// C2: a tabular reader's table, built a row or a column at a time, is
    /// the table its rows would make: the same blocks, in the same forms,
    /// holding the same columns -- so nothing downstream can tell them apart.
    #[test]
    fn a_tabular_reader_builds_the_table_its_rows_make() {
        let small = (0..300).map(|seed| (seed, SMALL, 64 * 3 + seed as usize % 70));
        let real = (1_000..1_002).map(|seed| (seed, Sizes::DEFAULT, 2 * Sizes::DEFAULT.block_rows + 17));
        for (seed, sizes, rows) in small.chain(real) {
            let mut rng = arbitrary::Rng::new(seed);
            let rows = rows.min(rng.below(rows + 1) + rows / 2);
            let (keys, features) = arbitrary::tabular(&mut rng, rows);
            for layout in LAYOUTS {
                let expected = Table::from_features_sized(features.clone(), layout, sizes);
                let mut builder = TableBuilder::sized(keys.clone(), layout, sizes);
                for f in features.clone() {
                    builder.push(f.geometry, f.properties.into_iter().map(|(_, v)| v), f.native);
                }
                assert!(builder.finish() == expected, "rows: seed {seed}, {sizes:?}, {layout:?}");
                if features.iter().any(|f| f.native.is_some()) {
                    continue;
                }
                let geometry = features.iter().map(|f| f.geometry.clone()).collect();
                let column = |i: usize| -> Result<Vec<JsonValue>, ()> {
                    Ok(features.iter().map(|f| f.properties[i].1.clone()).collect())
                };
                let built = Table::from_columns(keys.clone(), geometry, column, layout, sizes).unwrap();
                assert!(built == expected, "columns: seed {seed}, {sizes:?}, {layout:?}");
            }
        }
    }

    /// What every view says of a table, row by row, against the rows it was
    /// built from: each row whole, and each key through `get` and `column`.
    fn views_agree(table: &Table, rows: &[Feature], what: &str) {
        assert_eq!(table.len(), rows.len(), "{what}");
        let mut keys: Vec<&str> = rows.iter().flat_map(|f| f.properties.iter().map(|(k, _)| &**k)).collect();
        keys.sort_unstable();
        keys.dedup();
        keys.push("absent everywhere");
        let columns: Vec<ColumnRef<'_>> = keys.iter().map(|k| table.column(k)).collect();
        for (i, (view, row)) in table.rows().zip(rows).enumerate() {
            assert!(view.to_feature() == *row, "{what}: row {i}");
            assert_eq!(view.geometry(), row.geometry.as_ref(), "{what}: row {i}");
            assert_eq!(view.native(), row.native.as_deref(), "{what}: row {i}");
            let listed: Vec<(&str, JsonValue)> = view.properties().map(|(k, v)| (&**k, v.to_owned())).collect();
            let expected: Vec<(&str, JsonValue)> = row.properties.iter().map(|(k, v)| (&**k, v.clone())).collect();
            assert!(listed == expected, "{what}: row {i}");
            for (key, column) in keys.iter().zip(&columns) {
                // The first value under the key, as `schema.rs` reads a repeat.
                let first = row.properties.iter().find(|(k, _)| &**k == *key).map(|(_, v)| v.clone());
                assert!(view.get(key).map(Value::to_owned) == first, "{what}: row {i} get {key}");
                assert!(column.get(i).map(Value::to_owned) == first, "{what}: row {i} column {key}");
            }
        }
        // The cursor a writer reads a column through says what `get` does.
        for (key, column) in keys.iter().zip(&columns) {
            assert_eq!(column.values().len(), rows.len(), "{what}: {key}");
            assert!(column.values().eq((0..rows.len()).map(|i| column.get(i))), "{what}: values of {key}");
        }
    }

    #[test]
    fn views_read_what_the_rows_hold() {
        for (seed, sizes, features, _) in cases() {
            for layout in LAYOUTS {
                let table = Table::from_features_sized(features.clone(), layout, sizes);
                views_agree(&table, &features, &format!("seed {seed}, {sizes:?}, {layout:?}"));
            }
        }
    }

    /// `retain` and `permute`, any number and in any order, against the same
    /// edits made to a `Vec<Feature>`; then again after a gather.
    #[test]
    fn selections_match_the_same_edits_to_rows() {
        for (seed, sizes, features, mut rng) in cases() {
            for layout in LAYOUTS {
                let what = format!("seed {seed}, {sizes:?}, {layout:?}");
                let mut table = Table::from_features_sized(features.clone(), layout, sizes);
                let mut model = features.clone();
                for _ in 0..1 + rng.below(3) {
                    if rng.one_in(2) {
                        // Keep by content and by position, so a predicate that
                        // reads the view is tested as well as the bookkeeping.
                        let modulus = 2 + rng.below(3);
                        let mut i = 0;
                        let mut keep = |f: &Feature| {
                            i += 1;
                            f.geometry.is_none() || i % modulus != 0
                        };
                        model.retain(|f| keep(f));
                        let mut i = 0;
                        table.retain(|r| {
                            i += 1;
                            r.geometry().is_none() || i % modulus != 0
                        });
                    } else {
                        let mut order: Vec<usize> = (0..model.len()).collect();
                        rng.shuffle(&mut order);
                        let mut slots: Vec<Option<Feature>> = model.into_iter().map(Some).collect();
                        model = order.iter().map(|&i| slots[i].take().unwrap()).collect();
                        table.permute(&order);
                    }
                    views_agree(&table, &model, &what);
                }
                let mut gathered = table.clone();
                gathered.gather();
                assert!(gathered.selection.is_none(), "{what}");
                views_agree(&gathered, &model, &format!("{what}, gathered"));
                assert!(table.into_features() == model, "{what}");
            }
        }
    }

    /// The row IR's own edits, which the table's must match.
    mod model {
        use super::*;

        pub(super) fn set(row: &mut Feature, key: &str, value: JsonValue) {
            match row.properties.iter_mut().find(|(k, _)| &**k == key) {
                Some((_, held)) => *held = value,
                None => row.properties.push((Rc::from(key), value)),
            }
        }

        pub(super) fn remove(row: &mut Feature, key: &str) {
            row.properties.retain(|(k, _)| &**k != key);
        }

        pub(super) fn get<'a>(row: &'a Feature, key: &str) -> Option<&'a JsonValue> {
            row.properties.iter().find(|(k, _)| &**k == key).map(|(_, v)| v)
        }
    }

    /// Whether every block has the form a forced layout gives it. A fold that
    /// missed a block leaves its rows reading right, so only its form shows it.
    fn sealed_as(table: &Table, layout: Layout) -> bool {
        table.blocks.iter().all(|b| match (layout, b) {
            (Layout::Auto, _) => true,
            (Layout::Rows, Block::Rows(_)) | (Layout::Columns, Block::Columns(_)) => true,
            _ => false,
        })
    }

    #[test]
    fn a_fold_seals_every_block_pushes_reached() {
        let mut table = Table::from_features_sized(vec![feature(&[("a", JsonValue::Int(0))])], Layout::Columns, SMALL);
        let mut model = vec![feature(&[("a", JsonValue::Int(0))])];
        for i in 1..SMALL.block_rows as i64 * 2 + 3 {
            let added = feature(&[("a", JsonValue::Int(i)), ("b", JsonValue::Bool(true))]);
            table.push(added.clone());
            model.push(added);
        }
        assert_eq!(table.blocks.len(), 4, "the sealed block and three of appends");
        table.fold();
        assert!(sealed_as(&table, Layout::Columns));
        views_agree(&table, &model, "pushed and folded");
    }

    /// A key to edit: one the collection uses, now and then a new one.
    fn some_key(rng: &mut arbitrary::Rng, rows: &[Feature]) -> String {
        let used: Vec<&str> = rows.iter().flat_map(|f| f.properties.iter().map(|(k, _)| &**k)).take(64).collect();
        match used.is_empty() || rng.one_in(4) {
            true => format!("new{}", rng.below(3)),
            false => used[rng.below(used.len())].to_owned(),
        }
    }

    /// C1's second property test: random edit sequences -- every D-C12 edit,
    /// with `retain`, `permute`, `fold` and `gather` among them -- give the
    /// same rows as the same edits to a `Vec<Feature>`, under every layout.
    #[test]
    fn edit_sequences_match_the_same_edits_to_rows() {
        for (seed, sizes, features, mut rng) in cases() {
            for layout in LAYOUTS {
                let mut table = Table::from_features_sized(features.clone(), layout, sizes);
                let mut model = features.clone();
                let mut done = Vec::new();
                for _ in 0..12 {
                    let n = model.len();
                    let op = rng.below(if n == 0 { 3 } else { 12 });
                    match op {
                        0 => {
                            let added = arbitrary::collection_of(&mut rng, 1).remove(0);
                            table.push(added.clone());
                            model.push(added);
                        }
                        1 => {
                            table.fold();
                            assert!(sealed_as(&table, layout), "seed {seed}, {layout:?}: a fold left a block unsealed");
                        }
                        2 => {
                            table.gather();
                            assert!(table.selection.is_none() && !table.blocks.iter().any(Block::is_patched));
                            assert!(sealed_as(&table, layout), "seed {seed}, {layout:?}: a gather left a block unsealed");
                        }
                        3 | 4 => {
                            let (i, key, value) = (rng.below(n), some_key(&mut rng, &model), arbitrary::value(&mut rng, 0));
                            table.set(i, &key, value.clone());
                            model::set(&mut model[i], &key, value);
                        }
                        5 => {
                            let (i, key) = (rng.below(n), some_key(&mut rng, &model));
                            table.remove(i, &key);
                            model::remove(&mut model[i], &key);
                        }
                        6 => {
                            let i = rng.below(n);
                            let geometry = (!rng.one_in(3)).then(|| Geometry::Point(Position::new(0.5, i as f64)));
                            table.set_geometry(i, geometry.clone());
                            model[i].geometry = geometry;
                        }
                        7 => {
                            let key = some_key(&mut rng, &model);
                            let values: Vec<Option<JsonValue>> =
                                (0..n).map(|_| (!rng.one_in(4)).then(|| arbitrary::value(&mut rng, 0))).collect();
                            table.set_column(&key, values.clone());
                            for (row, value) in model.iter_mut().zip(values) {
                                match value {
                                    Some(value) => model::set(row, &key, value),
                                    None => model::remove(row, &key),
                                }
                            }
                        }
                        8 => {
                            // An integer where a float was, and back: `1` into a
                            // column of floats stays an integer.
                            let key = some_key(&mut rng, &model);
                            let f = |i: usize, old: Option<JsonValue>| match old {
                                Some(JsonValue::Float(x)) => Some(JsonValue::Int(x as i64)),
                                Some(JsonValue::Int(x)) => Some(JsonValue::Float(x as f64 + 0.5)),
                                Some(_) if i % 3 == 0 => None,
                                Some(other) => Some(other),
                                None => (i % 2 == 0).then_some(JsonValue::Int(1)),
                            };
                            table.map_column(&key, |i, old| f(i, old.map(Value::to_owned)));
                            for (i, row) in model.iter_mut().enumerate() {
                                match f(i, model::get(row, &key).cloned()) {
                                    Some(value) => model::set(row, &key, value),
                                    None => model::remove(row, &key),
                                }
                            }
                        }
                        9 => {
                            let key = some_key(&mut rng, &model);
                            table.drop_column(&key);
                            model.iter_mut().for_each(|row| model::remove(row, &key));
                        }
                        10 => {
                            let modulus = 2 + rng.below(4);
                            let mut i = 0;
                            table.retain(|_| {
                                i += 1;
                                i % modulus != 0
                            });
                            let mut i = 0;
                            model.retain(|_| {
                                i += 1;
                                i % modulus != 0
                            });
                        }
                        _ => {
                            let mut order: Vec<usize> = (0..n).collect();
                            rng.shuffle(&mut order);
                            table.permute(&order);
                            let mut slots: Vec<Option<Feature>> = std::mem::take(&mut model).into_iter().map(Some).collect();
                            model = order.iter().map(|&i| slots[i].take().unwrap()).collect();
                        }
                    }
                    done.push(op);
                    // Every view after every edit is most of the run at the
                    // real size, where the edits themselves take milliseconds;
                    // there, every fourth edit and the end.
                    if sizes == SMALL || done.len() % 4 == 0 {
                        views_agree(&table, &model, &format!("seed {seed}, {sizes:?}, {layout:?}, ops {done:?}"));
                    }
                }
                assert!(table.into_features() == model, "seed {seed}, {sizes:?}, {layout:?}, ops {done:?}");
            }
        }
    }

    #[test]
    fn rows_past_one_block_keep_their_order() {
        let features: Vec<Feature> = (0..Sizes::DEFAULT.block_rows as i64 * 2 + 5)
            .map(|i| match i % 3 {
                0 => feature(&[("a", JsonValue::Int(i))]),
                1 => feature(&[("b", JsonValue::Int(i)), ("a", JsonValue::Null)]),
                _ => feature(&[]),
            })
            .collect();
        let table = Table::from_features(features.clone(), Layout::Columns);
        assert_eq!(table.blocks.len(), 3);
        assert_eq!(table.len(), features.len());
        assert_eq!(table.into_features(), features);
    }
}
