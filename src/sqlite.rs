//! A minimal SQLite file-format reader and writer, implemented from the format
//! spec rather than a crate (keeping the project dependency-free) — the basis
//! for reading and writing GeoPackage.
//!
//! The reader reads the 100-byte database header, walks table b-trees (both
//! interior and leaf pages, following overflow chains), and decodes the record
//! format (serial-type varints → typed values). The [`write_database`] writer
//! produces a complete database from a set of tables (rows are `rowid`-ordered):
//! it encodes records, packs cells into leaf pages, builds interior levels,
//! spills large payloads to overflow pages, and writes `sqlite_master` on page
//! one. Multi-byte integers in the header and b-tree structures are big-endian.

use crate::error::{Error, Result};

fn err<T>(msg: &str) -> Result<T> {
    Err(Error::Convert(format!("sqlite: {msg}")))
}

const HEADER_MAGIC: &[u8; 16] = b"SQLite format 3\0";

/// A decoded cell value (SQLite storage classes).
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Null,
    Int(i64),
    Real(f64),
    Text(String),
    Blob(Vec<u8>),
}

/// A table from `sqlite_master`, with its column names parsed from the DDL.
pub struct Table {
    pub name: String,
    pub columns: Vec<String>,
    /// Each column's declared type as written in the DDL (`TEXT`, `BOOLEAN`,
    /// `MEDIUMINT`…), parallel to [`Self::columns`], or `None` where the
    /// declaration omitted one — SQLite permits that, and it means the column
    /// has no type affinity rather than a default one.
    ///
    /// SQLite itself is dynamically typed, so a *value*'s storage class need
    /// not match this. That gap is exactly the point: a GeoPackage `BOOLEAN`
    /// stores 0/1 as `INTEGER`, so the declaration is the only place the
    /// booleanness is recorded.
    pub column_types: Vec<Option<String>>,
    root_page: u32,
    /// Index of the `INTEGER PRIMARY KEY` column (a rowid alias), if any: its
    /// stored value is NULL and the real value is the rowid.
    rowid_alias: Option<usize>,
    /// Each column's `DEFAULT` literal, parallel to [`Self::columns`].
    ///
    /// This is not decoration: `ALTER TABLE … ADD COLUMN` does not rewrite the
    /// rows that already exist, so their records stay *short* and the added
    /// column's value lives only in the DDL. Padding those rows with NULL
    /// instead reads back a value SQLite never stored and no other reader
    /// reports — see [`Database::read_rows`].
    defaults: Vec<Option<Value>>,
}

impl Table {
    /// The index of the `INTEGER PRIMARY KEY` (rowid-alias) column, if any.
    pub fn rowid_alias(&self) -> Option<usize> {
        self.rowid_alias
    }
}

/// One `sqlite_master` entry, reduced to what a survey of a file's contents
/// needs: what kind of object it is, what it is attached to, and the text that
/// declared it. Unlike [`Table`] the DDL here is *undecoded*, because this
/// describes entries the column parser has no opinion about (triggers, views,
/// virtual tables).
pub struct MasterObject {
    /// `sqlite_master.type` — `"table"`, `"index"`, `"view"` or `"trigger"`.
    pub kind: String,
    pub name: String,
    /// The table this entry is attached to (its own name for a table).
    pub tbl_name: String,
    /// The b-tree root page, or 0 for a schema-only entry.
    pub root_page: u32,
    /// `sqlite_master.sql` — the statement that created this object, stored by
    /// SQLite exactly as it was written, whitespace and all. `None` for the
    /// entries SQLite creates on its own behalf, which is every
    /// `sqlite_autoindex_*`.
    ///
    /// Undecoded on purpose: a caller comparing what a file *declares* against
    /// what some other authority *specifies* needs the text, and parsing it
    /// first would silently normalise away the difference it is looking for.
    ///
    /// Read today only by
    /// `geopackage::writer::tests::the_registry_ddl_is_what_the_standard_says`,
    /// which is why it is allowed to be otherwise unread. M3b is the consumer
    /// in prose: carrying a view or a trigger verbatim means carrying exactly
    /// this string, since there is nothing else to reconstruct one from.
    #[allow(dead_code)]
    pub sql: Option<String>,
}

pub struct Database<'a> {
    data: &'a [u8],
    page_size: usize,
    usable_size: usize,
}

impl<'a> Database<'a> {
    pub fn open(data: &'a [u8]) -> Result<Database<'a>> {
        if data.len() < 100 || &data[0..16] != HEADER_MAGIC {
            return err("not a sqlite database (bad magic)");
        }
        let page_size = match u16::from_be_bytes([data[16], data[17]]) {
            1 => 65536,
            n => n as usize,
        };
        if page_size < 512 || data.len() < page_size {
            return err("invalid page size");
        }
        let reserved = data[20] as usize;
        let encoding = u32::from_be_bytes([data[56], data[57], data[58], data[59]]);
        if encoding != 1 {
            return err("only UTF-8 databases are supported");
        }
        Ok(Database {
            data,
            page_size,
            usable_size: page_size - reserved,
        })
    }

    /// The page-1-based byte slice for page `n` (1-indexed).
    fn page(&self, n: u32) -> Result<&'a [u8]> {
        let n = n as usize;
        if n == 0 {
            return err("page 0 is invalid");
        }
        let start = (n - 1) * self.page_size;
        self.data
            .get(start..start + self.page_size)
            .ok_or_else(|| Error::Convert("sqlite: page out of range".into()))
    }

    /// A descriptor for `sqlite_master` itself: a table b-tree rooted at page
    /// 1 whose columns are (type, name, tbl_name, rootpage, sql).
    fn master_descriptor() -> Table {
        Table {
            name: "sqlite_master".into(),
            columns: vec!["type".into(), "name".into(), "tbl_name".into(), "rootpage".into(), "sql".into()],
            // Never consulted: this synthetic descriptor exists only to read
            // `sqlite_master`'s own rows, whose types are known statically.
            column_types: Vec::new(),
            root_page: 1,
            rowid_alias: None,
            defaults: Vec::new(),
        }
    }

    /// Every `sqlite_master` entry, whatever its type — tables, indexes,
    /// views, triggers, virtual tables.
    ///
    /// [`Self::tables`] is the *readable* subset of this: what the decoders
    /// can turn into rows. This one answers the different question of what a
    /// file actually *contains*, which is what a caller reporting on content
    /// it is about to destroy needs — see
    /// `geopackage::container_drop_warnings`.
    pub fn master_objects(&self) -> Result<Vec<MasterObject>> {
        let mut objects = Vec::new();
        for row in self.read_rows(&Self::master_descriptor())? {
            let (Some(kind), Some(name)) = (as_text(&row, 0), as_text(&row, 1)) else {
                continue;
            };
            objects.push(MasterObject {
                kind,
                tbl_name: as_text(&row, 2).unwrap_or_else(|| name.clone()),
                name,
                // Schema-only entries (triggers, views, virtual tables) carry
                // rootpage 0; a negative value is not something SQLite writes.
                root_page: as_int(&row, 3).unwrap_or(0).max(0) as u32,
                sql: as_text(&row, 4),
            });
        }
        Ok(objects)
    }

    /// The AUTOINCREMENT high-water marks this file records, as
    /// `(table_name, mark)`.
    ///
    /// Empty when the file has no `sqlite_sequence` table, and *also* empty
    /// when it has one that no table has been inserted into yet — those are
    /// different states of the file and the same answer here, because a mark
    /// is all a caller can carry. Whether the table itself must exist is
    /// decided by the DDL, not by this.
    ///
    /// **Not derivable from the rows.** The mark is a high-water mark: delete
    /// the last row and it stays behind, delete every row and it still stands.
    /// That is the whole reason `container.rs`'s "these follow the DDL rather
    /// than carrying anything" is false of `sqlite_sequence`, and why dropping
    /// it produces a database that passes `integrity_check` and rejects every
    /// `INSERT`. See [`Autoincrement`].
    ///
    /// Read today only by this module's tests, and allowed to be otherwise
    /// unread on the precedent of `convert::convert`. **The consumer is M3b**:
    /// the write side landed first deliberately, because the gap is forced by
    /// the very first table M3b carries — `sqlite_contents.gpkg`'s `imagery`
    /// tile table and `gpkg_metadata` both declare AUTOINCREMENT — and because
    /// it is invisible to the verification ritual, so discovering it later
    /// would mean discovering it from a user.
    #[allow(dead_code)]
    pub fn sequence_numbers(&self) -> Result<Vec<(String, i64)>> {
        let tables = self.tables()?;
        let Some(seq) = tables.iter().find(|t| t.name == SEQUENCE_TABLE) else {
            return Ok(Vec::new());
        };
        let name_col = seq.columns.iter().position(|c| c == "name");
        let seq_col = seq.columns.iter().position(|c| c == "seq");
        let (Some(name_col), Some(seq_col)) = (name_col, seq_col) else {
            return Ok(Vec::new());
        };
        let mut out = Vec::new();
        for row in self.read_rows(seq)? {
            if let (Some(name), Some(n)) = (as_text(&row, name_col), as_int(&row, seq_col)) {
                out.push((name, n));
            }
        }
        Ok(out)
    }

    /// All ordinary tables (`type = 'table'`) with a real root page.
    pub fn tables(&self) -> Result<Vec<Table>> {
        let master = Self::master_descriptor();
        let mut tables = Vec::new();
        for row in self.read_rows(&master)? {
            let kind = as_text(&row, 0);
            let root = as_int(&row, 3);
            let sql = as_text(&row, 4);
            if kind.as_deref() == Some("table")
                && let (Some(name), Some(root), Some(sql)) = (as_text(&row, 1), root, sql)
                && root > 0
            {
                let ddl = parse_columns(&sql);
                tables.push(Table {
                    name,
                    columns: ddl.names,
                    column_types: ddl.types,
                    root_page: root as u32,
                    rowid_alias: ddl.rowid_alias,
                    defaults: ddl.defaults,
                });
            }
        }
        Ok(tables)
    }

    /// Every row of `table`, values aligned to `table.columns`.
    pub fn read_rows(&self, table: &Table) -> Result<Vec<Vec<Value>>> {
        let mut cells = Vec::new();
        self.walk_table(table.root_page, &mut cells)?;
        let ncols = table.columns.len();

        let mut rows = Vec::with_capacity(cells.len());
        for (rowid, payload) in cells {
            let mut values = decode_record(&payload)?;
            // Pad a short record — a row that predates an `ALTER TABLE … ADD
            // COLUMN` — with each missing column's declared default, which is
            // where SQLite keeps the value for exactly these rows. Columns with
            // no literal default fall back to NULL, as SQLite does.
            for i in values.len()..ncols {
                values.push(
                    table
                        .defaults
                        .get(i)
                        .cloned()
                        .flatten()
                        .unwrap_or(Value::Null),
                );
            }
            values.truncate(ncols);
            if let Some(i) = table.rowid_alias
                && matches!(values.get(i), Some(Value::Null))
            {
                values[i] = Value::Int(rowid);
            }
            rows.push(values);
        }
        Ok(rows)
    }

    /// Depth-first walk of a table b-tree, collecting `(rowid, payload)`.
    fn walk_table(&self, page_no: u32, out: &mut Vec<(i64, Vec<u8>)>) -> Result<()> {
        let page = self.page(page_no)?;
        let base = if page_no == 1 { 100 } else { 0 };
        let page_type = page[base];
        let num_cells = u16::from_be_bytes([page[base + 3], page[base + 4]]) as usize;

        match page_type {
            0x0d => {
                // Leaf table page: 8-byte header, then cell pointers.
                let ptr_array = base + 8;
                for i in 0..num_cells {
                    let off = u16::from_be_bytes([page[ptr_array + i * 2], page[ptr_array + i * 2 + 1]])
                        as usize;
                    let (rowid, payload) = self.leaf_cell(page, off)?;
                    out.push((rowid, payload));
                }
            }
            0x05 => {
                // Interior table page: 12-byte header (right pointer at base+8).
                let ptr_array = base + 12;
                for i in 0..num_cells {
                    let off = u16::from_be_bytes([page[ptr_array + i * 2], page[ptr_array + i * 2 + 1]])
                        as usize;
                    let child = u32::from_be_bytes([page[off], page[off + 1], page[off + 2], page[off + 3]]);
                    self.walk_table(child, out)?;
                }
                let right = u32::from_be_bytes([page[base + 8], page[base + 9], page[base + 10], page[base + 11]]);
                self.walk_table(right, out)?;
            }
            other => return err(&format!("unexpected b-tree page type {other:#x}")),
        }
        Ok(())
    }

    /// Every record of an index b-tree rooted at `page_no`, **in index order**.
    ///
    /// This is the read half of decision D10 in
    /// `plans/same-format-fidelity.org`: an index on disk is *already sorted*,
    /// so reading it in order yields records that can be re-packed into a fresh
    /// file without ever comparing two keys. Nothing here knows SQLite's
    /// collation or type-ordering rules, and nothing here needs to.
    ///
    /// An index record is a plain SQLite record whose last column is the rowid
    /// of the table row it points at; this returns it undecoded, because a
    /// carried index is carried, not understood.
    pub fn read_index(&self, root_page: u32) -> Result<Vec<Vec<u8>>> {
        let mut out = Vec::new();
        self.walk_index(root_page, &mut out)?;
        Ok(out)
    }

    /// In-order walk of an index b-tree: `0x0a` leaf, `0x02` interior.
    ///
    /// Unlike a table b-tree — where an interior cell carries only a rowid and
    /// every payload lives in a leaf — an index interior cell carries a *real
    /// record*, which sorts between its left subtree and the next cell. So the
    /// walk has to emit it, in position, or the sequence comes back missing one
    /// record per interior cell.
    fn walk_index(&self, page_no: u32, out: &mut Vec<Vec<u8>>) -> Result<()> {
        let page = self.page(page_no)?;
        // An index b-tree never roots on page 1: page 1 is `sqlite_master`,
        // which is a table b-tree. So there is no 100-byte header to skip here.
        let page_type = page[0];
        let num_cells = u16::from_be_bytes([page[3], page[4]]) as usize;

        match page_type {
            0x0a => {
                let ptr_array = 8;
                for i in 0..num_cells {
                    let off = u16::from_be_bytes([page[ptr_array + i * 2], page[ptr_array + i * 2 + 1]])
                        as usize;
                    out.push(self.index_cell(page, off, false)?.1);
                }
            }
            0x02 => {
                let ptr_array = 12;
                for i in 0..num_cells {
                    let off = u16::from_be_bytes([page[ptr_array + i * 2], page[ptr_array + i * 2 + 1]])
                        as usize;
                    let (child, payload) = self.index_cell(page, off, true)?;
                    self.walk_index(child, out)?;
                    out.push(payload);
                }
                let right = u32::from_be_bytes([page[8], page[9], page[10], page[11]]);
                self.walk_index(right, out)?;
            }
            other => return err(&format!("unexpected index b-tree page type {other:#x}")),
        }
        Ok(())
    }

    /// Decode an index cell at `off`, assembling any overflow payload.
    /// Returns `(left_child, payload)`; `left_child` is 0 on a leaf.
    ///
    /// An interior cell is a 4-byte child pointer followed by exactly what a
    /// leaf cell is, which is why one function serves both.
    fn index_cell(&self, page: &[u8], off: usize, interior: bool) -> Result<(u32, Vec<u8>)> {
        let (child, off) = if interior {
            let c = u32::from_be_bytes([page[off], page[off + 1], page[off + 2], page[off + 3]]);
            (c, off + 4)
        } else {
            (0, off)
        };
        let (payload_len, n) = varint(page, off)?;
        let payload_len = payload_len as usize;
        let content = off + n;

        // Index cells keep a larger share on-page than table leaves do:
        // `((usable - 12) * 64 / 255) - 23` against `usable - 35`.
        let u = self.usable_size;
        let local = local_payload(payload_len, u, ((u - 12) * 64 / 255) - 23);

        let mut payload = page
            .get(content..content + local)
            .ok_or_else(|| Error::Convert("sqlite: index cell payload out of range".into()))?
            .to_vec();

        if payload_len > local {
            let mut next = u32::from_be_bytes([
                page[content + local],
                page[content + local + 1],
                page[content + local + 2],
                page[content + local + 3],
            ]);
            while next != 0 && payload.len() < payload_len {
                let ov = self.page(next)?;
                next = u32::from_be_bytes([ov[0], ov[1], ov[2], ov[3]]);
                let take = (payload_len - payload.len()).min(u - 4);
                payload.extend_from_slice(&ov[4..4 + take]);
            }
        }
        if payload.len() < payload_len {
            return err("truncated overflow payload");
        }
        Ok((child, payload))
    }

    /// Decode a leaf table cell at `off`, assembling any overflow payload.
    fn leaf_cell(&self, page: &[u8], off: usize) -> Result<(i64, Vec<u8>)> {
        let (payload_len, n1) = varint(page, off)?;
        let (rowid, n2) = varint(page, off + n1)?;
        let payload_len = payload_len as usize;
        let content = off + n1 + n2;

        // How much of the payload lives on this page. A table leaf keeps up to
        // `usable - 35`; an index cell uses a different ceiling, which is why
        // this is a parameter rather than a constant (see `local_payload`).
        let u = self.usable_size;
        let local = local_payload(payload_len, u, u - 35);

        let mut payload = page
            .get(content..content + local)
            .ok_or_else(|| Error::Convert("sqlite: cell payload out of range".into()))?
            .to_vec();

        if payload_len > local {
            let mut next = u32::from_be_bytes([
                page[content + local],
                page[content + local + 1],
                page[content + local + 2],
                page[content + local + 3],
            ]);
            while next != 0 && payload.len() < payload_len {
                let ov = self.page(next)?;
                next = u32::from_be_bytes([ov[0], ov[1], ov[2], ov[3]]);
                let take = (payload_len - payload.len()).min(u - 4);
                payload.extend_from_slice(&ov[4..4 + take]);
            }
        }
        if payload.len() < payload_len {
            return err("truncated overflow payload");
        }
        Ok((rowid, payload))
    }
}

// --- record decoding -------------------------------------------------------

/// Decode a record payload into its column values.
fn decode_record(payload: &[u8]) -> Result<Vec<Value>> {
    let (header_len, mut hp) = varint(payload, 0)?;
    let header_len = header_len as usize;
    let mut bp = header_len;
    let mut values = Vec::new();
    while hp < header_len {
        let (serial, adv) = varint(payload, hp)?;
        hp += adv;
        let (value, size) = decode_value(serial, &payload[bp.min(payload.len())..])?;
        bp += size;
        values.push(value);
    }
    Ok(values)
}

fn decode_value(serial: i64, data: &[u8]) -> Result<(Value, usize)> {
    let need = |n: usize| -> Result<&[u8]> {
        data.get(..n)
            .ok_or_else(|| Error::Convert("sqlite: truncated value".into()))
    };
    Ok(match serial {
        0 => (Value::Null, 0),
        1 => (Value::Int(signed_be(need(1)?)), 1),
        2 => (Value::Int(signed_be(need(2)?)), 2),
        3 => (Value::Int(signed_be(need(3)?)), 3),
        4 => (Value::Int(signed_be(need(4)?)), 4),
        5 => (Value::Int(signed_be(need(6)?)), 6),
        6 => (Value::Int(signed_be(need(8)?)), 8),
        7 => {
            let b = need(8)?;
            (Value::Real(f64::from_be_bytes(b.try_into().unwrap())), 8)
        }
        8 => (Value::Int(0), 0),
        9 => (Value::Int(1), 0),
        n if n >= 12 && n % 2 == 0 => {
            let len = (n as usize - 12) / 2;
            (Value::Blob(need(len)?.to_vec()), len)
        }
        n if n >= 13 => {
            let len = (n as usize - 13) / 2;
            let s = std::str::from_utf8(need(len)?)
                .map_err(|_| Error::Convert("sqlite: invalid utf-8 text".into()))?;
            (Value::Text(s.to_string()), len)
        }
        other => return err(&format!("reserved serial type {other}")),
    })
}

/// Sign-extend a big-endian signed integer of 1..8 bytes.
fn signed_be(bytes: &[u8]) -> i64 {
    let mut v: u64 = if bytes[0] & 0x80 != 0 { u64::MAX } else { 0 };
    for &b in bytes {
        v = (v << 8) | b as u64;
    }
    v as i64
}

/// A SQLite varint: big-endian, up to 9 bytes, high bit as continuation.
fn varint(data: &[u8], pos: usize) -> Result<(i64, usize)> {
    let mut result: u64 = 0;
    for i in 0..8 {
        let b = *data
            .get(pos + i)
            .ok_or_else(|| Error::Convert("sqlite: truncated varint".into()))?;
        result = (result << 7) | (b & 0x7f) as u64;
        if b & 0x80 == 0 {
            return Ok((result as i64, i + 1));
        }
    }
    let b = *data
        .get(pos + 8)
        .ok_or_else(|| Error::Convert("sqlite: truncated varint".into()))?;
    result = (result << 8) | b as u64;
    Ok((result as i64, 9))
}

// --- CREATE TABLE column parsing -------------------------------------------

/// One `CREATE TABLE` statement, decoded into what the row reader needs.
#[derive(Default)]
struct ColumnDdl {
    names: Vec<String>,
    types: Vec<Option<String>>,
    defaults: Vec<Option<Value>>,
    rowid_alias: Option<usize>,
}

/// Extract column names (in order), their declared types, their `DEFAULT`
/// literals and the rowid-alias column index from a `CREATE TABLE` statement.
fn parse_columns(sql: &str) -> ColumnDdl {
    let Some(open) = sql.find('(') else {
        return ColumnDdl::default();
    };
    let Some(close) = sql.rfind(')') else {
        return ColumnDdl::default();
    };
    let body = &sql[open + 1..close];

    let mut ddl = ColumnDdl::default();
    for part in split_top_level(body) {
        let part = part.trim();
        if part.is_empty() || is_table_constraint(part) {
            continue;
        }
        let Some(name) = first_identifier(part) else {
            continue;
        };
        let upper = part.to_ascii_uppercase();
        if upper.contains("INTEGER") && upper.contains("PRIMARY KEY") {
            ddl.rowid_alias = Some(ddl.names.len());
        }
        ddl.types.push(declared_type(part, &name));
        ddl.defaults.push(default_literal(part, &name));
        ddl.names.push(name);
    }
    ddl
}

/// The `DEFAULT` literal of one column definition, or `None` where there is
/// none this can evaluate.
///
/// Only *literals* are decoded, which is sufficient: the sole path by which a
/// default reaches an already-stored row is `ALTER TABLE … ADD COLUMN`, and
/// SQLite rejects a non-constant default there ("Cannot add a column with
/// non-constant default"). So an expression default — `DEFAULT (…)`,
/// `CURRENT_TIMESTAMP` — can never apply to a short record, and returning
/// `None` for it is correct rather than a shortfall.
fn default_literal(part: &str, name: &str) -> Option<Value> {
    // Step over the column name however it was quoted, exactly as
    // `declared_type` does: leaving the closing quote in place would open a
    // string that swallows the rest of the definition.
    let rest = part.get(part.find(name)? + name.len()..)?;
    let rest = rest.trim_start_matches(['"', '\'', '`', ']']);
    let bytes = rest.as_bytes();
    let mut i = 0;
    let mut depth = 0i32;
    let mut quote: Option<u8> = None;
    while i < bytes.len() {
        let c = bytes[i];
        if let Some(q) = quote {
            if c == q {
                quote = None;
            }
            i += 1;
            continue;
        }
        match c {
            b'"' | b'\'' | b'`' => quote = Some(c),
            b'[' => quote = Some(b']'),
            b'(' => depth += 1,
            b')' => depth -= 1,
            _ if depth == 0 && starts_word(bytes, i, b"DEFAULT") => {
                return parse_literal(rest[i + 7..].trim_start());
            }
            _ => {}
        }
        i += 1;
    }
    None
}

/// Whether `word` sits at `i` in `bytes` as a whole word, case-insensitively.
fn starts_word(bytes: &[u8], i: usize, word: &[u8]) -> bool {
    let is_word = |b: u8| b.is_ascii_alphanumeric() || b == b'_';
    bytes
        .get(i..i + word.len())
        .is_some_and(|s| s.eq_ignore_ascii_case(word))
        && (i == 0 || !is_word(bytes[i - 1]))
        && bytes.get(i + word.len()).is_none_or(|b| !is_word(*b))
}

/// One SQLite literal, as written in a `DEFAULT` clause. `NULL` returns `None`,
/// which is the same padding this produces anyway.
fn parse_literal(s: &str) -> Option<Value> {
    let bytes = s.as_bytes();
    if starts_word(bytes, 0, b"NULL") {
        return None;
    }
    if starts_word(bytes, 0, b"TRUE") {
        return Some(Value::Int(1));
    }
    if starts_word(bytes, 0, b"FALSE") {
        return Some(Value::Int(0));
    }
    if s.starts_with('\'') {
        return text_literal(s);
    }
    // `X'0102'` — a blob literal, in either case.
    if bytes.len() > 2 && (bytes[0] | 0x20) == b'x' && bytes[1] == b'\'' {
        let hex = s[2..].strip_suffix('\'')?;
        if hex.len() % 2 != 0 {
            return None;
        }
        let mut out = Vec::with_capacity(hex.len() / 2);
        for pair in hex.as_bytes().chunks(2) {
            out.push(u8::from_str_radix(std::str::from_utf8(pair).ok()?, 16).ok()?);
        }
        return Some(Value::Blob(out));
    }
    // A number, possibly signed. `split_top_level` has already cut the
    // definition at its comma, so the token runs to whitespace or end.
    let token = s.split_whitespace().next()?;
    if let Ok(i) = token.parse::<i64>() {
        return Some(Value::Int(i));
    }
    token.parse::<f64>().ok().map(Value::Real)
}

/// A single-quoted SQLite string literal, in which `''` is an escaped quote.
fn text_literal(s: &str) -> Option<Value> {
    let mut out = String::new();
    let mut chars = s[1..].chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\'' {
            out.push(c);
            continue;
        }
        match chars.peek() {
            Some('\'') => {
                out.push('\'');
                chars.next();
            }
            // The closing quote.
            _ => return Some(Value::Text(out)),
        }
    }
    // Unterminated: not a literal this should guess at.
    None
}

/// The type token of one column definition: everything between the column name
/// and the first constraint keyword.
///
/// Kept as the source's own text rather than mapped here, because SQLite's type
/// names are open-ended (any identifier is legal, affinity is decided by
/// substring rules) and the GeoPackage spoke is the layer that knows which
/// spellings it cares about.
fn declared_type(part: &str, name: &str) -> Option<String> {
    // Step over the column name however it was quoted.
    let rest = match part.find(name) {
        Some(i) => &part[i + name.len()..],
        None => return None,
    };
    let rest = rest.trim_start_matches(['"', '\'', '`', ']']).trim();
    const CONSTRAINTS: [&str; 8] = [
        "PRIMARY", "NOT", "NULL", "UNIQUE", "CHECK", "DEFAULT", "COLLATE", "REFERENCES",
    ];
    let mut ty = String::new();
    for token in rest.split_whitespace() {
        if CONSTRAINTS.contains(&token.to_ascii_uppercase().trim_end_matches('(')) {
            break;
        }
        if !ty.is_empty() {
            ty.push(' ');
        }
        ty.push_str(token);
        // A parenthesised size (`VARCHAR(80)`) closes the type.
        if token.contains(')') {
            break;
        }
    }
    (!ty.is_empty()).then_some(ty)
}

/// Split on top-level commas, respecting parentheses and quotes.
fn split_top_level(s: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut cur = String::new();
    let mut depth = 0i32;
    let mut quote: Option<char> = None;
    for c in s.chars() {
        match quote {
            Some(q) => {
                cur.push(c);
                if c == q {
                    quote = None;
                }
            }
            None => match c {
                '"' | '\'' | '`' => {
                    quote = Some(c);
                    cur.push(c);
                }
                '[' => {
                    quote = Some(']');
                    cur.push(c);
                }
                '(' => {
                    depth += 1;
                    cur.push(c);
                }
                ')' => {
                    depth -= 1;
                    cur.push(c);
                }
                ',' if depth == 0 => {
                    parts.push(std::mem::take(&mut cur));
                }
                _ => cur.push(c),
            },
        }
    }
    parts.push(cur);
    parts
}

fn is_table_constraint(part: &str) -> bool {
    let word = part
        .trim_start()
        .split(|c: char| c.is_whitespace() || c == '(')
        .next()
        .unwrap_or("")
        .to_ascii_uppercase();
    matches!(
        word.as_str(),
        "CONSTRAINT" | "PRIMARY" | "UNIQUE" | "CHECK" | "FOREIGN"
    )
}

/// The column name at the start of a column definition, stripping any quoting.
fn first_identifier(part: &str) -> Option<String> {
    let s = part.trim_start();
    let mut chars = s.chars();
    let first = chars.next()?;
    let (open, close) = match first {
        '"' => ('"', '"'),
        '`' => ('`', '`'),
        '[' => ('[', ']'),
        _ => {
            // Unquoted: up to the next whitespace or '('.
            let name: String = s
                .chars()
                .take_while(|c| !c.is_whitespace() && *c != '(')
                .collect();
            return (!name.is_empty()).then_some(name);
        }
    };
    let _ = open;
    let mut name = String::new();
    for c in chars {
        if c == close {
            break;
        }
        name.push(c);
    }
    (!name.is_empty()).then_some(name)
}

fn as_text(row: &[Value], i: usize) -> Option<String> {
    match row.get(i) {
        Some(Value::Text(s)) => Some(s.clone()),
        _ => None,
    }
}

fn as_int(row: &[Value], i: usize) -> Option<i64> {
    match row.get(i) {
        Some(Value::Int(n)) => Some(*n),
        _ => None,
    }
}

// --- writer ----------------------------------------------------------------

const WRITE_PAGE_SIZE: usize = 4096;

/// SQLite's AUTOINCREMENT bookkeeping table.
pub const SEQUENCE_TABLE: &str = "sqlite_sequence";

/// Its DDL, **verbatim as SQLite stores it** — no space after the table name,
/// and no type on either column. Read off a `sqlite3`-built file rather than
/// written from memory, because this string lands in `sqlite_master` and is
/// what any other reader will compare against.
const SEQUENCE_DDL: &str = "CREATE TABLE sqlite_sequence(name,seq)";

/// A table's participation in `sqlite_sequence`, SQLite's AUTOINCREMENT
/// bookkeeping.
///
/// # Why this is not derivable
///
/// `sqlite_sequence` holds a *high-water mark*, not `max(rowid)`. Measured:
/// insert three rows and delete the third and the mark stays at 3 while
/// `max(id)` is 2; delete *every* row and the mark still stands. Nothing in
/// the DDL or the rows regenerates it, which is why `container.rs`'s reasoning
/// that `sqlite_*` objects "follow the DDL rather than carrying anything" is
/// false of this one.
///
/// # Why omitting it is worse than losing an id
///
/// The recorded cost used to be that a later insert could reuse a retired id.
/// Measured 2026-08-23, on a file shaped the way this writer emits one — DDL
/// declaring `AUTOINCREMENT`, no sequence table, no orphaned pages:
///
/// ```text
/// PRAGMA integrity_check  -> ok
/// SELECT                  -> works
/// INSERT                  -> database disk image is malformed (11)
/// ```
///
/// So it is the same class of malformation as a missing autoindex
/// (`handoff.org` § BUGS FOUND entry 15) and **strictly harder to catch**:
/// entry 15 fails `integrity_check`, and this passes it.
///
/// Constructed today only by this module's tests, on the same footing as
/// [`Database::sequence_numbers`] beside it: the capability landed ahead of
/// M3b's carrying deliberately, because this is the gap the *first* carried
/// table trips and the one the verification ritual cannot see.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
pub enum Autoincrement {
    /// The DDL declares `AUTOINCREMENT` and SQLite holds no row for the table
    /// — nothing has ever been inserted. The `sqlite_sequence` *table* still
    /// has to exist, because SQLite creates it at `CREATE TABLE` time and
    /// every `INSERT` fails without it.
    Declared,
    /// The mark SQLite recorded for this table.
    HighWater(i64),
}

/// A table to write: its DDL (`CREATE TABLE …`, recorded in `sqlite_master`)
/// and its rows as `(rowid, values)`, rowid-ordered. A value at the
/// INTEGER PRIMARY KEY column should be `Null` (the rowid carries it).
pub struct TableSpec {
    pub name: String,
    pub sql: String,
    pub rows: Vec<(i64, Vec<Value>)>,
    /// The **implicit** indexes this table's DDL requires, in
    /// `sqlite_autoindex_<name>_1`, `_2`, … order — each as its records, in
    /// index order (see [`autoindex_records`]).
    ///
    /// SQLite creates one of these for every `UNIQUE` and every non-INTEGER
    /// `PRIMARY KEY`. **Declaring such a constraint without supplying its
    /// index produces a malformed database** — `SELECT` works, but
    /// `PRAGMA integrity_check` fails and every `INSERT` fails. That is
    /// `handoff.org` § BUGS FOUND entry 15, and it is why this field sits on
    /// `TableSpec` rather than being something a caller may forget: the DDL
    /// and the indexes it implies are one decision.
    ///
    /// Empty for a table whose only key is `INTEGER PRIMARY KEY`, which is a
    /// rowid alias and owns no separate b-tree.
    pub autoindexes: Vec<Vec<Vec<u8>>>,
    /// `Some` when this table's DDL declares `AUTOINCREMENT`.
    ///
    /// On the same reasoning as [`Self::autoindexes`], and it is the same
    /// failure: the DDL and the bookkeeping it obliges are **one decision**,
    /// so this sits on the spec rather than being a second call a caller may
    /// forget to make. [`write_database`] emits `sqlite_sequence` when any
    /// spec sets this, and a row for each that carries a mark.
    pub autoincrement: Option<Autoincrement>,
}

/// A `sqlite_master` entry that is not a [`TableSpec`]: a virtual table
/// (`CREATE VIRTUAL TABLE …`), a trigger, or an **index**.
///
/// The first two own no b-tree and take `records: None`, which writes
/// `rootpage` 0 — a virtual table lives in its shadow tables and a trigger is
/// pure schema. An index owns one, and supplies its records in index order.
pub struct MasterEntry {
    /// `sqlite_master.type` — `"table"` for a virtual table, `"trigger"`,
    /// `"index"`.
    pub kind: String,
    pub name: String,
    /// The table this entry is attached to (its own name for a virtual table,
    /// the triggering table for a trigger, the indexed table for an index).
    pub tbl_name: String,
    pub sql: String,
    /// An index's records, **already in index order** — as
    /// [`Database::read_index`] returns them.
    ///
    /// `Some` makes this entry own a b-tree and get a real `rootpage`; `None`
    /// leaves it schema-only. Per decision D10 in
    /// `plans/same-format-fidelity.org` this writer only ever *re-packs* an
    /// order some other index already established. **It does not sort**, and
    /// giving it unordered records produces a file SQLite will read as
    /// corrupt rather than an error here — there is nothing at this layer that
    /// could tell the difference, since ordering depends on a collation this
    /// module deliberately knows nothing about.
    pub records: Option<Vec<Vec<u8>>>,
}

/// Write a complete SQLite database from `tables` plus any schema-only
/// `extra_master` entries (virtual tables, triggers). `application_id` and
/// `user_version` populate the corresponding header fields (0 for plain SQLite).
pub fn write_database(
    tables: &[TableSpec],
    extra_master: &[MasterEntry],
    application_id: u32,
    user_version: u32,
) -> Result<Vec<u8>> {
    let mut w = Writer {
        pages: std::collections::BTreeMap::new(),
        next_page: 2, // page 1 is reserved for sqlite_master
    };

    // Build each table, then the indexes its own DDL implies. A
    // `sqlite_master` row per object, and the autoindexes follow their table
    // immediately — the layout SQLite itself produces.
    let mut master_rows: Vec<(i64, Vec<Value>)> = Vec::new();
    let mut next_rowid = 1i64;
    // Set once, by the first table declaring AUTOINCREMENT: there is exactly
    // one `sqlite_sequence` per database however many tables use it.
    let mut sequence_root: Option<u32> = None;
    for t in tables {
        let cells: Vec<(i64, Vec<u8>)> = t
            .rows
            .iter()
            .map(|(rowid, values)| (*rowid, w.leaf_cell(*rowid, &encode_record(values))))
            .collect();
        let root = w.build_btree(&cells)?;
        master_rows.push((
            next_rowid,
            vec![
                Value::Text("table".into()),
                Value::Text(t.name.clone()),
                Value::Text(t.name.clone()),
                Value::Int(root as i64),
                Value::Text(t.sql.clone()),
            ],
        ));
        next_rowid += 1;

        for (k, records) in t.autoindexes.iter().enumerate() {
            let root = w.build_index_btree(records)?;
            master_rows.push((
                next_rowid,
                vec![
                    Value::Text("index".into()),
                    Value::Text(format!("sqlite_autoindex_{}_{}", t.name, k + 1)),
                    Value::Text(t.name.clone()),
                    Value::Int(root as i64),
                    // An implicit index has no `CREATE INDEX` text of its own;
                    // SQLite stores NULL here and re-derives it from the DDL.
                    Value::Null,
                ],
            ));
            next_rowid += 1;
        }

        // `sqlite_sequence` goes in directly after the *first* table that
        // declares AUTOINCREMENT, which is where SQLite itself puts it — it
        // creates the table on the first such `CREATE TABLE`, not on the first
        // `INSERT`. Verified by reading a `sqlite3`-built file: with tables
        // `plain`, `a`, `b` and AUTOINCREMENT on the last two, the entry sits
        // between `a` and `b`.
        if t.autoincrement.is_some() && sequence_root.is_none() {
            let rows: Vec<(i64, Vec<Value>)> = tables
                .iter()
                .filter_map(|t| match t.autoincrement {
                    // A table that declares AUTOINCREMENT and has never been
                    // inserted into holds no row at all — the table's
                    // existence is what its `INSERT` needs, not a mark.
                    Some(Autoincrement::HighWater(n)) => Some((t.name.clone(), n)),
                    _ => None,
                })
                .enumerate()
                .map(|(i, (name, n))| {
                    ((i + 1) as i64, vec![Value::Text(name), Value::Int(n)])
                })
                .collect();
            let cells: Vec<(i64, Vec<u8>)> = rows
                .iter()
                .map(|(rowid, values)| (*rowid, w.leaf_cell(*rowid, &encode_record(values))))
                .collect();
            let root = w.build_btree(&cells)?;
            sequence_root = Some(root);
            master_rows.push((
                next_rowid,
                vec![
                    Value::Text("table".into()),
                    Value::Text(SEQUENCE_TABLE.into()),
                    Value::Text(SEQUENCE_TABLE.into()),
                    Value::Int(root as i64),
                    // Verbatim, and the spelling matters: it is what SQLite
                    // stores, down to having no space after the table name and
                    // no types on either column.
                    Value::Text(SEQUENCE_DDL.into()),
                ],
            ));
            next_rowid += 1;
        }
    }

    // Then the entries that are not tables, keeping rowid order. `rootpage` is
    // per-entry: 0 for the schema-only ones (a virtual table lives in its
    // shadow tables; a trigger is pure schema), and a real page for an index,
    // whose b-tree is built here from the records it carries.
    for e in extra_master {
        let root = match &e.records {
            Some(records) => w.build_index_btree(records)?,
            None => 0,
        };
        master_rows.push((
            next_rowid,
            vec![
                Value::Text(e.kind.clone()),
                Value::Text(e.name.clone()),
                Value::Text(e.tbl_name.clone()),
                Value::Int(root as i64),
                Value::Text(e.sql.clone()),
            ],
        ));
        next_rowid += 1;
    }

    // sqlite_master is a table b-tree rooted at page 1, whose page header sits
    // after the 100-byte database header. Small schemas fit one leaf on page 1;
    // larger ones (e.g. many R*Tree triggers) grow into a proper b-tree whose
    // interior root stays on page 1.
    let master_cells: Vec<(i64, Vec<u8>)> = master_rows
        .iter()
        .map(|(rowid, values)| (*rowid, w.leaf_cell(*rowid, &encode_record(values))))
        .collect();
    w.build_master(&master_cells)?;

    let total_pages = *w.pages.keys().max().unwrap_or(&1);
    let mut out = vec![0u8; total_pages as usize * WRITE_PAGE_SIZE];
    for (n, bytes) in &w.pages {
        let start = (*n as usize - 1) * WRITE_PAGE_SIZE;
        out[start..start + WRITE_PAGE_SIZE].copy_from_slice(bytes);
    }
    write_db_header(&mut out, total_pages, application_id, user_version);
    Ok(out)
}

struct Writer {
    pages: std::collections::BTreeMap<u32, Vec<u8>>,
    next_page: u32,
}

impl Writer {
    fn alloc(&mut self) -> u32 {
        let p = self.next_page;
        self.next_page += 1;
        p
    }

    /// Assemble one leaf-cell's on-page bytes: `varint(payload_len)
    /// varint(rowid) [inline payload] [overflow-page]`, spilling to overflow
    /// pages when the record is too large for the page.
    fn leaf_cell(&mut self, rowid: i64, record: &[u8]) -> Vec<u8> {
        let u = WRITE_PAGE_SIZE;
        let max_local = u - 35;
        let (local, overflow) = if record.len() <= max_local {
            (record.len(), false)
        } else {
            let min_local = ((u - 12) * 32 / 255) - 23;
            let k = min_local + (record.len() - min_local) % (u - 4);
            (if k <= max_local { k } else { min_local }, true)
        };

        let mut cell = Vec::new();
        write_varint(&mut cell, record.len() as u64);
        write_varint(&mut cell, rowid as u64);
        cell.extend_from_slice(&record[..local]);
        if overflow {
            let first = self.write_overflow(&record[local..]);
            cell.extend_from_slice(&first.to_be_bytes());
        }
        cell
    }

    fn write_overflow(&mut self, data: &[u8]) -> u32 {
        let cap = WRITE_PAGE_SIZE - 4;
        let n = data.len().div_ceil(cap);
        let page_nos: Vec<u32> = (0..n).map(|_| self.alloc()).collect();
        for i in 0..n {
            let mut page = vec![0u8; WRITE_PAGE_SIZE];
            let next = if i + 1 < n { page_nos[i + 1] } else { 0 };
            page[0..4].copy_from_slice(&next.to_be_bytes());
            let start = i * cap;
            let end = (start + cap).min(data.len());
            page[4..4 + (end - start)].copy_from_slice(&data[start..end]);
            self.pages.insert(page_nos[i], page);
        }
        page_nos[0]
    }

    /// Build a table b-tree from rowid-ordered cells; returns the root page.
    /// The root may land on any page ≥ 2 (unlike `sqlite_master`, which must
    /// root on page 1 — see [`Writer::build_master`]).
    fn build_btree(&mut self, cells: &[(i64, Vec<u8>)]) -> Result<u32> {
        let mut level = self.pack_leaves(cells, WRITE_PAGE_SIZE - 8)?;
        while level.len() > 1 {
            level = self.pack_interior(&level, WRITE_PAGE_SIZE - 12)?;
        }
        Ok(level[0].0)
    }

    /// Build the `sqlite_master` b-tree with its root on page 1, whose header
    /// starts after the 100-byte database header. A schema that fits one leaf
    /// becomes a leaf page 1; otherwise leaves and inner nodes live on pages
    /// ≥ 2 and the interior root sits on page 1. Interior nodes are packed with
    /// the page-1 capacity so the eventual root always fits there.
    fn build_master(&mut self, cells: &[(i64, Vec<u8>)]) -> Result<()> {
        const H: usize = 100;

        // Fits a single leaf on page 1?
        let leaf_used: usize = cells.iter().map(|(_, c)| 2 + c.len()).sum();
        if H + 8 + leaf_used <= WRITE_PAGE_SIZE {
            let refs: Vec<&(i64, Vec<u8>)> = cells.iter().collect();
            let page1 = self.render_leaf_at(H, &refs)?;
            self.pages.insert(1, page1);
            return Ok(());
        }

        // Otherwise: leaves on pages ≥ 2 (capped at the page-1 leaf size, so a
        // schema that overflowed page 1 always yields ≥ 2 leaves — never a
        // degenerate single-child root), then interior levels until the root
        // fits page 1.
        let mut level = self.pack_leaves(cells, WRITE_PAGE_SIZE - H - 8)?;
        loop {
            if interior_fits(&level, WRITE_PAGE_SIZE - H - 12) {
                let mut page = vec![0u8; WRITE_PAGE_SIZE];
                write_interior(&mut page, H, &level)?;
                self.pages.insert(1, page);
                return Ok(());
            }
            level = self.pack_interior(&level, WRITE_PAGE_SIZE - H - 12)?;
        }
    }

    /// Build an **index** b-tree from records already in index order; returns
    /// the root page. The read-side counterpart is
    /// [`Database::read_index`], and D10 in `plans/same-format-fidelity.org`
    /// is why only this direction exists: an index being carried is already
    /// sorted, so nothing here compares two keys.
    ///
    /// The shape differs from [`Writer::build_btree`] in one way that matters:
    /// an index interior cell carries a *real record*, not just a key. So
    /// building a level **promotes** one record out of the leaf sequence to
    /// serve as each divider, and that record then lives only in the interior
    /// node. Leaving it in the leaf as well would duplicate an entry.
    fn build_index_btree(&mut self, records: &[Vec<u8>]) -> Result<u32> {
        // An empty index still needs one (empty) leaf page as its root.
        if records.is_empty() {
            let page_no = self.alloc();
            let page = render_index_leaf(&[])?;
            self.pages.insert(page_no, page);
            return Ok(page_no);
        }

        let cap = WRITE_PAGE_SIZE - 8;
        let mut children: Vec<u32> = Vec::new();
        let mut dividers: Vec<Vec<u8>> = Vec::new();
        let mut i = 0;
        while i < records.len() {
            // Choose the group by *size* before building any cell: materialising
            // one allocates its overflow pages, and a cell built and then
            // rejected would leave those pages unreferenced — which SQLite's
            // own integrity check reports as "Page N: never used".
            let mut used = 0;
            let start = i;
            while i < records.len() {
                let sz = 2 + index_cell_size(records[i].len());
                if i > start && used + sz > cap {
                    break;
                }
                used += sz;
                i += 1;
            }
            let cells: Vec<Vec<u8>> = (start..i)
                .map(|k| self.index_leaf_cell(&records[k]))
                .collect();
            let page_no = self.alloc();
            let page = render_index_leaf(&cells)?;
            self.pages.insert(page_no, page);
            children.push(page_no);

            // Promote the next record as the divider between this leaf and the
            // one after it, and do not place it in either.
            if i < records.len() {
                dividers.push(records[i].clone());
                i += 1;
            }
        }

        while children.len() > 1 {
            let (c, d) = self.pack_index_interior(&children, &dividers)?;
            children = c;
            dividers = d;
        }
        Ok(children[0])
    }

    /// One interior level over `children`, separated by `dividers`
    /// (`dividers.len() == children.len() - 1`). Returns the level above it,
    /// with the dividers that were promoted out of this one.
    fn pack_index_interior(
        &mut self,
        children: &[u32],
        dividers: &[Vec<u8>],
    ) -> Result<(Vec<u32>, Vec<Vec<u8>>)> {
        let cap = WRITE_PAGE_SIZE - 12;
        let mut out_children = Vec::new();
        let mut out_dividers = Vec::new();
        let mut start = 0;
        while start < children.len() {
            // Grow the node while its dividers fit; `end` indexes the child
            // that becomes the right-most pointer, so the node spans
            // `children[start..=end]` and `dividers[start..end]`.
            let mut end = start;
            let mut used = 0;
            while end < children.len() - 1 {
                let cost = 2 + 4 + index_cell_size(dividers[end].len());
                if end > start && used + cost > cap {
                    break;
                }
                used += cost;
                end += 1;
            }
            let mut cells = Vec::with_capacity(end - start);
            for k in start..end {
                cells.push(self.index_interior_cell(children[k], &dividers[k]));
            }
            let page_no = self.alloc();
            let mut page = vec![0u8; WRITE_PAGE_SIZE];
            write_index_interior(&mut page, &cells, children[end])?;
            self.pages.insert(page_no, page);
            out_children.push(page_no);

            // The divider just past this node separates it from the next, so it
            // is promoted rather than stored here.
            if end < dividers.len() {
                out_dividers.push(dividers[end].clone());
            }
            start = end + 1;
        }
        Ok((out_children, out_dividers))
    }

    /// An index leaf cell: `varint(payload_len)` + as much payload as fits,
    /// plus an overflow page pointer when it does not all fit.
    fn index_leaf_cell(&mut self, record: &[u8]) -> Vec<u8> {
        let local = local_payload(record.len(), WRITE_PAGE_SIZE, index_max_local());
        let mut cell = Vec::new();
        write_varint(&mut cell, record.len() as u64);
        cell.extend_from_slice(&record[..local]);
        if local < record.len() {
            let first = self.write_overflow(&record[local..]);
            cell.extend_from_slice(&first.to_be_bytes());
        }
        cell
    }

    /// An index interior cell: a 4-byte left-child pointer, then exactly what a
    /// leaf cell is.
    fn index_interior_cell(&mut self, child: u32, record: &[u8]) -> Vec<u8> {
        let mut cell = child.to_be_bytes().to_vec();
        cell.extend_from_slice(&self.index_leaf_cell(record));
        cell
    }

    /// Pack cells into leaf pages of capacity `cap`; returns `(page, max_rowid)`
    /// Pack cells into leaf pages of capacity `cap`; returns `(page, max_rowid)`
    /// per page.
    fn pack_leaves(&mut self, cells: &[(i64, Vec<u8>)], cap: usize) -> Result<Vec<(u32, i64)>> {
        let mut out = Vec::new();
        let mut i = 0;
        // An empty table still needs one (empty) leaf page as its root.
        if cells.is_empty() {
            let page_no = self.alloc();
            let page = self.render_leaf_at(0, &[])?;
            self.pages.insert(page_no, page);
            return Ok(vec![(page_no, 0)]);
        }
        while i < cells.len() {
            let mut used = 0;
            let mut group: Vec<&(i64, Vec<u8>)> = Vec::new();
            while i < cells.len() {
                let sz = 2 + cells[i].1.len();
                if !group.is_empty() && used + sz > cap {
                    break;
                }
                if sz > cap {
                    return err("cell too large to fit a page");
                }
                used += sz;
                group.push(&cells[i]);
                i += 1;
            }
            let page_no = self.alloc();
            let page = self.render_leaf_at(0, &group)?;
            self.pages.insert(page_no, page);
            out.push((page_no, group.last().unwrap().0));
        }
        Ok(out)
    }

    /// One interior level over `children` (`(page, max_rowid)`), each node
    /// packed to capacity `cap`.
    fn pack_interior(&mut self, children: &[(u32, i64)], cap: usize) -> Result<Vec<(u32, i64)>> {
        let mut out = Vec::new();
        let mut i = 0;
        while i < children.len() {
            let mut used = 0;
            let mut j = i;
            while j < children.len() {
                let cost = 2 + 4 + varint_len(children[j].1 as u64);
                if j > i && used + cost > cap {
                    break;
                }
                used += cost;
                j += 1;
            }
            let group = &children[i..j];
            let page_no = self.alloc();
            let mut page = vec![0u8; WRITE_PAGE_SIZE];
            write_interior(&mut page, 0, group)?;
            self.pages.insert(page_no, page);
            out.push((page_no, group.last().unwrap().1));
            i = j;
        }
        Ok(out)
    }

    fn render_leaf_at(&self, header_offset: usize, cells: &[&(i64, Vec<u8>)]) -> Result<Vec<u8>> {
        let mut page = vec![0u8; WRITE_PAGE_SIZE];
        let mut content = WRITE_PAGE_SIZE;
        let mut ptrs = Vec::with_capacity(cells.len());
        for (_, cell) in cells {
            content -= cell.len();
            page[content..content + cell.len()].copy_from_slice(cell);
            ptrs.push(content as u16);
        }
        if header_offset + 8 + 2 * cells.len() > content {
            return err("leaf page overflow");
        }
        page[header_offset] = 0x0d;
        page[header_offset + 3..header_offset + 5].copy_from_slice(&(cells.len() as u16).to_be_bytes());
        page[header_offset + 5..header_offset + 7].copy_from_slice(&(content as u16).to_be_bytes());
        for (k, p) in ptrs.iter().enumerate() {
            let at = header_offset + 8 + k * 2;
            page[at..at + 2].copy_from_slice(&p.to_be_bytes());
        }
        Ok(page)
    }

}

/// Whether an interior page over `children` fits in `cap` bytes (all cells but
/// the last, which becomes the right-pointer, plus their pointer-array slots).
fn interior_fits(children: &[(u32, i64)], cap: usize) -> bool {
    if children.len() <= 1 {
        return true;
    }
    let used: usize = children[..children.len() - 1]
        .iter()
        .map(|(_, key)| 2 + 4 + varint_len(*key as u64))
        .sum();
    used <= cap
}

/// Render an interior table page into `page` with its header at `header_offset`
/// (0 for an ordinary page, 100 for the `sqlite_master` root on page 1). The
/// last child becomes the right-most pointer; the rest are `(child, max_rowid)`
/// cells.
fn write_interior(page: &mut [u8], header_offset: usize, group: &[(u32, i64)]) -> Result<()> {
    let right = group.last().unwrap().0;
    let cells: Vec<Vec<u8>> = group[..group.len() - 1]
        .iter()
        .map(|(child, key)| {
            let mut c = child.to_be_bytes().to_vec();
            write_varint(&mut c, *key as u64);
            c
        })
        .collect();

    let mut content = WRITE_PAGE_SIZE;
    let mut ptrs = Vec::with_capacity(cells.len());
    for cell in &cells {
        content -= cell.len();
        page[content..content + cell.len()].copy_from_slice(cell);
        ptrs.push(content as u16);
    }
    if header_offset + 12 + 2 * cells.len() > content {
        return err("interior page overflow");
    }
    page[header_offset] = 0x05;
    page[header_offset + 3..header_offset + 5].copy_from_slice(&(cells.len() as u16).to_be_bytes());
    page[header_offset + 5..header_offset + 7].copy_from_slice(&(content as u16).to_be_bytes());
    page[header_offset + 8..header_offset + 12].copy_from_slice(&right.to_be_bytes());
    for (k, p) in ptrs.iter().enumerate() {
        let at = header_offset + 12 + k * 2;
        page[at..at + 2].copy_from_slice(&p.to_be_bytes());
    }
    Ok(())
}

/// The records of an implicit index over `key_cols` of `rows`, in index order,
/// ready for [`TableSpec::autoindexes`].
///
/// An index record is the key columns followed by the rowid of the row it
/// points at, and the order is by key columns then rowid — so this both builds
/// the records and sorts them.
///
/// **This is the one place in this module that orders anything**, and D10 in
/// `plans/same-format-fidelity.org` draws the boundary deliberately: an index
/// being *carried* is re-packed in the order the source file already
/// established, and only an index this writer *builds for its own tables* is
/// constructed here. The distinction is what keeps [`compare_values`] small —
/// see its note on collations.
pub fn autoindex_records(rows: &[(i64, Vec<Value>)], key_cols: &[usize]) -> Vec<Vec<u8>> {
    let mut keyed: Vec<(Vec<Value>, i64)> = rows
        .iter()
        .map(|(rowid, values)| {
            let key: Vec<Value> = key_cols
                .iter()
                .map(|&c| values.get(c).cloned().unwrap_or(Value::Null))
                .collect();
            (key, *rowid)
        })
        .collect();

    keyed.sort_by(|(ka, ra), (kb, rb)| {
        for (a, b) in ka.iter().zip(kb) {
            match compare_values(a, b) {
                std::cmp::Ordering::Equal => {}
                other => return other,
            }
        }
        ra.cmp(rb)
    });

    keyed
        .into_iter()
        .map(|(mut key, rowid)| {
            key.push(Value::Int(rowid));
            encode_record(&key)
        })
        .collect()
}

/// SQLite's ordering over two values, under the **default `BINARY`**
/// collation: `NULL` first, then numbers, then text, then blobs, with text and
/// blobs compared byte by byte.
///
/// *No other collation is implemented, and that is a scoped decision rather
/// than a gap.* `NOCASE` and `RTRIM` can only appear in DDL this crate did not
/// write, and an index on such a table is **carried** rather than constructed
/// (D10) — so it is re-packed in the order that file already had, and nothing
/// here has to know how it was reached. The registry tables this writer builds
/// declare no `COLLATE` clause anywhere in their specified DDL.
fn compare_values(a: &Value, b: &Value) -> std::cmp::Ordering {
    use std::cmp::Ordering;

    // SQLite's storage-class order, which outranks any comparison within a
    // class: NULL < INTEGER = REAL < TEXT < BLOB.
    fn class(v: &Value) -> u8 {
        match v {
            Value::Null => 0,
            Value::Int(_) | Value::Real(_) => 1,
            Value::Text(_) => 2,
            Value::Blob(_) => 3,
        }
    }
    match class(a).cmp(&class(b)) {
        Ordering::Equal => {}
        other => return other,
    }
    match (a, b) {
        (Value::Null, Value::Null) => Ordering::Equal,
        // Integers and reals compare numerically across the two types. An i64
        // beyond f64's exact range would compare wrongly as a float, so the
        // integer pair is kept exact.
        (Value::Int(x), Value::Int(y)) => x.cmp(y),
        (Value::Real(x), Value::Real(y)) => x.total_cmp(y),
        (Value::Int(x), Value::Real(y)) => (*x as f64).total_cmp(y),
        (Value::Real(x), Value::Int(y)) => x.total_cmp(&(*y as f64)),
        (Value::Text(x), Value::Text(y)) => x.as_bytes().cmp(y.as_bytes()),
        (Value::Blob(x), Value::Blob(y)) => x.as_slice().cmp(y.as_slice()),
        // Unreachable: the class comparison above has already returned.
        _ => Ordering::Equal,
    }
}

/// The on-page payload ceiling for an index cell, at this writer's page size.
fn index_max_local() -> usize {
    ((WRITE_PAGE_SIZE - 12) * 64 / 255) - 23
}

/// The byte cost of an index cell holding a record of `record_len` bytes,
/// excluding its 2-byte pointer-array slot. Computed without building the cell,
/// so a group can be chosen before any overflow page is allocated.
fn index_cell_size(record_len: usize) -> usize {
    let local = local_payload(record_len, WRITE_PAGE_SIZE, index_max_local());
    varint_len(record_len as u64) + local + if local < record_len { 4 } else { 0 }
}

/// Render an index leaf page (`0x0a`) from pre-built cells.
fn render_index_leaf(cells: &[Vec<u8>]) -> Result<Vec<u8>> {
    let mut page = vec![0u8; WRITE_PAGE_SIZE];
    let mut content = WRITE_PAGE_SIZE;
    let mut ptrs = Vec::with_capacity(cells.len());
    for cell in cells {
        content -= cell.len();
        page[content..content + cell.len()].copy_from_slice(cell);
        ptrs.push(content as u16);
    }
    if 8 + 2 * cells.len() > content {
        return err("index leaf page overflow");
    }
    page[0] = 0x0a;
    page[3..5].copy_from_slice(&(cells.len() as u16).to_be_bytes());
    page[5..7].copy_from_slice(&(content as u16).to_be_bytes());
    for (k, p) in ptrs.iter().enumerate() {
        page[8 + k * 2..10 + k * 2].copy_from_slice(&p.to_be_bytes());
    }
    Ok(page)
}

/// Render an index interior page (`0x02`) from pre-built cells plus the
/// right-most child pointer.
fn write_index_interior(page: &mut [u8], cells: &[Vec<u8>], right: u32) -> Result<()> {
    let mut content = WRITE_PAGE_SIZE;
    let mut ptrs = Vec::with_capacity(cells.len());
    for cell in cells {
        content -= cell.len();
        page[content..content + cell.len()].copy_from_slice(cell);
        ptrs.push(content as u16);
    }
    if 12 + 2 * cells.len() > content {
        return err("index interior page overflow");
    }
    page[0] = 0x02;
    page[3..5].copy_from_slice(&(cells.len() as u16).to_be_bytes());
    page[5..7].copy_from_slice(&(content as u16).to_be_bytes());
    page[8..12].copy_from_slice(&right.to_be_bytes());
    for (k, p) in ptrs.iter().enumerate() {
        page[12 + k * 2..14 + k * 2].copy_from_slice(&p.to_be_bytes());
    }
    Ok(())
}

/// How many bytes of a `total`-byte payload live on the page itself, the rest
/// How many bytes of a `total`-byte payload live on the page itself, the rest
/// going to an overflow chain. `max_local` is the page-type's ceiling: a table
/// leaf keeps `usable - 35`, an index cell `((usable - 12) * 64 / 255) - 23`.
///
/// The `min_local`/`k` dance is SQLite's own, and its purpose is worth knowing
/// before anyone "simplifies" it: it makes the *spilled* remainder land in a
/// useful size band rather than leaving a few bytes on the page and a nearly
/// empty overflow page after it.
fn local_payload(total: usize, usable: usize, max_local: usize) -> usize {
    if total <= max_local {
        return total;
    }
    let min_local = ((usable - 12) * 32 / 255) - 23;
    let k = min_local + (total - min_local) % (usable - 4);
    if k <= max_local { k } else { min_local }
}

/// Encode a row into the SQLite record format: `varint(header_len)` + serial
/// types + value bodies.
fn encode_record(values: &[Value]) -> Vec<u8> {
    let mut serials = Vec::new();
    let mut body = Vec::new();
    for v in values {
        let (serial, bytes) = encode_value(v);
        write_varint(&mut serials, serial);
        body.extend_from_slice(&bytes);
    }
    // header_len includes its own varint; solve the tiny fixpoint.
    let mut header_len = serials.len() + 1;
    while varint_len(header_len as u64) + serials.len() != header_len {
        header_len = varint_len(header_len as u64) + serials.len();
    }
    let mut out = Vec::new();
    write_varint(&mut out, header_len as u64);
    out.extend_from_slice(&serials);
    out.extend_from_slice(&body);
    out
}

fn encode_value(v: &Value) -> (u64, Vec<u8>) {
    match v {
        Value::Null => (0, Vec::new()),
        Value::Int(0) => (8, Vec::new()),
        Value::Int(1) => (9, Vec::new()),
        Value::Int(n) => (6, n.to_be_bytes().to_vec()),
        Value::Real(f) => (7, f.to_be_bytes().to_vec()),
        Value::Text(s) => (13 + 2 * s.len() as u64, s.as_bytes().to_vec()),
        Value::Blob(b) => (12 + 2 * b.len() as u64, b.clone()),
    }
}

fn write_varint(out: &mut Vec<u8>, mut v: u64) {
    if v & 0xff00_0000_0000_0000 != 0 {
        // 9-byte form: 8 groups of 7 bits then a full trailing byte.
        let mut buf = [0u8; 9];
        buf[8] = (v & 0xff) as u8;
        v >>= 8;
        for j in (0..8).rev() {
            buf[j] = ((v & 0x7f) | 0x80) as u8;
            v >>= 7;
        }
        out.extend_from_slice(&buf);
        return;
    }
    let mut groups = Vec::new();
    loop {
        groups.push((v & 0x7f) as u8);
        v >>= 7;
        if v == 0 {
            break;
        }
    }
    for k in (0..groups.len()).rev() {
        let b = if k == 0 { groups[k] } else { groups[k] | 0x80 };
        out.push(b);
    }
}

fn varint_len(v: u64) -> usize {
    let mut tmp = Vec::new();
    write_varint(&mut tmp, v);
    tmp.len()
}

/// Fill in the 100-byte database header on page 1.
fn write_db_header(out: &mut [u8], total_pages: u32, application_id: u32, user_version: u32) {
    out[0..16].copy_from_slice(HEADER_MAGIC);
    out[16..18].copy_from_slice(&(WRITE_PAGE_SIZE as u16).to_be_bytes());
    out[18] = 1; // write version (legacy)
    out[19] = 1; // read version (legacy)
    out[20] = 0; // reserved bytes per page
    out[21] = 64; // max embedded payload fraction
    out[22] = 32; // min embedded payload fraction
    out[23] = 32; // leaf payload fraction
    out[24..28].copy_from_slice(&1u32.to_be_bytes()); // file change counter
    out[28..32].copy_from_slice(&total_pages.to_be_bytes());
    out[40..44].copy_from_slice(&1u32.to_be_bytes()); // schema cookie
    out[44..48].copy_from_slice(&4u32.to_be_bytes()); // schema format
    out[56..60].copy_from_slice(&1u32.to_be_bytes()); // text encoding = UTF-8
    out[60..64].copy_from_slice(&user_version.to_be_bytes());
    out[68..72].copy_from_slice(&application_id.to_be_bytes());
    out[92..96].copy_from_slice(&1u32.to_be_bytes()); // version-valid-for
    out[96..100].copy_from_slice(&3_045_000u32.to_be_bytes()); // SQLITE_VERSION_NUMBER
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An `ogr2ogr` base carrying the constraint shapes that need an index
    /// b-tree: `UNIQUE`, a `TEXT PRIMARY KEY`, an explicit `CREATE INDEX`, and
    /// a `UNIQUE COLLATE NOCASE`. See `tests/fixtures/HOSTILE.org`.
    const CONSTRAINTS: &[u8] = include_bytes!("../tests/fixtures/sqlite_constraints.gpkg");

    /// Root page of `name` in `sqlite_master`.
    fn root_of(db: &Database, name: &str) -> u32 {
        db.master_objects()
            .unwrap()
            .into_iter()
            .find(|o| o.name == name)
            .unwrap_or_else(|| panic!("no sqlite_master entry named {name}"))
            .root_page
    }

    /// The last column of an index record is the rowid it points at.
    fn trailing_rowid(record: &[u8]) -> i64 {
        let values = decode_record(record).unwrap();
        match values.last() {
            Some(Value::Int(n)) => *n,
            other => panic!("index record does not end in a rowid: {other:?}"),
        }
    }

    /// An index b-tree can be read, and it comes back *in index order* rather
    /// than in table order — which is the whole reason a carried index needs no
    /// comparison logic (D10).
    #[test]
    fn an_index_btree_reads_back_in_index_order() {
        let db = Database::open(CONSTRAINTS).unwrap();

        // `tags` holds ('alpha',1), ('beta',2), ('gamma',3) — inserted in an
        // order that already matches, so it cannot distinguish the two orders.
        // `idx_events_kind` can: 'open' was inserted at rowid 1 and 'close' at
        // rowid 2, so index order ('close' < 'open') reverses them.
        let records = db.read_index(root_of(&db, "idx_events_kind")).unwrap();
        let rowids: Vec<i64> = records.iter().map(|r| trailing_rowid(r)).collect();
        assert_eq!(rowids, vec![2, 1], "index order, not table order");

        // And an autoindex reads the same way.
        let auto = db.read_index(root_of(&db, "sqlite_autoindex_tags_1")).unwrap();
        assert_eq!(auto.len(), 3);
        assert_eq!(
            auto.iter().map(|r| trailing_rowid(r)).collect::<Vec<_>>(),
            vec![1, 2, 3]
        );
    }

    /// An index b-tree survives a write and reads back with every record in
    /// the same order — including at a size that forces interior nodes, which
    /// is where the divider promotion happens.
    ///
    /// A single-leaf index would pass this without ever exercising
    /// `pack_index_interior`, and the promotion is the one place this writer
    /// differs structurally from the table-b-tree case: a record promoted to a
    /// divider lives *only* in the interior node. Losing that record, or
    /// leaving a copy behind in the leaf, are the two natural bugs and both
    /// show up as a count mismatch here.
    #[test]
    fn an_index_btree_survives_a_write_at_every_depth() {
        // Enough records to need several leaves and at least one interior
        // level: each cell is ~40 bytes and a leaf holds under 4 KiB.
        for count in [0usize, 1, 3, 500] {
            let records: Vec<Vec<u8>> = (0..count)
                .map(|i| {
                    // Every fourth key is far past an index cell's on-page
                    // ceiling (~1002 bytes at this page size), so the overflow
                    // chain is exercised in the same pass as the packing. It
                    // was not, until 2026-08-23: the first version of this test
                    // used uniformly short keys and never reached
                    // `write_overflow` at all, which is the "unadventurous
                    // values" blind spot `handoff.org` records twice already.
                    let pad = if i % 4 == 0 { 2000 } else { 20 };
                    encode_record(&[
                        Value::Text(format!("key-{i:06}-{}", "x".repeat(pad))),
                        Value::Int(i as i64 + 1),
                    ])
                })
                .collect();

            let table = TableSpec {
                name: "t".into(),
                sql: "CREATE TABLE t(id INTEGER PRIMARY KEY, k TEXT)".into(),
                rows: Vec::new(),
                autoindexes: Vec::new(),
                autoincrement: None,
            };
            let entry = MasterEntry {
                kind: "index".into(),
                name: "idx_t_k".into(),
                tbl_name: "t".into(),
                sql: "CREATE INDEX idx_t_k ON t(k)".into(),
                records: Some(records.clone()),
            };
            let bytes = write_database(&[table], &[entry], 0, 0).unwrap();

            let db = Database::open(&bytes).unwrap();
            let root = root_of(&db, "idx_t_k");
            assert_ne!(root, 0, "an index must get a real root page (count={count})");
            let back = db.read_index(root).unwrap();
            assert_eq!(back.len(), records.len(), "record count (count={count})");
            assert_eq!(back, records, "records and their order (count={count})");
        }
    }

    /// A table declaring AUTOINCREMENT gets `sqlite_sequence`, with a row per
    /// table that carries a mark and no row for one that does not.
    #[test]
    fn a_declared_autoincrement_produces_the_sequence_table() {
        let spec = |name: &str, auto: Option<Autoincrement>| TableSpec {
            name: name.into(),
            sql: format!("CREATE TABLE {name}(id INTEGER PRIMARY KEY AUTOINCREMENT, v TEXT)"),
            rows: vec![(1, vec![Value::Null, Value::Text("x".into())])],
            autoindexes: Vec::new(),
            autoincrement: auto,
        };
        let bytes = write_database(
            &[
                // Declared and inserted into.
                spec("a", Some(Autoincrement::HighWater(7))),
                // Declared, never inserted into: no row, but its INSERT still
                // needs the table to be there.
                spec("b", Some(Autoincrement::Declared)),
            ],
            &[],
            0,
            0,
        )
        .unwrap();

        let db = Database::open(&bytes).unwrap();
        assert_eq!(db.sequence_numbers().unwrap(), vec![("a".to_string(), 7)]);

        // Placement: directly after the *first* declaring table, which is
        // where SQLite puts it.
        let names: Vec<String> =
            db.master_objects().unwrap().into_iter().map(|o| o.name).collect();
        assert_eq!(names, vec!["a", SEQUENCE_TABLE, "b"]);
    }

    /// No AUTOINCREMENT anywhere means no `sqlite_sequence` — the table is not
    /// furniture every database carries.
    ///
    /// This is what keeps the change invisible to every GeoPackage this crate
    /// writes today: none of its own tables declares AUTOINCREMENT, so none of
    /// them acquires a sequence table it does not need.
    #[test]
    fn a_database_without_autoincrement_has_no_sequence_table() {
        let bytes = write_database(
            &[TableSpec {
                name: "t".into(),
                sql: "CREATE TABLE t(id INTEGER PRIMARY KEY, v TEXT)".into(),
                rows: vec![(1, vec![Value::Null, Value::Text("x".into())])],
                autoindexes: Vec::new(),
                autoincrement: None,
            }],
            &[],
            0,
            0,
        )
        .unwrap();
        let db = Database::open(&bytes).unwrap();
        assert!(db.master_objects().unwrap().iter().all(|o| o.name != SEQUENCE_TABLE));
        assert!(db.sequence_numbers().unwrap().is_empty());
    }

    /// A mark read out of a real file is carried back unchanged, **including
    /// when it exceeds `max(rowid)`**.
    ///
    /// That is the case the whole feature turns on: if the mark were
    /// `max(rowid)` it could be recomputed and none of this would be needed.
    /// `sqlite_contents.gpkg` supplies real marks written by `sqlite3`.
    #[test]
    fn a_high_water_mark_survives_a_read_and_a_write() {
        const CONTENTS: &[u8] = include_bytes!("../tests/fixtures/sqlite_contents.gpkg");
        let src = Database::open(CONTENTS).unwrap();
        let marks = src.sequence_numbers().unwrap();
        assert!(
            marks.iter().any(|(n, _)| n == "gpkg_metadata"),
            "fixture should carry a gpkg_metadata mark: {marks:?}"
        );

        // Re-emit them through a table that declares AUTOINCREMENT, with a
        // mark deliberately above its only rowid.
        let bytes = write_database(
            &[TableSpec {
                name: "t".into(),
                sql: "CREATE TABLE t(id INTEGER PRIMARY KEY AUTOINCREMENT)".into(),
                rows: vec![(1, vec![Value::Null])],
                autoindexes: Vec::new(),
                autoincrement: Some(Autoincrement::HighWater(99)),
            }],
            &[],
            0,
            0,
        )
        .unwrap();
        let back = Database::open(&bytes).unwrap();
        assert_eq!(back.sequence_numbers().unwrap(), vec![("t".to_string(), 99)]);
    }

    /// **SQLite itself** accepts the AUTOINCREMENT bookkeeping this writer
    /// emits, which is the only check that means anything here.
    ///
    /// `PRAGMA integrity_check` cannot answer this question: a file whose DDL
    /// declares AUTOINCREMENT with no `sqlite_sequence` behind it passes
    /// `integrity_check` and fails every `INSERT` with "database disk image is
    /// malformed". Measured 2026-08-23 on a file built exactly that way. So
    /// this test asks the reference implementation to *insert*, which is the
    /// operation that actually exercises the table, and checks the new row got
    /// an id above the mark rather than reusing a retired one.
    ///
    /// `#[ignore]`d on the same footing as the index cross-check beside it: it
    /// needs a `sqlite3` binary on PATH.
    #[test]
    #[ignore = "needs the sqlite3 binary; run with --ignored"]
    fn sqlite_itself_accepts_the_autoincrement_bookkeeping() {
        use std::process::Command;

        let bytes = write_database(
            &[TableSpec {
                name: "t".into(),
                sql: "CREATE TABLE t(id INTEGER PRIMARY KEY AUTOINCREMENT, v TEXT)".into(),
                // Rowids 1 and 2, with the mark left at 41: the state a file
                // reaches by inserting 41 rows and deleting most of them.
                rows: vec![
                    (1, vec![Value::Null, Value::Text("a".into())]),
                    (2, vec![Value::Null, Value::Text("b".into())]),
                ],
                autoindexes: Vec::new(),
                autoincrement: Some(Autoincrement::HighWater(41)),
            }],
            &[],
            0,
            0,
        )
        .unwrap();

        let path = std::env::temp_dir().join("geosetta_sequence_crosscheck.db");
        std::fs::write(&path, &bytes).unwrap();
        let ask = |sql: &str| -> String {
            let out =
                Command::new("sqlite3").arg(&path).arg(sql).output().expect("sqlite3 on PATH");
            assert!(out.status.success(), "sqlite3 {sql}: {}", String::from_utf8_lossy(&out.stderr));
            String::from_utf8_lossy(&out.stdout).trim().to_string()
        };

        assert_eq!(ask("PRAGMA integrity_check;"), "ok");
        assert_eq!(ask("SELECT seq FROM sqlite_sequence WHERE name='t';"), "41");

        // The operation `integrity_check` cannot vouch for. Without the
        // sequence table this fails outright; with a wrong mark it returns 3.
        assert_eq!(ask("INSERT INTO t(v) VALUES('c'); SELECT id FROM t WHERE v='c';"), "42");

        let _ = std::fs::remove_file(&path);
    }

    /// The index this writer builds is checked by **SQLite itself**, not by
    /// this crate's reader.
    ///
    /// `an_index_btree_survives_a_write_at_every_depth` proves the writer and
    /// the reader agree, which is exactly the closure a shared misreading of
    /// the format would survive — the lesson `handoff.org` records from the
    /// `REAL` bug ("a writer never emits the spelling it cannot read"). So this
    /// one hands the bytes to the reference implementation and asks two
    /// questions our own code cannot: is the b-tree *well formed*, and does an
    /// index *scan* return the right rows?
    ///
    /// `#[ignore]`d and run with `cargo test -p geosetta -- --ignored`, on the
    /// same footing as the GDAL cross-checks in `tests/gdal_types.rs`: it needs
    /// a `sqlite3` binary on PATH.
    #[test]
    #[ignore = "needs the sqlite3 binary; run with --ignored"]
    fn sqlite_itself_validates_the_index_this_writer_builds() {
        use std::process::Command;

        // A real table with rows, plus an index over one of its columns — so
        // the index's trailing rowids have to line up with actual rows for a
        // scan to return anything.
        let n = 500i64;
        // Every fifth key overflows an index cell, so `integrity_check` is
        // asked about the overflow chains too — a chain whose pages are
        // mis-linked or left unreferenced is exactly what it reports.
        let key = |v: i64| -> String {
            if v % 5 == 0 { format!("k{v:04}{}", "y".repeat(2000)) } else { format!("k{v:04}") }
        };
        let rows: Vec<(i64, Vec<Value>)> = (1..=n)
            .map(|i| (i, vec![Value::Null, Value::Text(key(n - i))]))
            .collect();
        let table = TableSpec {
            name: "t".into(),
            sql: "CREATE TABLE t(id INTEGER PRIMARY KEY, k TEXT)".into(),
            rows,
            autoindexes: Vec::new(),
            autoincrement: None,
        };
        // Index order is by `k`, which reverses the rowids.
        let records: Vec<Vec<u8>> = (1..=n)
            .map(|i| encode_record(&[Value::Text(key(i - 1)), Value::Int(n - i + 1)]))
            .collect();
        let entry = MasterEntry {
            kind: "index".into(),
            name: "idx_t_k".into(),
            tbl_name: "t".into(),
            sql: "CREATE INDEX idx_t_k ON t(k)".into(),
            records: Some(records),
        };
        let bytes = write_database(&[table], &[entry], 0, 0).unwrap();

        let path = std::env::temp_dir().join("geosetta_index_crosscheck.db");
        std::fs::write(&path, &bytes).unwrap();

        let ask = |sql: &str| -> String {
            let out = Command::new("sqlite3")
                .arg(&path)
                .arg(sql)
                .output()
                .expect("sqlite3 on PATH");
            assert!(
                out.status.success(),
                "sqlite3 {sql}: {}",
                String::from_utf8_lossy(&out.stderr)
            );
            String::from_utf8_lossy(&out.stdout).trim().to_string()
        };

        // 1. The b-tree is well formed, and every page is reachable. This is
        //    the check that catches an orphaned overflow page, which is why
        //    `build_index_btree` sizes a cell before it builds one.
        assert_eq!(ask("PRAGMA integrity_check;"), "ok");

        // 2. The index is *usable*: a covering-index scan, which SQLite will
        //    only plan if it trusts the index, returns index order.
        assert_eq!(ask("SELECT count(*) FROM t ORDER BY k;"), n.to_string());
        // Strictly greater than the smallest key — which is an *overflowing*
        // one, so this also asks SQLite to compare a key it had to reassemble
        // from an overflow chain.
        assert_eq!(
            ask(&format!("SELECT count(*) FROM t WHERE k > '{}';", key(0))),
            (n - 1).to_string()
        );

        // 3. And it agrees with the table it points at — a lookup through the
        //    index resolves to the row that actually holds that key.
        // An overflowing key resolved through the index back to its own row.
        assert_eq!(ask(&format!("SELECT id FROM t WHERE k = '{}';", key(0))), n.to_string());

        std::fs::remove_file(&path).ok();
    }

    /// A `WITHOUT ROWID` table is *stored as* an index b-tree, which is why
    /// `walk_table` could never decode one — the read side of entry 11 in
    /// `handoff.org` § BUGS FOUND.
    #[test]
    fn a_without_rowid_table_is_an_index_btree() {
        const ALTERED: &[u8] = include_bytes!("../tests/fixtures/sqlite_altered.gpkg");
        let db = Database::open(ALTERED).unwrap();
        let root = root_of(&db, "kv");
        // The table walk still refuses it, which is the behaviour
        // `container.rs` classifies against.
        let mut cells = Vec::new();
        assert!(db.walk_table(root, &mut cells).is_err());
        // The index walk reads it.
        assert!(!db.read_index(root).unwrap().is_empty());
    }

    #[test]
    fn varint_values() {
        assert_eq!(varint(&[0x00], 0).unwrap(), (0, 1));
        assert_eq!(varint(&[0x7f], 0).unwrap(), (127, 1));
        assert_eq!(varint(&[0x81, 0x00], 0).unwrap(), (128, 2));
        assert_eq!(varint(&[0x82, 0x00], 0).unwrap(), (256, 2));
    }

    #[test]
    fn parses_column_ddl() {
        let sql = r#"CREATE TABLE "places" ( "fid" INTEGER PRIMARY KEY AUTOINCREMENT NOT NULL, "geom" GEOMETRY, "name" TEXT, "pop" MEDIUMINT, "score" DOUBLE)"#;
        let ddl = parse_columns(sql);
        assert_eq!(ddl.names, vec!["fid", "geom", "name", "pop", "score"]);
        assert_eq!(ddl.rowid_alias, Some(0));
        // The declared type is everything between the name and the first
        // constraint keyword — `INTEGER`, not `INTEGER PRIMARY KEY ...`.
        let types: Vec<Option<&str>> = ddl.types.iter().map(|t| t.as_deref()).collect();
        assert_eq!(
            types,
            vec![Some("INTEGER"), Some("GEOMETRY"), Some("TEXT"), Some("MEDIUMINT"), Some("DOUBLE")]
        );
    }

    /// A parenthesised size belongs to the type, and a column may legally
    /// declare no type at all.
    #[test]
    fn parses_sized_and_untyped_column_ddl() {
        let sql = r#"CREATE TABLE "t" ("a" VARCHAR(80) NOT NULL, "b", "c" DOUBLE PRECISION DEFAULT 0)"#;
        let ddl = parse_columns(sql);
        assert_eq!(ddl.names, vec!["a", "b", "c"]);
        let types: Vec<Option<&str>> = ddl.types.iter().map(|t| t.as_deref()).collect();
        assert_eq!(types, vec![Some("VARCHAR(80)"), None, Some("DOUBLE PRECISION")]);
        // The `DEFAULT` follows the type without ending it.
        assert_eq!(ddl.defaults, vec![None, None, Some(Value::Int(0))]);
    }

    /// Every literal form SQLite writes into a `DEFAULT` clause. These are the
    /// values a row predating an `ALTER TABLE … ADD COLUMN` reads back as.
    #[test]
    fn parses_default_literals() {
        let sql = "CREATE TABLE t(a, b TEXT DEFAULT 'B', c INT DEFAULT 5, d REAL DEFAULT -1.5, \
                   e BLOB DEFAULT X'0102', f BOOLEAN DEFAULT TRUE, g TEXT DEFAULT 'it''s', \
                   h TEXT, i TEXT DEFAULT NULL, j TEXT DEFAULT 'a, b')";
        let ddl = parse_columns(sql);
        assert_eq!(
            ddl.defaults,
            vec![
                None,
                Some(Value::Text("B".into())),
                Some(Value::Int(5)),
                Some(Value::Real(-1.5)),
                Some(Value::Blob(vec![0x01, 0x02])),
                // SQLite stores TRUE as the integer 1.
                Some(Value::Int(1)),
                Some(Value::Text("it's".into())),
                None,
                // An explicit NULL default is the same padding as no default.
                None,
                // A comma inside the literal does not split the definition.
                Some(Value::Text("a, b".into())),
            ]
        );
    }

    /// A default this cannot evaluate must read back NULL rather than a guess.
    /// SQLite forbids these on `ALTER TABLE … ADD COLUMN`, so no stored row can
    /// depend on one.
    #[test]
    fn declines_non_constant_defaults() {
        let sql = "CREATE TABLE t(a TEXT DEFAULT CURRENT_TIMESTAMP, b INT DEFAULT (1 + 1))";
        assert_eq!(parse_columns(sql).defaults, vec![None, None]);
    }

    /// `DEFAULT` inside a quoted name or a `CHECK` expression is not this
    /// column's default.
    #[test]
    fn ignores_default_that_is_not_a_clause() {
        let sql = r#"CREATE TABLE t("default" TEXT, b INT CHECK (b <> 7) DEFAULT 3)"#;
        let ddl = parse_columns(sql);
        assert_eq!(ddl.names, vec!["default", "b"]);
        assert_eq!(ddl.defaults, vec![None, Some(Value::Int(3))]);
    }

    #[test]
    fn writer_round_trips_through_reader() {
        // A table with a rowid alias, mixed types, and a large blob that forces
        // an overflow chain; plus enough rows to span multiple leaf pages.
        let big = vec![0xABu8; 9000];
        let mut rows = Vec::new();
        for i in 1..=300i64 {
            rows.push((
                i,
                vec![
                    Value::Null, // id INTEGER PRIMARY KEY -> carried by the rowid
                    Value::Text(format!("row {i}")),
                    Value::Int(i * 10),
                    Value::Real(i as f64 / 4.0),
                    if i == 1 { Value::Blob(big.clone()) } else { Value::Null },
                ],
            ));
        }
        let spec = TableSpec {
            name: "t".into(),
            sql: r#"CREATE TABLE "t" ("id" INTEGER PRIMARY KEY, "name" TEXT, "n" INTEGER, "r" REAL, "data" BLOB)"#.into(),
            rows,
            autoindexes: Vec::new(),
            autoincrement: None,
        };
        let bytes = write_database(&[spec], &[], 0, 0).unwrap();

        let db = Database::open(&bytes).unwrap();
        let table = db.tables().unwrap().into_iter().find(|t| t.name == "t").unwrap();
        assert_eq!(table.columns, vec!["id", "name", "n", "r", "data"]);
        let out = db.read_rows(&table).unwrap();
        assert_eq!(out.len(), 300);
        assert_eq!(out[0][0], Value::Int(1)); // rowid alias filled
        assert_eq!(out[0][1], Value::Text("row 1".into()));
        assert_eq!(out[0][2], Value::Int(10));
        assert_eq!(out[0][3], Value::Real(0.25));
        assert_eq!(out[0][4], Value::Blob(big)); // survived the overflow chain
        assert_eq!(out[299][0], Value::Int(300));
        assert_eq!(out[299][2], Value::Int(3000));
    }

    #[test]
    fn reads_a_real_geopackage() {
        // A DuckDB-written GPKG: layer "places" with fid/geom/name/pop/score.
        let bytes = include_bytes!("../tests/fixtures/points.gpkg");
        let db = Database::open(bytes).unwrap();
        let tables = db.tables().unwrap();

        // Required GPKG metadata tables plus the feature table are present.
        let names: Vec<&str> = tables.iter().map(|t| t.name.as_str()).collect();
        assert!(names.contains(&"gpkg_contents"));
        assert!(names.contains(&"places"));

        let places = tables.iter().find(|t| t.name == "places").unwrap();
        assert_eq!(places.columns, vec!["fid", "geom", "name", "pop", "score"]);
        let rows = db.read_rows(places).unwrap();
        assert_eq!(rows.len(), 2);

        // fid is the rowid alias -> filled from the rowid (1, 2).
        assert_eq!(rows[0][0], Value::Int(1));
        assert_eq!(rows[0][2], Value::Text("Alpha".into()));
        assert_eq!(rows[0][3], Value::Int(100));
        assert_eq!(rows[0][4], Value::Real(1.5));
        assert!(matches!(rows[0][1], Value::Blob(_))); // GPKG geometry blob
        assert_eq!(rows[1][2], Value::Text("Beta".into()));
        assert_eq!(rows[1][3], Value::Int(200));
    }
}
