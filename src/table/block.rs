//! A block: a run of rows held as shaped rows or as columns
//! (`plans/columnar.org` D-C10, D-C13).

use super::column::Column;
use super::kinds::Kinds;
use super::value::Value;
use super::Layout;
use crate::feature::Feature;
use crate::geometry::Geometry;
use crate::json::JsonValue;
use crate::native::Native;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::rc::Rc;

/// D-C7: the few rows that carry a per-feature native, sorted by row.
pub(crate) type RowNatives = Vec<(u32, Box<Native>)>;

/// D-C12: the rows an edit has changed, each its whole property list, sorted
/// by row. Views read through it until a fold rebuilds the block, so a column
/// promotes at most once per block however many edits reach it. Whole rows
/// rather than D-C12's `(row, key)` cells: an edit is then the row IR's own
/// edit to a `Vec`, and every guarantee about order and repeats holds by it.
type Patch = BTreeMap<u32, Vec<(Rc<str>, JsonValue)>>;

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Block {
    Rows(RowBlock),
    Columns(ColumnBlock),
}

/// Shaped rows (D-C13): each row's keys are an interned shape, and its
/// values sit back to back in `values`. Every guarantee holds by existing,
/// as it does in `Vec<Feature>`.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct RowBlock {
    geometry: Vec<Option<Geometry>>,
    /// Key lists, in order, repeats kept.
    shapes: Vec<Box<[Rc<str>]>>,
    row_shape: Vec<u32>,
    /// Row `i`'s values start at `starts[i]`.
    starts: Vec<u32>,
    values: Vec<JsonValue>,
    natives: RowNatives,
    patch: Patch,
}

/// Columns (D-C10): a slot per row in every column, in the block's column
/// order (D-C4), with the rows that deviate from it in `shapes`.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct ColumnBlock {
    geometry: Vec<Option<Geometry>>,
    columns: Vec<Column>,
    /// D-C4: the irregular rows, sorted by row.
    shapes: Vec<(u32, Box<[Member]>)>,
    natives: RowNatives,
    patch: Patch,
    /// The columns in the order the block's rows first use them, which a
    /// tabular writer declares (D-C4, D-C10): not the column order, which is
    /// the modal row's.
    first_seen: Box<[u32]>,
}

/// One property of an irregular row, in the row's own order.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Member {
    /// The value is in this column.
    Column(u32),
    /// A key the row repeats (guarantee 3): the extra pair itself.
    Extra(Rc<str>, JsonValue),
}

impl Block {
    pub(crate) fn len(&self) -> usize {
        match self {
            Block::Rows(b) => b.geometry.len(),
            Block::Columns(b) => b.geometry.len(),
        }
    }

    #[cfg(test)]
    pub(crate) fn into_features(self, out: &mut Vec<Feature>) {
        match self {
            Block::Rows(b) => b.into_features(out),
            Block::Columns(b) => b.into_features(out),
        }
    }

    pub(crate) fn geometry(&self, row: usize) -> Option<&Geometry> {
        match self {
            Block::Rows(b) => b.geometry[row].as_ref(),
            Block::Columns(b) => b.geometry[row].as_ref(),
        }
    }

    pub(crate) fn native(&self, row: usize) -> Option<&Native> {
        let natives = match self {
            Block::Rows(b) => &b.natives,
            Block::Columns(b) => &b.natives,
        };
        let i = natives.binary_search_by_key(&(row as u32), |(r, _)| *r).ok()?;
        Some(&natives[i].1)
    }

    /// Row `row`'s properties, in its own order, repeats kept.
    pub(crate) fn properties(&self, row: usize) -> Properties<'_> {
        if let Some(patched) = self.patch().get(&(row as u32)) {
            return Properties::Patched(patched.iter());
        }
        match self {
            Block::Rows(b) => {
                let keys = &b.shapes[b.row_shape[row] as usize];
                let start = b.starts[row] as usize;
                Properties::Shaped { keys: keys.iter(), values: b.values[start..start + keys.len()].iter() }
            }
            Block::Columns(b) => match b.shapes.binary_search_by_key(&(row as u32), |(r, _)| *r) {
                Ok(i) => Properties::Irregular { members: b.shapes[i].1.iter(), columns: &b.columns, row },
                Err(_) => Properties::Regular { columns: b.columns.iter(), row },
            },
        }
    }

    /// Where each of `keys` (by their index) is held in this block, found in
    /// one pass: its column in a column block, and in a row block, its
    /// position in each shape that has it.
    pub(crate) fn slots(&self, keys: &HashMap<&str, usize>) -> Vec<Slot> {
        match self {
            Block::Columns(b) => {
                let mut slots = vec![Slot::Column(None); keys.len()];
                for (c, column) in b.columns.iter().enumerate() {
                    if let Some(&k) = keys.get(&*column.name) {
                        slots[k] = Slot::Column(Some(c));
                    }
                }
                slots
            }
            Block::Rows(b) => {
                let words = b.shapes.len().div_ceil(64);
                let mut at: Vec<ShapeSlots> = vec![ShapeSlots::default(); keys.len()];
                for (s, shape) in b.shapes.iter().enumerate() {
                    for (pos, key) in shape.iter().enumerate() {
                        let Some(&k) = keys.get(&**key) else { continue };
                        let slots = &mut at[k];
                        // A repeat is not the row's value for its key.
                        if slots.at.last().is_none_or(|&(last, _)| last != s as u32) {
                            if slots.has.is_empty() {
                                slots.has = vec![0; words];
                            }
                            slots.has[s / 64] |= 1 << (s % 64);
                            slots.at.push((s as u32, pos as u32));
                        }
                    }
                }
                at.into_iter().map(Slot::Shapes).collect()
            }
        }
    }

    /// Row `row`'s first value for the key `slot` locates, or `None` when
    /// the row has none. `key` is the key itself, for a patched row.
    pub(crate) fn value_at(&self, row: usize, slot: &Slot, key: &str) -> Option<Value<'_>> {
        if self.is_row_patched(row) {
            return self.properties(row).find(|(k, _)| &***k == key).map(|(_, v)| v);
        }
        self.sealed_value_at(row, slot)
    }

    /// [`Self::value_at`] for a row no edit has reached.
    pub(crate) fn sealed_value_at(&self, row: usize, slot: &Slot) -> Option<Value<'_>> {
        match (self, slot) {
            (Block::Columns(b), Slot::Column(Some(c))) => {
                let column = &b.columns[*c];
                column.is_present(row).then(|| column.value(row))
            }
            (Block::Rows(b), Slot::Shapes(slots)) => {
                let shape = b.row_shape[row];
                // Most rows of a sparse block lack most keys: a bit says so
                // without searching.
                if slots.has.get(shape as usize / 64).is_none_or(|w| w & (1 << (shape % 64)) == 0) {
                    return None;
                }
                let i = slots.at.binary_search_by_key(&shape, |&(s, _)| s).ok()?;
                Some(Value::of(&b.values[b.starts[row] as usize + slots.at[i].1 as usize]))
            }
            _ => None,
        }
    }

    /// The block's keys in the order its rows first use them. Read off its
    /// shapes or its recorded order; a patched row's new keys are not in it.
    pub(crate) fn first_seen(&self) -> Vec<&Rc<str>> {
        match self {
            Block::Rows(b) => {
                let mut seen = HashSet::new();
                b.shapes.iter().flat_map(|s| s.iter()).filter(|k| seen.insert(&***k)).collect()
            }
            Block::Columns(b) => b.first_seen.iter().map(|&c| &b.columns[c as usize].name).collect(),
        }
    }

    /// Report each key's kinds of value to `found`, a column at a time where
    /// the block is columns; a row block, or a patched one, row by row.
    pub(crate) fn kinds<'a>(&'a self, found: &mut dyn FnMut(&'a Rc<str>, Kinds)) {
        match self {
            Block::Columns(b) if b.patch.is_empty() => {
                for column in &b.columns {
                    found(&column.name, column.kinds());
                }
                // A repeat's value counts toward its key's type, as the row
                // inference counts it.
                for (_, members) in &b.shapes {
                    for member in members {
                        if let Member::Extra(key, value) = member {
                            found(key, Kinds::of(Value::of(value)));
                        }
                    }
                }
            }
            _ => {
                for row in 0..self.len() {
                    for (key, value) in self.properties(row) {
                        found(key, Kinds::of(value));
                    }
                }
            }
        }
    }

    fn patch(&self) -> &Patch {
        match self {
            Block::Rows(b) => &b.patch,
            Block::Columns(b) => &b.patch,
        }
    }

    fn patch_mut(&mut self) -> &mut Patch {
        match self {
            Block::Rows(b) => &mut b.patch,
            Block::Columns(b) => &mut b.patch,
        }
    }

    /// Whether an edit has changed any row's properties since the block was
    /// sealed.
    pub(crate) fn is_patched(&self) -> bool {
        !self.patch().is_empty()
    }

    /// Whether row `row`'s properties are read from the patch.
    pub(crate) fn is_row_patched(&self, row: usize) -> bool {
        self.patch().contains_key(&(row as u32))
    }

    /// Change row `row`'s properties as `edit` says, through the patch.
    fn edit(&mut self, row: usize, edit: impl FnOnce(&mut Vec<(Rc<str>, JsonValue)>)) {
        if !self.is_row_patched(row) {
            let whole = self.properties(row).map(|(k, v)| (k.clone(), v.to_owned())).collect();
            self.patch_mut().insert(row as u32, whole);
        }
        edit(self.patch_mut().get_mut(&(row as u32)).expect("inserted above"));
    }

    /// Give row `row`'s first `key` the value `value`, in its place; a row
    /// without the key gains it last (D-C12).
    pub(crate) fn set(&mut self, row: usize, key: &str, value: JsonValue) {
        // A row block takes an edit that keeps the row's shape in place.
        if let Block::Rows(b) = self
            && !b.patch.contains_key(&(row as u32))
            && let Some(at) = b.shapes[b.row_shape[row] as usize].iter().position(|k| &**k == key)
        {
            b.values[b.starts[row] as usize + at] = value;
            return;
        }
        self.edit(row, |members| match members.iter_mut().find(|(k, _)| &**k == key) {
            Some((_, held)) => *held = value,
            None => members.push((Rc::from(key), value)),
        });
    }

    /// Take every `key` out of row `row`.
    pub(crate) fn remove(&mut self, row: usize, key: &str) {
        if self.properties(row).any(|(k, _)| &**k == key) {
            self.edit(row, |members| members.retain(|(k, _)| &**k != key));
        }
    }

    /// Give row `row` the native `native`, or none.
    pub(crate) fn set_native(&mut self, row: usize, native: Option<Native>) {
        let natives = match self {
            Block::Rows(b) => &mut b.natives,
            Block::Columns(b) => &mut b.natives,
        };
        let row = row as u32;
        match (natives.binary_search_by_key(&row, |(r, _)| *r), native) {
            (Ok(i), Some(native)) => natives[i].1 = Box::new(native),
            (Ok(i), None) => {
                natives.remove(i);
            }
            (Err(i), Some(native)) => natives.insert(i, (row, Box::new(native))),
            (Err(_), None) => {}
        }
    }

    /// Drop each per-row native `drop` says to.
    pub(crate) fn drop_natives(&mut self, drop: &mut dyn FnMut(&Native) -> bool) {
        let natives = match self {
            Block::Rows(b) => &mut b.natives,
            Block::Columns(b) => &mut b.natives,
        };
        natives.retain(|(_, n)| !drop(n));
    }

    pub(crate) fn geometries_mut(&mut self) -> &mut [Option<Geometry>] {
        match self {
            Block::Rows(b) => &mut b.geometry,
            Block::Columns(b) => &mut b.geometry,
        }
    }

    pub(crate) fn set_geometry(&mut self, row: usize, geometry: Option<Geometry>) {
        match self {
            Block::Rows(b) => b.geometry[row] = geometry,
            Block::Columns(b) => b.geometry[row] = geometry,
        }
    }

    /// Append a feature to a row block (D-C12's tail block).
    pub(crate) fn push(&mut self, feature: Feature) {
        let Block::Rows(b) = self else { unreachable!("features are appended to a row block") };
        let mut builder = RowBuilder::resume(std::mem::take(b));
        builder.push(feature);
        *b = builder.block;
    }

    #[cfg(test)]
    /// The block rebuilt from its rows as they now read, sealed again: the
    /// patch folded in, and a tail that took appends given its form.
    pub(crate) fn fold(self, layout: Layout, sizes: Sizes) -> Block {
        let mut features = Vec::with_capacity(self.len());
        self.into_features(&mut features);
        let mut builder = RowBuilder::new(sizes.intern_sample);
        for feature in features {
            builder.push(feature);
        }
        builder.seal(layout)
    }
}

/// Where a block holds one key, for reading it a row at a time
/// ([`Block::slots`]).
#[derive(Debug, Clone)]
pub(crate) enum Slot {
    /// The column holding it in a column block, if any.
    Column(Option<usize>),
    /// In a row block, the shapes holding it.
    Shapes(ShapeSlots),
}

/// [`Slot::Shapes`]: the shapes holding a key, sorted, with its position in
/// each, and a bit per shape saying which do -- empty when none.
#[derive(Debug, Clone, Default)]
pub(crate) struct ShapeSlots {
    at: Vec<(u32, u32)>,
    has: Vec<u64>,
}

/// [`Block::properties`]: one row's members, whichever form holds them.
pub(crate) enum Properties<'a> {
    /// A row an edit changed, read from the patch.
    Patched(std::slice::Iter<'a, (Rc<str>, JsonValue)>),
    Shaped { keys: std::slice::Iter<'a, Rc<str>>, values: std::slice::Iter<'a, JsonValue> },
    /// A regular row of a column block: its present columns, in order.
    Regular { columns: std::slice::Iter<'a, Column>, row: usize },
    Irregular { members: std::slice::Iter<'a, Member>, columns: &'a [Column], row: usize },
}

impl<'a> Iterator for Properties<'a> {
    type Item = (&'a Rc<str>, Value<'a>);

    fn next(&mut self) -> Option<Self::Item> {
        match self {
            Properties::Patched(members) => members.next().map(|(k, v)| (k, Value::of(v))),
            Properties::Shaped { keys, values } => Some((keys.next()?, Value::of(values.next()?))),
            Properties::Regular { columns, row } => {
                columns.find(|c| c.is_present(*row)).map(|c| (&c.name, c.value(*row)))
            }
            Properties::Irregular { members, columns, row } => Some(match members.next()? {
                Member::Column(c) => {
                    let column = &columns[*c as usize];
                    (&column.name, column.value(*row))
                }
                Member::Extra(key, value) => (key, Value::of(value)),
            }),
        }
    }
}

/// How big a block is, and how much of one the builder sees before judging
/// whether interning pays. A parameter rather than constants so tests can
/// run small blocks, which cross many boundaries cheaply, through the same
/// code as the real size.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Sizes {
    /// Rows a block holds before it seals.
    pub(crate) block_rows: usize,
    /// Rows seen before the builder judges whether interning pays.
    pub(crate) intern_sample: usize,
}

impl Sizes {
    /// § SMOKE TEST saw little between 8 192 and 65 536 rows except at a
    /// stretch boundary, where the smaller block wasted less; the choice
    /// stays open until C3's profile (§ OPEN QUESTIONS).
    pub(crate) const DEFAULT: Sizes = Sizes { block_rows: 8_192, intern_sample: 1_024 };
}

impl Default for Sizes {
    fn default() -> Sizes {
        Sizes::DEFAULT
    }
}

/// Fills a row block a feature at a time.
#[derive(Default)]
pub(crate) struct RowBuilder {
    block: RowBlock,
    index: HashMap<Box<[Rc<str>]>, u32>,
    /// Stops hashing once shapes stop repeating (D-C13): on sparse input
    /// every row's keys are its own, and interning bought 2% for 4× the time.
    interning: bool,
    intern_sample: usize,
    keys: Vec<Rc<str>>,
}

impl RowBuilder {
    pub(crate) fn new(intern_sample: usize) -> RowBuilder {
        RowBuilder { interning: true, intern_sample, ..RowBuilder::default() }
    }

    /// Append to a sealed row block. Without its index there is nothing to
    /// intern against, so a new row reuses its predecessor's shape or adds
    /// its own; a fold re-interns.
    fn resume(block: RowBlock) -> RowBuilder {
        RowBuilder { block, ..RowBuilder::default() }
    }

    pub(crate) fn len(&self) -> usize {
        self.block.geometry.len()
    }

    pub(crate) fn push(&mut self, feature: Feature) {
        let row = self.len() as u32;
        let b = &mut self.block;
        b.geometry.push(feature.geometry);
        if let Some(native) = feature.native {
            b.natives.push((row, native));
        }
        b.starts.push(b.values.len() as u32);
        self.keys.clear();
        for (key, value) in feature.properties {
            self.keys.push(key);
            b.values.push(value);
        }
        // A row with its predecessor's keys reuses the shape without hashing,
        // which is most rows of any regular input.
        if let Some(&last) = b.row_shape.last()
            && *b.shapes[last as usize] == *self.keys
        {
            b.row_shape.push(last);
            return;
        }
        let shape = match self.interning.then(|| self.index.get(&*self.keys)).flatten() {
            Some(&shape) => shape,
            None => {
                let shape = b.shapes.len() as u32;
                let keys: Box<[Rc<str>]> = self.keys.as_slice().into();
                if self.interning {
                    self.index.insert(keys.clone(), shape);
                }
                b.shapes.push(keys);
                shape
            }
        };
        b.row_shape.push(shape);
        let sample = self.intern_sample;
        if self.interning && b.row_shape.len() == sample && b.shapes.len() > sample / 2 {
            self.interning = false;
            self.index = HashMap::new();
        }
    }

    /// Seal the block as `layout` asks: as rows, as columns, or by D-C13's
    /// rule.
    pub(crate) fn seal(self, layout: Layout) -> Block {
        let block = self.block;
        let plan = match layout {
            Layout::Rows => None,
            Layout::Columns => Some(ColumnPlan::of(&block)),
            Layout::Auto => Some(ColumnPlan::of(&block)).filter(|p| p.pays(block.geometry.len())),
        };
        match plan {
            Some(plan) => Block::Columns(block.transpose(plan)),
            None => Block::Rows(block),
        }
    }
}

/// Fills a block a tabular reader's rows at a time (C2): every row holds
/// every key, in the reader's declared order, so each value goes straight
/// into its column with no shape to intern and nothing to transpose.
pub(crate) struct TabularBlock {
    geometry: Vec<Option<Geometry>>,
    /// One per declared key, repeats included, in declared order.
    columns: Vec<Column>,
    natives: RowNatives,
}

impl TabularBlock {
    pub(crate) fn new(keys: &[Rc<str>]) -> TabularBlock {
        TabularBlock { geometry: Vec::new(), columns: keys.iter().cloned().map(Column::new).collect(), natives: Vec::new() }
    }

    pub(crate) fn len(&self) -> usize {
        self.geometry.len()
    }

    /// Append a row: its geometry, a value for each key in declared order,
    /// and its native.
    pub(crate) fn push(
        &mut self,
        geometry: Option<Geometry>,
        values: impl IntoIterator<Item = JsonValue>,
        native: Option<Box<Native>>,
    ) {
        if let Some(native) = native {
            self.natives.push((self.len() as u32, native));
        }
        self.geometry.push(geometry);
        let mut values = values.into_iter();
        for column in &mut self.columns {
            column.push(Some(values.next().expect("a tabular row holds a value for every key")));
        }
        debug_assert!(values.next().is_none(), "a tabular row holds a value for no other key");
    }

    /// Append the rows' geometry alone, for a reader that then fills the
    /// columns one at a time ([`Self::column`]).
    pub(crate) fn push_geometry(&mut self, geometry: Option<Geometry>) {
        self.geometry.push(geometry);
    }

    /// Key `i`'s column, to fill with a value per row.
    pub(crate) fn column(&mut self, i: usize, values: impl IntoIterator<Item = JsonValue>) {
        let column = &mut self.columns[i];
        for value in values {
            column.push(Some(value));
        }
        debug_assert_eq!(column.len(), self.geometry.len(), "a tabular column holds a value per row");
    }

    /// Seal the block as `layout` asks. With its keys distinct it is the
    /// column block a row builder would transpose these rows into -- one
    /// shape, so its order and first-seen order are the declared order, and
    /// D-C13 always chooses columns for it. A key declared twice, or rows
    /// asked for, goes the row builder's way.
    pub(crate) fn seal(self, distinct: bool, layout: Layout, intern_sample: usize) -> Block {
        if distinct && layout != Layout::Rows {
            let first_seen = (0..self.columns.len() as u32).collect();
            return Block::Columns(ColumnBlock {
                geometry: self.geometry,
                columns: self.columns,
                shapes: Vec::new(),
                natives: self.natives,
                patch: Patch::new(),
                first_seen,
            });
        }
        let mut builder = RowBuilder::new(intern_sample);
        self.into_rows(&mut builder);
        builder.seal(layout)
    }

    /// Push every row to `builder`, as the features they were.
    pub(crate) fn into_rows(self, builder: &mut RowBuilder) {
        let TabularBlock { geometry, mut columns, natives } = self;
        let mut natives = natives.into_iter().peekable();
        for (row, geometry) in geometry.into_iter().enumerate() {
            let properties = columns.iter_mut().map(|c| (c.name.clone(), c.take(row))).collect();
            let native = natives.next_if(|(r, _)| *r as usize == row).map(|(_, n)| n);
            builder.push(Feature { geometry, properties, native });
        }
    }
}

/// What transposing a row block would build, read off its shapes alone.
struct ColumnPlan {
    /// The block's column order (D-C4): its modal shape's keys, then every
    /// other key in first-seen order.
    names: Vec<Rc<str>>,
    /// Per shape, whether it is regular: its keys are columns in column order,
    /// each once.
    regular: Vec<bool>,
    /// Rows whose shape is irregular.
    irregular_rows: usize,
    /// Present cells, counting a repeated key once.
    present_cells: usize,
}

impl ColumnPlan {
    fn of(block: &RowBlock) -> ColumnPlan {
        let mut rows_of = vec![0usize; block.shapes.len()];
        for &s in &block.row_shape {
            rows_of[s as usize] += 1;
        }
        // Shapes are interned in the order rows first use them, so walking
        // them in index order meets keys in first-seen order.
        let modal = (0..rows_of.len()).max_by_key(|&s| (rows_of[s], std::cmp::Reverse(s)));
        let mut index: HashMap<Rc<str>, u32> = HashMap::new();
        let mut names = Vec::new();
        for s in modal.into_iter().chain(0..block.shapes.len()) {
            for key in &block.shapes[s] {
                if !index.contains_key(key) {
                    index.insert(key.clone(), names.len() as u32);
                    names.push(key.clone());
                }
            }
        }
        let mut regular = Vec::with_capacity(block.shapes.len());
        let (mut irregular_rows, mut present_cells) = (0, 0);
        for (s, keys) in block.shapes.iter().enumerate() {
            let cols: Vec<u32> = keys.iter().map(|k| index[k]).collect();
            let is_regular = cols.windows(2).all(|w| w[0] < w[1]);
            let mut distinct = cols.clone();
            distinct.sort_unstable();
            distinct.dedup();
            present_cells += rows_of[s] * distinct.len();
            if !is_regular {
                irregular_rows += rows_of[s];
            }
            regular.push(is_regular);
        }
        ColumnPlan { names, regular, irregular_rows, present_cells }
    }

    /// D-C13's starting rule from § SMOKE TEST: columns when the block is at
    /// least a quarter dense and at most a quarter of its rows deviate from
    /// its modal order. On every smoke-test fixture it chose what building
    /// both and keeping the smaller chose.
    fn pays(&self, rows: usize) -> bool {
        let slots = rows * self.names.len();
        let dense = slots == 0 || self.present_cells * 4 >= slots;
        dense && self.irregular_rows * 4 <= rows
    }
}

impl RowBlock {
    /// Move every value into columns, mapping each row through its shape's
    /// column indices -- no per-cell key lookup, which ran 3--9× slower in
    /// § SMOKE TEST.
    fn transpose(self, plan: ColumnPlan) -> ColumnBlock {
        let index: HashMap<&str, u32> = plan.names.iter().enumerate().map(|(i, k)| (&**k, i as u32)).collect();
        let shape_cols: Vec<Box<[u32]>> =
            self.shapes.iter().map(|keys| keys.iter().map(|k| index[&**k]).collect()).collect();
        let mut columns: Vec<Column> = plan.names.iter().cloned().map(Column::new).collect();
        // Shapes are interned in the order rows first use them.
        let mut seen = HashSet::new();
        let first_seen = shape_cols.iter().flat_map(|s| s.iter().copied()).filter(|&c| seen.insert(c)).collect();
        let mut shapes = Vec::new();
        // Per column, the row's value for it, if the row holds the key.
        let mut slot: Vec<Option<JsonValue>> = vec![None; columns.len()];
        let mut values = self.values.into_iter();
        for (row, &s) in self.row_shape.iter().enumerate() {
            let (cols, keys) = (&shape_cols[s as usize], &self.shapes[s as usize]);
            let mut members = Vec::new();
            for (&c, key) in cols.iter().zip(keys) {
                let value = values.next().expect("a row's values match its shape");
                let cell = &mut slot[c as usize];
                if plan.regular[s as usize] {
                    *cell = Some(value);
                } else if cell.is_none() {
                    *cell = Some(value);
                    members.push(Member::Column(c));
                } else {
                    members.push(Member::Extra(key.clone(), value));
                }
            }
            for (column, cell) in columns.iter_mut().zip(&mut slot) {
                column.push(cell.take());
            }
            if !plan.regular[s as usize] {
                shapes.push((row as u32, members.into_boxed_slice()));
            }
        }
        ColumnBlock { geometry: self.geometry, columns, shapes, natives: self.natives, patch: Patch::new(), first_seen }
    }

    #[cfg(test)]
    fn into_features(mut self, out: &mut Vec<Feature>) {
        let mut values = self.values.into_iter();
        let mut natives = self.natives.into_iter().peekable();
        for (row, (geometry, &s)) in self.geometry.into_iter().zip(&self.row_shape).enumerate() {
            let keys = &self.shapes[s as usize];
            let held = values.by_ref().take(keys.len());
            let properties = match self.patch.remove(&(row as u32)) {
                Some(patched) => {
                    held.for_each(drop);
                    patched
                }
                None => keys.iter().cloned().zip(held).collect(),
            };
            let native = natives.next_if(|(r, _)| *r as usize == row).map(|(_, n)| n);
            out.push(Feature { geometry, properties, native });
        }
    }
}

impl ColumnBlock {
    #[cfg(test)]
    fn into_features(mut self, out: &mut Vec<Feature>) {
        let mut shapes = self.shapes.into_iter().peekable();
        let mut natives = self.natives.into_iter().peekable();
        for (row, geometry) in self.geometry.into_iter().enumerate() {
            let irregular = shapes.next_if(|(r, _)| *r as usize == row);
            let properties = match (self.patch.remove(&(row as u32)), irregular) {
                (Some(patched), _) => patched,
                (None, Some((_, members))) => members
                    .into_iter()
                    .map(|m| match m {
                        Member::Column(c) => {
                            let column = &mut self.columns[c as usize];
                            (column.name.clone(), column.take(row))
                        }
                        Member::Extra(key, value) => (key, value),
                    })
                    .collect(),
                (None, None) => self
                    .columns
                    .iter_mut()
                    .filter(|c| c.is_present(row))
                    .map(|c| (c.name.clone(), c.take(row)))
                    .collect(),
            };
            let native = natives.next_if(|(r, _)| *r as usize == row).map(|(_, n)| n);
            out.push(Feature { geometry, properties, native });
        }
    }
}
