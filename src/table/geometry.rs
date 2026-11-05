//! A block's geometry (`plans/columnar.org` D-C6, D-C16): owned, as the row
//! IR held it, or flat -- coordinates in buffers, structure in offsets --
//! with what an edit replaced read from an overlay.

use super::bitmap::Bitmap;
use super::Layout;
use crate::geometry::{Bbox, Geometry, Position};
use std::collections::BTreeMap;

/// One block's geometry column.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum GeometryColumn {
    /// A geometry a row, as the row IR held them: what a block builds into,
    /// and what `--layout rows` keeps (D-C16).
    Owned(Vec<Option<Geometry>>),
    /// Flat buffers (D-C6), what a block seals as.
    Flat(Box<Flat>),
}

impl Default for GeometryColumn {
    fn default() -> GeometryColumn {
        GeometryColumn::Owned(Vec::new())
    }
}

impl GeometryColumn {
    pub(crate) fn len(&self) -> usize {
        match self {
            GeometryColumn::Owned(g) => g.len(),
            GeometryColumn::Flat(f) => f.len(),
        }
    }

    pub(crate) fn push(&mut self, geometry: Option<Geometry>) {
        match self {
            GeometryColumn::Owned(g) => g.push(geometry),
            GeometryColumn::Flat(f) => f.push(geometry),
        }
    }

    /// The column in the form a block sealed under `layout` holds: flat,
    /// unless every block is to be rows.
    pub(crate) fn seal(self, layout: Layout) -> GeometryColumn {
        match (self, layout) {
            (GeometryColumn::Owned(g), Layout::Auto | Layout::Columns) => {
                let mut flat = Flat::with_capacity(g.len());
                g.into_iter().for_each(|g| flat.push(g));
                flat.shrink();
                GeometryColumn::Flat(Box::new(flat))
            }
            (column, _) => column,
        }
    }

    pub(crate) fn get(&self, row: usize) -> Option<GeometryRef<'_>> {
        match self {
            GeometryColumn::Owned(g) => g[row].as_ref().map(GeometryRef::owned),
            GeometryColumn::Flat(f) => f.get(row),
        }
    }

    /// Row `row`'s geometry as an owned value, kept as `f` leaves it: in
    /// place when owned, through the overlay when flat (D-C16).
    pub(crate) fn edit<R>(&mut self, row: usize, f: impl FnOnce(&mut Option<Geometry>) -> R) -> R {
        match self {
            GeometryColumn::Owned(g) => f(&mut g[row]),
            GeometryColumn::Flat(flat) => {
                if let Some(edited) = flat.edited.get_mut(&(row as u32)) {
                    return f(edited);
                }
                // Copied out, and kept only when `f` changed it, so a pass
                // that edits a few rows overlays those few.
                let before = flat.get(row).map(GeometryRef::to_geometry);
                let mut after = before.clone();
                let r = f(&mut after);
                if !identical(&before, &after) {
                    flat.edited.insert(row as u32, after);
                }
                r
            }
        }
    }

    pub(crate) fn set(&mut self, row: usize, geometry: Option<Geometry>) {
        match self {
            GeometryColumn::Owned(g) => g[row] = geometry,
            GeometryColumn::Flat(f) => {
                f.edited.insert(row as u32, geometry);
            }
        }
    }

    /// Make the column owned, for a pass that edits many of its rows: the
    /// pass then edits each in place, and the overlay never holds the column
    /// twice over (D-C16). Returns whether it was flat, for [`Self::reseal`].
    pub(crate) fn make_owned(&mut self) -> bool {
        match std::mem::take(self) {
            GeometryColumn::Owned(g) => {
                *self = GeometryColumn::Owned(g);
                false
            }
            GeometryColumn::Flat(f) => {
                *self = GeometryColumn::Owned(f.into_owned());
                true
            }
        }
    }

    /// Flatten a column [`Self::make_owned`] found flat.
    pub(crate) fn reseal(&mut self, was_flat: bool) {
        if was_flat {
            *self = std::mem::take(self).seal(Layout::Auto);
        }
    }

    /// The column as owned geometry, a row each.
    #[cfg(test)]
    pub(crate) fn into_owned(self) -> Vec<Option<Geometry>> {
        match self {
            GeometryColumn::Owned(g) => g,
            GeometryColumn::Flat(f) => f.into_owned(),
        }
    }

    /// Every position of every row, in row order, by mutable reference.
    pub(crate) fn for_each_position_mut(&mut self, f: &mut dyn FnMut(&mut Position)) {
        match self {
            GeometryColumn::Owned(g) => g.iter_mut().flatten().for_each(|g| g.for_each_position_mut(&mut *f)),
            GeometryColumn::Flat(flat) => flat.for_each_position_mut(f),
        }
    }

    /// Every run of positions of every row, in row order, by mutable slice:
    /// the runs [`Geometry::for_each_position_run_mut`] gives.
    pub(crate) fn for_each_position_run_mut(&mut self, f: &mut dyn FnMut(&mut [Position])) {
        match self {
            GeometryColumn::Owned(g) => g.iter_mut().flatten().for_each(|g| g.for_each_position_run_mut(&mut *f)),
            GeometryColumn::Flat(flat) => flat.for_each_position_run_mut(f),
        }
    }
}

/// What a row's geometry is, in a flat column.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
enum Tag {
    None,
    Point,
    LineString,
    Polygon,
    MultiPoint,
    MultiLineString,
    MultiPolygon,
    /// Held whole, in [`Flat::collections`].
    Collection,
}

/// An ordinate held for some positions only: a value a position, and which
/// positions have one.
#[derive(Debug, Clone, Default, PartialEq)]
struct Ordinate {
    /// Empty while no position has one; a value a position after.
    values: Vec<f64>,
    /// Which positions have one, once some position lacks one; `None`
    /// while every position since the first value has one.
    present: Option<Bitmap>,
}

impl Ordinate {
    fn get(&self, i: usize) -> Option<f64> {
        if self.values.is_empty() || self.present.as_ref().is_some_and(|p| !p.get(i)) {
            return None;
        }
        Some(self.values[i])
    }

    /// Append position `len`'s value, `len` positions being held before it.
    fn push(&mut self, len: usize, value: Option<f64>) {
        match (value, self.values.is_empty()) {
            (None, true) => {}
            (Some(v), true) => {
                if len > 0 {
                    self.values = vec![0.0; len];
                    self.present = Some(Bitmap::filled(len, false));
                }
                self.values.push(v);
                if let Some(p) = &mut self.present {
                    p.push(true);
                }
            }
            (value, false) => {
                self.values.push(value.unwrap_or(0.0));
                match (&mut self.present, value) {
                    (Some(p), _) => p.push(value.is_some()),
                    (None, Some(_)) => {}
                    (None, None) => {
                        let mut p = Bitmap::filled(len, true);
                        p.push(false);
                        self.present = Some(p);
                    }
                }
            }
        }
    }

    /// Give position `i` of `len` the value `value`, or none.
    fn set(&mut self, i: usize, len: usize, value: Option<f64>) {
        if self.get(i).map(f64::to_bits) == value.map(f64::to_bits) {
            return;
        }
        if self.values.is_empty() {
            self.values = vec![0.0; len];
            self.present = Some(Bitmap::filled(len, false));
        }
        self.values[i] = value.unwrap_or(0.0);
        if self.present.is_none() && value.is_none() {
            self.present = Some(Bitmap::filled(len, true));
        }
        if let Some(p) = &mut self.present {
            p.set(i, value.is_some());
        }
    }
}

/// Flat geometry (D-C6): a tag a row; the row's parts, each part's rings
/// and each ring's positions as offsets, the same three levels for every
/// type; X and Y in one buffer, Z and M each in their own.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Flat {
    tags: Vec<Tag>,
    /// Row `r`'s parts are `parts[r]..parts[r + 1]`.
    parts: Vec<u32>,
    /// Part `p`'s rings are `rings[p]..rings[p + 1]`.
    rings: Vec<u32>,
    /// Ring `k`'s positions are `coords[k]..coords[k + 1]`.
    coords: Vec<u32>,
    xy: Vec<[f64; 2]>,
    z: Ordinate,
    m: Ordinate,
    /// The rows holding a `GeometryCollection`, sorted by row: rare, so kept
    /// whole (D-C6).
    collections: Vec<(u32, Geometry)>,
    /// The rows an edit has replaced, read in place of the buffers until the
    /// column is next rebuilt (D-C12, D-C16).
    edited: BTreeMap<u32, Option<Geometry>>,
}

impl Flat {
    fn with_capacity(rows: usize) -> Flat {
        let mut parts = Vec::with_capacity(rows + 1);
        parts.push(0);
        Flat {
            tags: Vec::with_capacity(rows),
            parts,
            rings: vec![0],
            coords: vec![0],
            xy: Vec::with_capacity(rows),
            z: Ordinate::default(),
            m: Ordinate::default(),
            collections: Vec::new(),
            edited: BTreeMap::new(),
        }
    }

    fn len(&self) -> usize {
        self.tags.len()
    }

    /// Give back what the buffers grew past their length while filling: up
    /// to half of each, which on polygon-heavy input is most of the saving.
    fn shrink(&mut self) {
        self.tags.shrink_to_fit();
        self.parts.shrink_to_fit();
        self.rings.shrink_to_fit();
        self.coords.shrink_to_fit();
        self.xy.shrink_to_fit();
        self.z.values.shrink_to_fit();
        self.m.values.shrink_to_fit();
    }

    fn position(&mut self, p: &Position) {
        let len = self.xy.len();
        self.z.push(len, p.z);
        self.m.push(len, p.m);
        self.xy.push([p.x, p.y]);
    }

    fn ring(&mut self, ps: &[Position]) {
        ps.iter().for_each(|p| self.position(p));
        self.coords.push(self.xy.len() as u32);
    }

    fn part<'a>(&mut self, rings: impl IntoIterator<Item = &'a Vec<Position>>) {
        rings.into_iter().for_each(|r| self.ring(r));
        self.rings.push((self.coords.len() - 1) as u32);
    }

    fn push(&mut self, geometry: Option<Geometry>) {
        let tag = match &geometry {
            None => Tag::None,
            Some(Geometry::Point(p)) => {
                self.ring(std::slice::from_ref(p));
                self.rings.push((self.coords.len() - 1) as u32);
                Tag::Point
            }
            Some(Geometry::LineString(ps)) => {
                self.ring(ps);
                self.rings.push((self.coords.len() - 1) as u32);
                Tag::LineString
            }
            Some(Geometry::MultiPoint(ps)) => {
                self.ring(ps);
                self.rings.push((self.coords.len() - 1) as u32);
                Tag::MultiPoint
            }
            Some(Geometry::Polygon(rings)) => {
                self.part(rings);
                Tag::Polygon
            }
            Some(Geometry::MultiLineString(lines)) => {
                self.part(lines);
                Tag::MultiLineString
            }
            Some(Geometry::MultiPolygon(polys)) => {
                polys.iter().for_each(|rings| self.part(rings));
                Tag::MultiPolygon
            }
            Some(Geometry::GeometryCollection(_)) => Tag::Collection,
        };
        if let Some(g @ Geometry::GeometryCollection(_)) = geometry {
            self.collections.push((self.tags.len() as u32, g));
        }
        self.tags.push(tag);
        self.parts.push((self.rings.len() - 1) as u32);
    }

    fn get(&self, row: usize) -> Option<GeometryRef<'_>> {
        if let Some(edited) = self.edited.get(&(row as u32)) {
            return edited.as_ref().map(GeometryRef::owned);
        }
        match self.tags[row] {
            Tag::None => None,
            Tag::Collection => Some(GeometryRef::owned(self.collection(row))),
            _ => Some(GeometryRef(Repr::Flat { flat: self, row: row as u32 })),
        }
    }

    fn collection(&self, row: usize) -> &Geometry {
        let i = self.collections.binary_search_by_key(&(row as u32), |(r, _)| *r).expect("a collection row is held");
        &self.collections[i].1
    }

    fn pos(&self, i: usize) -> Position {
        let [x, y] = self.xy[i];
        Position { x, y, z: self.z.get(i), m: self.m.get(i) }
    }

    /// Row `row`'s parts, as ranges of rings.
    fn parts_of(&self, row: usize) -> std::ops::Range<usize> {
        self.parts[row] as usize..self.parts[row + 1] as usize
    }

    fn rings_of(&self, part: usize) -> std::ops::Range<usize> {
        self.rings[part] as usize..self.rings[part + 1] as usize
    }

    fn coords_of(&self, ring: usize) -> std::ops::Range<usize> {
        self.coords[ring] as usize..self.coords[ring + 1] as usize
    }

    /// Row `row`'s positions, every ring of every part.
    fn row_coords(&self, row: usize) -> std::ops::Range<usize> {
        let parts = self.parts_of(row);
        let first = self.rings[parts.start] as usize;
        let last = self.rings[parts.end] as usize;
        self.coords[first] as usize..self.coords[last] as usize
    }

    fn run(&self, ring: usize) -> Vec<Position> {
        self.coords_of(ring).map(|i| self.pos(i)).collect()
    }

    fn rings(&self, part: usize) -> Vec<Vec<Position>> {
        self.rings_of(part).map(|k| self.run(k)).collect()
    }

    fn to_geometry(&self, row: usize) -> Geometry {
        let first = self.parts[row] as usize;
        let first_ring = self.rings[first] as usize;
        match self.tags[row] {
            Tag::Point => Geometry::Point(self.pos(self.coords[first_ring] as usize)),
            Tag::LineString => Geometry::LineString(self.run(first_ring)),
            Tag::MultiPoint => Geometry::MultiPoint(self.run(first_ring)),
            Tag::Polygon => Geometry::Polygon(self.rings(first)),
            Tag::MultiLineString => Geometry::MultiLineString(self.rings(first)),
            Tag::MultiPolygon => Geometry::MultiPolygon(self.parts_of(row).map(|p| self.rings(p)).collect()),
            Tag::Collection => self.collection(row).clone(),
            Tag::None => unreachable!("a row without geometry has no view"),
        }
    }

    fn into_owned(self) -> Vec<Option<Geometry>> {
        let mut collections = self.collections.iter().peekable();
        (0..self.len())
            .map(|row| {
                if let Some(edited) = self.edited.get(&(row as u32)) {
                    return edited.clone();
                }
                match self.tags[row] {
                    Tag::None => None,
                    Tag::Collection => {
                        let (_, g) = collections.find(|(r, _)| *r as usize == row).expect("a collection row is held");
                        Some(g.clone())
                    }
                    _ => Some(self.to_geometry(row)),
                }
            })
            .collect()
    }

    /// Write `p` back as position `i`.
    fn put(&mut self, i: usize, p: Position) {
        self.xy[i] = [p.x, p.y];
        let len = self.xy.len();
        self.z.set(i, len, p.z);
        self.m.set(i, len, p.m);
    }

    /// The rows a buffer holds positions for: not an edited row, whose
    /// buffers are stale, and not a collection.
    fn held_in_buffers(&self, row: usize) -> bool {
        !matches!(self.tags[row], Tag::None | Tag::Collection) && !self.edited.contains_key(&(row as u32))
    }

    fn for_each_position_mut(&mut self, f: &mut dyn FnMut(&mut Position)) {
        let mut collections = std::mem::take(&mut self.collections);
        let mut edited = std::mem::take(&mut self.edited);
        let mut held = collections.iter_mut().peekable();
        for row in 0..self.len() {
            if let Some(g) = edited.get_mut(&(row as u32)) {
                g.iter_mut().for_each(|g| g.for_each_position_mut(&mut *f));
            } else if self.tags[row] == Tag::Collection {
                let (_, g) = held.find(|(r, _)| *r as usize == row).expect("a collection row is held");
                g.for_each_position_mut(&mut *f);
            } else if self.tags[row] != Tag::None {
                for i in self.row_coords(row) {
                    let mut p = self.pos(i);
                    f(&mut p);
                    self.put(i, p);
                }
            }
        }
        self.collections = collections;
        self.edited = edited;
    }

    fn for_each_position_run_mut(&mut self, f: &mut dyn FnMut(&mut [Position])) {
        let mut collections = std::mem::take(&mut self.collections);
        let mut edited = std::mem::take(&mut self.edited);
        let mut held = collections.iter_mut().peekable();
        let mut run = Vec::new();
        for row in 0..self.len() {
            if let Some(g) = edited.get_mut(&(row as u32)) {
                g.iter_mut().for_each(|g| g.for_each_position_run_mut(&mut *f));
            } else if self.tags[row] == Tag::Collection {
                let (_, g) = held.find(|(r, _)| *r as usize == row).expect("a collection row is held");
                g.for_each_position_run_mut(&mut *f);
            } else if self.held_in_buffers(row) {
                let first = self.rings[self.parts[row] as usize] as usize;
                let last = self.rings[self.parts[row + 1] as usize] as usize;
                for ring in first..last {
                    let coords = self.coords_of(ring);
                    run.clear();
                    run.extend(coords.clone().map(|i| self.pos(i)));
                    f(&mut run);
                    for (i, p) in coords.zip(&run) {
                        self.put(i, *p);
                    }
                }
            }
        }
        self.collections = collections;
        self.edited = edited;
    }
}

/// A feature's geometry, read in place: what
/// [`crate::FeatureRef::geometry`] gives. A block may hold it as a
/// [`Geometry`] or flat (`plans/columnar.org` D-C6), so this is a view of
/// either; [`Self::to_geometry`] makes an owned copy.
#[derive(Clone, Copy)]
pub struct GeometryRef<'a>(Repr<'a>);

#[derive(Clone, Copy)]
enum Repr<'a> {
    Owned(&'a Geometry),
    Flat { flat: &'a Flat, row: u32 },
}

impl<'a> GeometryRef<'a> {
    pub(crate) fn owned(geometry: &'a Geometry) -> GeometryRef<'a> {
        GeometryRef(Repr::Owned(geometry))
    }

    /// The geometry as an owned [`Geometry`], copied.
    pub fn to_geometry(self) -> Geometry {
        match self.0 {
            Repr::Owned(g) => g.clone(),
            Repr::Flat { flat, row } => flat.to_geometry(row as usize),
        }
    }

    /// The geometry, borrowed where the block holds one, copied where flat.
    pub(crate) fn to_cow(self) -> std::borrow::Cow<'a, Geometry> {
        match self.0 {
            Repr::Owned(g) => std::borrow::Cow::Borrowed(g),
            Repr::Flat { .. } => std::borrow::Cow::Owned(self.to_geometry()),
        }
    }

    /// The geometry itself, when the block holds it as one: no copy.
    pub(crate) fn as_geometry(self) -> Option<&'a Geometry> {
        match self.0 {
            Repr::Owned(g) => Some(g),
            Repr::Flat { .. } => None,
        }
    }

    /// The GeoParquet/GeoJSON type name, as [`Geometry::type_name`].
    pub fn type_name(self) -> &'static str {
        match self.0 {
            Repr::Owned(g) => g.type_name(),
            Repr::Flat { flat, row } => match flat.tags[row as usize] {
                Tag::Point => "Point",
                Tag::LineString => "LineString",
                Tag::Polygon => "Polygon",
                Tag::MultiPoint => "MultiPoint",
                Tag::MultiLineString => "MultiLineString",
                Tag::MultiPolygon => "MultiPolygon",
                Tag::Collection | Tag::None => unreachable!("held whole, or no view"),
            },
        }
    }

    /// This geometry's own bounding box, as [`Geometry::bbox`].
    pub fn bbox(self) -> Bbox {
        let mut b = Bbox::empty();
        self.extend_bbox(&mut b);
        b
    }

    /// Fold every position into `bbox`, as [`Geometry::extend_bbox`].
    pub fn extend_bbox(self, bbox: &mut Bbox) {
        match self.0 {
            Repr::Owned(g) => g.extend_bbox(bbox),
            Repr::Flat { flat, row } => flat.row_coords(row as usize).for_each(|i| bbox.add(flat.pos(i))),
        }
    }

    /// Whether any position carries an M, as [`Geometry::has_m`].
    pub fn has_m(self) -> bool {
        match self.0 {
            Repr::Owned(g) => g.has_m(),
            Repr::Flat { flat, row } => flat.row_coords(row as usize).any(|i| flat.m.get(i).is_some()),
        }
    }
}

impl<'a> GeometryRef<'a> {
    /// The geometry's structure, read in place from either form: what an
    /// encoder walks, so a flat block is written without a copy (C6 step 4).
    pub(crate) fn shape(self) -> Shape<'a> {
        match self.0 {
            Repr::Owned(g) => match g {
                Geometry::Point(p) => Shape::Point(*p),
                Geometry::LineString(ps) => Shape::LineString(Run::Slice(ps)),
                Geometry::MultiPoint(ps) => Shape::MultiPoint(Run::Slice(ps)),
                Geometry::Polygon(r) => Shape::Polygon(Rings::Slice(r)),
                Geometry::MultiLineString(r) => Shape::MultiLineString(Rings::Slice(r)),
                Geometry::MultiPolygon(p) => Shape::MultiPolygon(Parts::Slice(p)),
                Geometry::GeometryCollection(g) => Shape::Collection(g),
            },
            Repr::Flat { flat, row } => {
                let row = row as usize;
                let first = flat.parts[row] as usize;
                let ring = || Run::Flat(flat, flat.coords[flat.rings[first] as usize], flat.coords[flat.rings[first] as usize + 1]);
                let rings = || Rings::Flat(flat, flat.rings[first], flat.rings[first + 1]);
                match flat.tags[row] {
                    Tag::Point => Shape::Point(flat.pos(flat.coords[flat.rings[first] as usize] as usize)),
                    Tag::LineString => Shape::LineString(ring()),
                    Tag::MultiPoint => Shape::MultiPoint(ring()),
                    Tag::Polygon => Shape::Polygon(rings()),
                    Tag::MultiLineString => Shape::MultiLineString(rings()),
                    Tag::MultiPolygon => Shape::MultiPolygon(Parts::Flat(flat, flat.parts[row], flat.parts[row + 1])),
                    Tag::Collection | Tag::None => unreachable!("held whole, or no view"),
                }
            }
        }
    }
}

impl GeometryRef<'_> {
    /// The geometry's first position -- a collection's first member's --
    /// or `None` when it has none: what a writer reads a geometry's
    /// declared dimensionality off.
    pub(crate) fn first_position(self) -> Option<Position> {
        match self.shape() {
            Shape::Point(p) => Some(p),
            Shape::LineString(ps) | Shape::MultiPoint(ps) => ps.first(),
            Shape::Polygon(r) | Shape::MultiLineString(r) => r.first().and_then(Run::first),
            Shape::MultiPolygon(p) => p.first().and_then(Rings::first).and_then(Run::first),
            Shape::Collection(g) => g.first().and_then(|g| GeometryRef::owned(g).first_position()),
        }
    }
}

/// A geometry's structure, borrowed from whichever form holds it
/// ([`GeometryRef::shape`]).
#[derive(Clone, Copy)]
pub(crate) enum Shape<'a> {
    Point(Position),
    LineString(Run<'a>),
    Polygon(Rings<'a>),
    MultiPoint(Run<'a>),
    MultiLineString(Rings<'a>),
    MultiPolygon(Parts<'a>),
    Collection(&'a [Geometry]),
}

/// A run of positions: a line, a ring, a multipoint's points.
#[derive(Clone, Copy)]
pub(crate) enum Run<'a> {
    Slice(&'a [Position]),
    /// Positions `start..end` of a flat column.
    Flat(&'a Flat, u32, u32),
}

impl<'a> Run<'a> {
    pub(crate) fn len(self) -> usize {
        match self {
            Run::Slice(ps) => ps.len(),
            Run::Flat(_, start, end) => (end - start) as usize,
        }
    }

    pub(crate) fn first(self) -> Option<Position> {
        self.iter().next()
    }

    pub(crate) fn iter(self) -> impl Iterator<Item = Position> + 'a {
        let (slice, flat) = match self {
            Run::Slice(ps) => (ps, None),
            Run::Flat(flat, start, end) => (&[][..], Some((flat, start as usize..end as usize))),
        };
        slice.iter().copied().chain(flat.into_iter().flat_map(|(flat, range)| range.map(|i| flat.pos(i))))
    }
}

/// A list of runs: a polygon's rings, a multilinestring's lines.
#[derive(Clone, Copy)]
pub(crate) enum Rings<'a> {
    Slice(&'a [Vec<Position>]),
    /// Rings `start..end` of a flat column.
    Flat(&'a Flat, u32, u32),
}

impl<'a> Rings<'a> {
    pub(crate) fn len(self) -> usize {
        match self {
            Rings::Slice(r) => r.len(),
            Rings::Flat(_, start, end) => (end - start) as usize,
        }
    }

    pub(crate) fn first(self) -> Option<Run<'a>> {
        self.iter().next()
    }

    pub(crate) fn iter(self) -> impl Iterator<Item = Run<'a>> + 'a {
        let (slice, flat) = match self {
            Rings::Slice(r) => (r, None),
            Rings::Flat(flat, start, end) => (&[][..], Some((flat, start as usize..end as usize))),
        };
        slice.iter().map(|r| Run::Slice(r)).chain(
            flat.into_iter().flat_map(|(flat, range)| range.map(|k| Run::Flat(flat, flat.coords[k], flat.coords[k + 1]))),
        )
    }
}

/// A multipolygon's polygons, each a list of rings.
#[derive(Clone, Copy)]
pub(crate) enum Parts<'a> {
    Slice(&'a [Vec<Vec<Position>>]),
    /// Parts `start..end` of a flat column.
    Flat(&'a Flat, u32, u32),
}

impl<'a> Parts<'a> {
    pub(crate) fn len(self) -> usize {
        match self {
            Parts::Slice(p) => p.len(),
            Parts::Flat(_, start, end) => (end - start) as usize,
        }
    }

    pub(crate) fn first(self) -> Option<Rings<'a>> {
        self.iter().next()
    }

    pub(crate) fn iter(self) -> impl Iterator<Item = Rings<'a>> + 'a {
        let (slice, flat) = match self {
            Parts::Slice(p) => (p, None),
            Parts::Flat(flat, start, end) => (&[][..], Some((flat, start as usize..end as usize))),
        };
        slice.iter().map(|p| Rings::Slice(p)).chain(
            flat.into_iter().flat_map(|(flat, range)| range.map(|p| Rings::Flat(flat, flat.rings[p], flat.rings[p + 1]))),
        )
    }
}

/// Two views are equal when the geometries they show are.
impl PartialEq for GeometryRef<'_> {
    fn eq(&self, other: &GeometryRef<'_>) -> bool {
        match (self.as_geometry(), other.as_geometry()) {
            (Some(a), Some(b)) => a == b,
            _ => self.to_geometry() == other.to_geometry(),
        }
    }
}

impl PartialEq<Geometry> for GeometryRef<'_> {
    fn eq(&self, other: &Geometry) -> bool {
        match self.as_geometry() {
            Some(g) => g == other,
            None => self.to_geometry() == *other,
        }
    }
}

impl std::fmt::Debug for GeometryRef<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.as_geometry() {
            Some(g) => g.fmt(f),
            None => self.to_geometry().fmt(f),
        }
    }
}

/// Whether `a` and `b` are the same geometry bit for bit: a coordinate by its
/// bits, so `-0.0` is not `0.0` and a NaN is itself, where `==` says
/// otherwise. What an edit compares to tell whether it changed anything.
pub(crate) fn identical(a: &Option<Geometry>, b: &Option<Geometry>) -> bool {
    fn pos(a: &Position, b: &Position) -> bool {
        let bits = |v: Option<f64>| v.map(f64::to_bits);
        a.x.to_bits() == b.x.to_bits() && a.y.to_bits() == b.y.to_bits() && bits(a.z) == bits(b.z) && bits(a.m) == bits(b.m)
    }
    fn run(a: &[Position], b: &[Position]) -> bool {
        a.len() == b.len() && a.iter().zip(b).all(|(a, b)| pos(a, b))
    }
    fn rings(a: &[Vec<Position>], b: &[Vec<Position>]) -> bool {
        a.len() == b.len() && a.iter().zip(b).all(|(a, b)| run(a, b))
    }
    fn geometry(a: &Geometry, b: &Geometry) -> bool {
        match (a, b) {
            (Geometry::Point(a), Geometry::Point(b)) => pos(a, b),
            (Geometry::LineString(a), Geometry::LineString(b)) | (Geometry::MultiPoint(a), Geometry::MultiPoint(b)) => {
                run(a, b)
            }
            (Geometry::Polygon(a), Geometry::Polygon(b))
            | (Geometry::MultiLineString(a), Geometry::MultiLineString(b)) => rings(a, b),
            (Geometry::MultiPolygon(a), Geometry::MultiPolygon(b)) => {
                a.len() == b.len() && a.iter().zip(b).all(|(a, b)| rings(a, b))
            }
            (Geometry::GeometryCollection(a), Geometry::GeometryCollection(b)) => {
                a.len() == b.len() && a.iter().zip(b).all(|(a, b)| geometry(a, b))
            }
            _ => false,
        }
    }
    match (a, b) {
        (None, None) => true,
        (Some(a), Some(b)) => geometry(a, b),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::table::arbitrary;

    fn column(rng: &mut arbitrary::Rng, rows: usize) -> Vec<Option<Geometry>> {
        (0..rows).map(|_| (!rng.one_in(6)).then(|| arbitrary::geometry(rng, true, 0))).collect()
    }

    fn flat(rows: &[Option<Geometry>]) -> GeometryColumn {
        GeometryColumn::Owned(rows.to_vec()).seal(Layout::Auto)
    }

    fn reads(column: &GeometryColumn) -> Vec<Option<Geometry>> {
        (0..column.len()).map(|row| column.get(row).map(GeometryRef::to_geometry)).collect()
    }

    fn same(a: &[Option<Geometry>], b: &[Option<Geometry>]) -> bool {
        a.len() == b.len() && a.iter().zip(b).all(|(a, b)| identical(a, b))
    }

    /// C6 step 3's property test: a flat column rebuilds every geometry bit
    /// for bit -- every type, empty parts and rings, Z and M per position,
    /// collections, mixed types, -0.0 and NaN -- and its views say what the
    /// geometry they show says.
    #[test]
    fn a_flat_column_rebuilds_every_geometry_exactly() {
        for seed in 0..500 {
            let mut rng = arbitrary::Rng::new(seed);
            let n = rng.below(40);
            let rows = column(&mut rng, n);
            let column = flat(&rows);
            assert!(matches!(column, GeometryColumn::Flat(_)));
            assert!(same(&reads(&column), &rows), "seed {seed}");
            assert!(same(&column.clone().into_owned(), &rows), "seed {seed}");
            for (row, g) in rows.iter().enumerate() {
                let (Some(view), Some(g)) = (column.get(row), g) else { continue };
                assert_eq!(view.type_name(), g.type_name(), "seed {seed}, row {row}");
                assert_eq!(view.has_m(), g.has_m(), "seed {seed}, row {row}");
                let (a, b) = (view.bbox(), g.bbox());
                let bits = |b: Bbox| [b.min_x, b.min_y, b.max_x, b.max_y].map(f64::to_bits);
                assert_eq!(bits(a), bits(b), "seed {seed}, row {row}");
            }
        }
    }

    /// The position editors visit a flat column's positions as they visit
    /// the owned geometries': the same positions in the same order, the same
    /// runs, and what an edit leaves -- a Z or M given or taken away from one
    /// position among its neighbours -- is kept.
    #[test]
    fn position_edits_on_a_flat_column_are_the_owned_ones() {
        for seed in 0..500 {
            let mut rng = arbitrary::Rng::new(seed);
            let n = rng.below(40);
            let mut rows = column(&mut rng, n);
            let mut column = flat(&rows);
            // A row replaced through the overlay is edited where it is read.
            if !rows.is_empty() {
                let row = rng.below(rows.len());
                let replacement = Some(arbitrary::geometry(&mut rng, true, 0));
                column.set(row, replacement.clone());
                rows[row] = replacement;
            }
            let edit = |p: &mut Position, n: usize| {
                p.x += 1.0;
                match n % 5 {
                    0 => p.z = None,
                    1 => p.z = Some(n as f64),
                    2 => p.m = p.m.xor(Some(-0.0)),
                    _ => {}
                }
            };
            let (mut seen_owned, mut seen_flat) = (0, 0);
            let mut visited = Vec::new();
            for g in rows.iter_mut().flatten() {
                g.for_each_position_mut(|p| {
                    visited.push(*p);
                    edit(p, seen_owned);
                    seen_owned += 1;
                });
            }
            let mut i = 0;
            column.for_each_position_mut(&mut |p| {
                assert!(identical(&Some(Geometry::Point(*p)), &Some(Geometry::Point(visited[i]))), "seed {seed}, position {i}");
                i += 1;
                edit(p, seen_flat);
                seen_flat += 1;
            });
            assert_eq!(seen_owned, seen_flat, "seed {seed}");
            assert!(same(&reads(&column), &rows), "seed {seed}: position edits");

            let mut owned_runs = Vec::new();
            for g in rows.iter_mut().flatten() {
                g.for_each_position_run_mut(|run| {
                    owned_runs.push(run.len());
                    run.iter_mut().for_each(|p| p.y *= 2.0);
                    if let Some(p) = run.first_mut() {
                        p.m = Some(1.0);
                    }
                });
            }
            let mut flat_runs = Vec::new();
            column.for_each_position_run_mut(&mut |run| {
                flat_runs.push(run.len());
                run.iter_mut().for_each(|p| p.y *= 2.0);
                if let Some(p) = run.first_mut() {
                    p.m = Some(1.0);
                }
            });
            assert_eq!(owned_runs, flat_runs, "seed {seed}");
            assert!(same(&reads(&column), &rows), "seed {seed}: run edits");
        }
    }

    /// An edit through a flat column's overlay: kept when it changes the
    /// geometry, even by a sign bit, and not overlaid when it changes
    /// nothing.
    #[test]
    fn an_edit_that_changes_nothing_leaves_no_overlay() {
        let rows = vec![Some(Geometry::Point(Position::new(0.0, 1.0))), None];
        let mut column = flat(&rows);
        column.edit(0, |_| {});
        column.edit(1, |_| {});
        let GeometryColumn::Flat(f) = &column else { unreachable!() };
        assert!(f.edited.is_empty());
        column.edit(0, |g| *g = Some(Geometry::Point(Position::new(-0.0, 1.0))));
        let GeometryColumn::Flat(f) = &column else { unreachable!() };
        assert_eq!(f.edited.len(), 1);
        let Some(Geometry::Point(p)) = column.get(0).map(GeometryRef::to_geometry) else { unreachable!() };
        assert!(p.x.is_sign_negative());
        // A pass over every row makes the column owned and flat again,
        // folding the overlay.
        let was_flat = column.make_owned();
        column.reseal(was_flat);
        let GeometryColumn::Flat(f) = &column else { unreachable!() };
        assert!(f.edited.is_empty());
        assert!(identical(&column.get(0).map(GeometryRef::to_geometry), &Some(Geometry::Point(Position::new(-0.0, 1.0)))));
    }

    /// Every writer writes a flat block's geometry as it writes the owned
    /// one: WKB bit for bit from a view, and every format's whole output
    /// from a collection held each way.
    #[test]
    fn writers_write_flat_geometry_as_they_write_owned() {
        for seed in 0..300 {
            let mut rng = arbitrary::Rng::new(seed);
            let n = rng.below(30);
            let rows = column(&mut rng, n);
            let column = flat(&rows);
            for (row, g) in rows.iter().enumerate() {
                let (Some(view), Some(g)) = (column.get(row), g) else { continue };
                assert_eq!(crate::geometry::wkb_ref(view), crate::geometry::to_wkb(g), "seed {seed}, row {row}");
            }
            let features: Vec<crate::feature::Feature> = rows
                .iter()
                .map(|g| crate::feature::Feature { geometry: g.clone(), ..Default::default() })
                .collect();
            let held = |layout| {
                crate::feature::FeatureCollection::from_table(crate::table::Table::from_features(features.clone(), layout))
            };
            let (owned, flat) = (held(Layout::Rows), held(Layout::Columns));
            for format in crate::Format::ALL {
                if matches!(format, crate::Format::Gpkg | crate::Format::Shapefile) {
                    continue;
                }
                let write = |fc| crate::write_features(format, fc).map_err(|e| e.to_string());
                assert_eq!(write(&owned), write(&flat), "seed {seed}, {format:?}");
            }
            let shp = |fc| crate::shapefile::write(fc).map(|e| (e.shp, e.shx)).map_err(|e| e.to_string());
            assert_eq!(shp(&owned), shp(&flat), "seed {seed}, Shapefile");
        }
    }

    /// `--layout rows` keeps a block's geometry owned (D-C16).
    #[test]
    fn rows_keep_their_geometry_owned() {
        let sealed = GeometryColumn::Owned(vec![None]).seal(Layout::Rows);
        assert!(matches!(sealed, GeometryColumn::Owned(_)));
    }
}
