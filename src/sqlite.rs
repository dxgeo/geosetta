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
    root_page: u32,
    /// Index of the `INTEGER PRIMARY KEY` column (a rowid alias), if any: its
    /// stored value is NULL and the real value is the rowid.
    rowid_alias: Option<usize>,
}

impl Table {
    /// The index of the `INTEGER PRIMARY KEY` (rowid-alias) column, if any.
    pub fn rowid_alias(&self) -> Option<usize> {
        self.rowid_alias
    }
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

    /// All ordinary tables (`type = 'table'`) with a real root page.
    pub fn tables(&self) -> Result<Vec<Table>> {
        // sqlite_master is a table b-tree rooted at page 1. Its columns are
        // (type, name, tbl_name, rootpage, sql).
        let master = Table {
            name: "sqlite_master".into(),
            columns: vec!["type".into(), "name".into(), "tbl_name".into(), "rootpage".into(), "sql".into()],
            root_page: 1,
            rowid_alias: None,
        };
        let mut tables = Vec::new();
        for row in self.read_rows(&master)? {
            let kind = as_text(&row, 0);
            let root = as_int(&row, 3);
            let sql = as_text(&row, 4);
            if kind.as_deref() == Some("table")
                && let (Some(name), Some(root), Some(sql)) = (as_text(&row, 1), root, sql)
                && root > 0
            {
                let (columns, rowid_alias) = parse_columns(&sql);
                tables.push(Table {
                    name,
                    columns,
                    root_page: root as u32,
                    rowid_alias,
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
            values.resize(ncols, Value::Null); // pad/truncate to the column count
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

    /// Decode a leaf table cell at `off`, assembling any overflow payload.
    fn leaf_cell(&self, page: &[u8], off: usize) -> Result<(i64, Vec<u8>)> {
        let (payload_len, n1) = varint(page, off)?;
        let (rowid, n2) = varint(page, off + n1)?;
        let payload_len = payload_len as usize;
        let content = off + n1 + n2;

        // How much of the payload lives on this page.
        let u = self.usable_size;
        let max_local = u - 35;
        let local = if payload_len <= max_local {
            payload_len
        } else {
            let min_local = ((u - 12) * 32 / 255) - 23;
            let k = min_local + (payload_len - min_local) % (u - 4);
            if k <= max_local { k } else { min_local }
        };

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

/// Extract column names (in order) and the rowid-alias column index from a
/// `CREATE TABLE` statement.
fn parse_columns(sql: &str) -> (Vec<String>, Option<usize>) {
    let Some(open) = sql.find('(') else {
        return (Vec::new(), None);
    };
    let Some(close) = sql.rfind(')') else {
        return (Vec::new(), None);
    };
    let body = &sql[open + 1..close];

    let mut names = Vec::new();
    let mut rowid_alias = None;
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
            rowid_alias = Some(names.len());
        }
        names.push(name);
    }
    (names, rowid_alias)
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

/// A table to write: its DDL (`CREATE TABLE …`, recorded in `sqlite_master`)
/// and its rows as `(rowid, values)`, rowid-ordered. A value at the
/// INTEGER PRIMARY KEY column should be `Null` (the rowid carries it).
pub struct TableSpec {
    pub name: String,
    pub sql: String,
    pub rows: Vec<(i64, Vec<Value>)>,
}

/// A schema-only `sqlite_master` entry with no b-tree of its own (`rootpage`
/// 0): a virtual table (`CREATE VIRTUAL TABLE …`) or a trigger. Used by the
/// GeoPackage R*Tree extension, whose virtual table is backed by ordinary
/// shadow [`TableSpec`]s and whose triggers are pure schema.
pub struct MasterEntry {
    /// `sqlite_master.type` — `"table"` for a virtual table, `"trigger"`.
    pub kind: String,
    pub name: String,
    /// The table this entry is attached to (its own name for a virtual table,
    /// the triggering table for a trigger).
    pub tbl_name: String,
    pub sql: String,
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

    // Build each user table; collect a sqlite_master row per table.
    let mut master_rows: Vec<(i64, Vec<Value>)> = Vec::new();
    for (i, t) in tables.iter().enumerate() {
        let cells: Vec<(i64, Vec<u8>)> = t
            .rows
            .iter()
            .map(|(rowid, values)| (*rowid, w.leaf_cell(*rowid, &encode_record(values))))
            .collect();
        let root = w.build_btree(&cells)?;
        master_rows.push((
            (i + 1) as i64,
            vec![
                Value::Text("table".into()),
                Value::Text(t.name.clone()),
                Value::Text(t.name.clone()),
                Value::Int(root as i64),
                Value::Text(t.sql.clone()),
            ],
        ));
    }

    // Schema-only entries follow the tables, keeping rowid order. Their
    // `rootpage` is 0 — they own no b-tree (a virtual table lives in its shadow
    // tables; a trigger is pure schema).
    for (j, e) in extra_master.iter().enumerate() {
        master_rows.push((
            (tables.len() + j + 1) as i64,
            vec![
                Value::Text(e.kind.clone()),
                Value::Text(e.name.clone()),
                Value::Text(e.tbl_name.clone()),
                Value::Int(0),
                Value::Text(e.sql.clone()),
            ],
        ));
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
        let (cols, alias) = parse_columns(sql);
        assert_eq!(cols, vec!["fid", "geom", "name", "pop", "score"]);
        assert_eq!(alias, Some(0));
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
