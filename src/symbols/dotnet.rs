//! A minimal, read-only ECMA-335 metadata reader: just enough of a .NET assembly (PE) or a
//! portable PDB to turn them into a Bugsee mapping file ([`super::bmf`]).
//!
//! The one thing this deliberately does NOT do is resolve other assemblies. Naming a method the
//! way the Mono runtime prints it in a stack trace only needs the assembly's OWN tables (a type
//! reference is named by its namespace and name, never loaded), and the build machines that make
//! these files (Unity's `Library/Bee/artifacts`) do not have the referenced `UnityEngine.*`
//! assemblies next to them.
//!
//! Everything borrows from one `&[u8]` (a memory map, see [`super::mapped`]), nothing is copied
//! out of it except the names a caller asks for, and no accessor panics: every table is bounds
//! checked once in [`Metadata::parse`], and a stray heap or table index afterwards reads as
//! "null" (empty string, empty blob, row 0). A damaged file is an `InputInvalid` error or a
//! method that does not resolve, never a crash.

use std::borrow::Cow;
use std::collections::HashMap;

use crate::error::{Error, Result};

fn invalid<T>(what: &str) -> Result<T> {
    Err(Error::InputInvalid(format!(".NET metadata: {what}")))
}

fn u16_at(d: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_le_bytes(
        d.get(at..at.checked_add(2)?)?.try_into().ok()?,
    ))
}

fn u32_at(d: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(
        d.get(at..at.checked_add(4)?)?.try_into().ok()?,
    ))
}

/// Sequential reader over a blob (signatures, sequence points).
#[derive(Clone)]
pub struct Cursor<'a> {
    b: &'a [u8],
    pos: usize,
}

impl<'a> Cursor<'a> {
    pub fn new(b: &'a [u8]) -> Self {
        Cursor { b, pos: 0 }
    }

    pub fn at_end(&self) -> bool {
        self.pos >= self.b.len()
    }

    pub fn u8(&mut self) -> Option<u8> {
        let v = *self.b.get(self.pos)?;
        self.pos += 1;
        Some(v)
    }

    pub fn bytes(&mut self, n: usize) -> Option<&'a [u8]> {
        let s = self.b.get(self.pos..self.pos.checked_add(n)?)?;
        self.pos += n;
        Some(s)
    }

    /// ECMA-335 II.23.2 compressed unsigned integer.
    pub fn compressed_u32(&mut self) -> Option<u32> {
        let a = self.u8()? as u32;
        if a & 0x80 == 0 {
            Some(a)
        } else if a & 0xC0 == 0x80 {
            Some(((a & 0x3F) << 8) | self.u8()? as u32)
        } else if a & 0xE0 == 0xC0 {
            let (b, c, d) = (self.u8()? as u32, self.u8()? as u32, self.u8()? as u32);
            Some(((a & 0x1F) << 24) | (b << 16) | (c << 8) | d)
        } else {
            None
        }
    }

    /// ECMA-335 II.23.2 compressed signed integer (sequence-point deltas).
    pub fn compressed_i32(&mut self) -> Option<i32> {
        let first = *self.b.get(self.pos)?;
        let raw = self.compressed_u32()?;
        let sign_all_ones: i64 = if first & 0x80 == 0 {
            -64
        } else if first & 0xC0 == 0x80 {
            -8192
        } else {
            -268_435_456
        };
        // Rotate the sign bit (the LSB on the wire) back into place.
        let magnitude = (raw >> 1) as i64;
        Some(if raw & 1 == 1 {
            (magnitude + sign_all_ones) as i32
        } else {
            magnitude as i32
        })
    }

    /// A compressed length followed by that many bytes.
    fn length_prefixed(&mut self) -> Option<&'a [u8]> {
        let n = self.compressed_u32()? as usize;
        self.bytes(n)
    }
}

// --- the table schema ------------------------------------------------------------------------

#[derive(Clone, Copy)]
enum Coded {
    TypeDefOrRef,
    HasConstant,
    HasCustomAttribute,
    HasFieldMarshal,
    HasDeclSecurity,
    MemberRefParent,
    HasSemantics,
    MethodDefOrRef,
    MemberForwarded,
    Implementation,
    CustomAttributeType,
    ResolutionScope,
    TypeOrMethodDef,
}

const NONE: u8 = 0xFF;

impl Coded {
    fn tables(self) -> &'static [u8] {
        match self {
            Coded::TypeDefOrRef => &[T_TYPEDEF, T_TYPEREF, T_TYPESPEC],
            Coded::HasConstant => &[T_FIELD, T_PARAM, T_PROPERTY],
            Coded::HasCustomAttribute => &[
                T_METHODDEF,
                T_FIELD,
                T_TYPEREF,
                T_TYPEDEF,
                T_PARAM,
                9,
                T_MEMBERREF,
                0,
                14,
                T_PROPERTY,
                20,
                17,
                26,
                T_TYPESPEC,
                32,
                35,
                38,
                39,
                40,
                T_GENERICPARAM,
                44,
                43,
            ],
            Coded::HasFieldMarshal => &[T_FIELD, T_PARAM],
            Coded::HasDeclSecurity => &[T_TYPEDEF, T_METHODDEF, 32],
            Coded::MemberRefParent => &[T_TYPEDEF, T_TYPEREF, 26, T_METHODDEF, T_TYPESPEC],
            Coded::HasSemantics => &[20, T_PROPERTY],
            Coded::MethodDefOrRef => &[T_METHODDEF, T_MEMBERREF],
            Coded::MemberForwarded => &[T_FIELD, T_METHODDEF],
            Coded::Implementation => &[38, 35, 39],
            Coded::CustomAttributeType => &[NONE, NONE, T_METHODDEF, T_MEMBERREF, NONE],
            Coded::ResolutionScope => &[0, 26, 35, T_TYPEREF],
            Coded::TypeOrMethodDef => &[T_TYPEDEF, T_METHODDEF],
        }
    }

    fn tag_bits(self) -> u32 {
        match self.tables().len() {
            0..=2 => 1,
            3..=4 => 2,
            5..=8 => 3,
            9..=16 => 4,
            _ => 5,
        }
    }
}

#[derive(Clone, Copy)]
enum Col {
    U2,
    U4,
    Str,
    Guid,
    Blob,
    /// A plain index into another table.
    Idx(u8),
    Ci(Coded),
}

use Col::*;

pub const T_MODULE: u8 = 0;
pub const T_TYPEREF: u8 = 1;
pub const T_TYPEDEF: u8 = 2;
pub const T_FIELD: u8 = 4;
pub const T_METHODDEF: u8 = 6;
pub const T_PARAM: u8 = 8;
pub const T_MEMBERREF: u8 = 10;
pub const T_CUSTOMATTRIBUTE: u8 = 12;
pub const T_PROPERTYMAP: u8 = 21;
pub const T_PROPERTY: u8 = 23;
pub const T_TYPESPEC: u8 = 27;
pub const T_ASSEMBLY: u8 = 32;
pub const T_NESTEDCLASS: u8 = 41;
pub const T_GENERICPARAM: u8 = 42;
pub const T_DOCUMENT: u8 = 0x30;
pub const T_METHODDEBUGINFO: u8 = 0x31;

fn schema(table: usize) -> Option<&'static [Col]> {
    use Coded as C;
    Some(match table {
        0 => &[U2, Str, Guid, Guid, Guid],
        1 => &[Ci(C::ResolutionScope), Str, Str],
        2 => &[
            U4,
            Str,
            Str,
            Ci(C::TypeDefOrRef),
            Idx(T_FIELD),
            Idx(T_METHODDEF),
        ],
        3 => &[Idx(T_FIELD)],
        4 => &[U2, Str, Blob],
        5 => &[Idx(T_METHODDEF)],
        6 => &[U4, U2, U2, Str, Blob, Idx(T_PARAM)],
        7 => &[Idx(T_PARAM)],
        8 => &[U2, U2, Str],
        9 => &[Idx(T_TYPEDEF), Ci(C::TypeDefOrRef)],
        10 => &[Ci(C::MemberRefParent), Str, Blob],
        11 => &[U2, Ci(C::HasConstant), Blob],
        12 => &[Ci(C::HasCustomAttribute), Ci(C::CustomAttributeType), Blob],
        13 => &[Ci(C::HasFieldMarshal), Blob],
        14 => &[U2, Ci(C::HasDeclSecurity), Blob],
        15 => &[U2, U4, Idx(T_TYPEDEF)],
        16 => &[U4, Idx(T_FIELD)],
        17 => &[Blob],
        18 => &[Idx(T_TYPEDEF), Idx(19)],
        19 => &[Idx(20)],
        20 => &[U2, Str, Ci(C::TypeDefOrRef)],
        21 => &[Idx(T_TYPEDEF), Idx(T_PROPERTY)],
        22 => &[Idx(T_PROPERTY)],
        23 => &[U2, Str, Blob],
        24 => &[U2, Idx(T_METHODDEF), Ci(C::HasSemantics)],
        25 => &[Idx(T_TYPEDEF), Ci(C::MethodDefOrRef), Ci(C::MethodDefOrRef)],
        26 => &[Str],
        27 => &[Blob],
        28 => &[U2, Ci(C::MemberForwarded), Str, Idx(26)],
        29 => &[U4, Idx(T_FIELD)],
        30 => &[U4, U4],
        31 => &[U4],
        32 => &[U4, U2, U2, U2, U2, U4, Blob, Str, Str],
        33 => &[U4],
        34 => &[U4, U4, U4],
        35 => &[U2, U2, U2, U2, U4, Blob, Str, Str, Blob],
        36 => &[U4, Idx(35)],
        37 => &[U4, U4, U4, Idx(35)],
        38 => &[U4, Str, Blob],
        39 => &[U4, U4, Str, Str, Ci(C::Implementation)],
        40 => &[U4, U4, Str, Ci(C::Implementation)],
        41 => &[Idx(T_TYPEDEF), Idx(T_TYPEDEF)],
        42 => &[U2, U2, Ci(C::TypeOrMethodDef), Str],
        43 => &[Ci(C::MethodDefOrRef), Blob],
        44 => &[Idx(T_GENERICPARAM), Ci(C::TypeDefOrRef)],
        // Portable PDB.
        0x30 => &[Blob, Guid, Blob, Guid],
        0x31 => &[Idx(T_DOCUMENT), Blob],
        0x32 => &[Idx(T_METHODDEF), Idx(0x35), Idx(0x33), Idx(0x34), U4, U4],
        0x33 => &[U2, U2, Str],
        0x34 => &[Str, Blob],
        0x35 => &[Idx(0x35), Blob],
        0x36 => &[Idx(T_METHODDEF), Idx(T_METHODDEF)],
        0x37 => &[Ci(Coded::HasCustomAttribute), Guid, Blob],
        _ => return None,
    })
}

const MAX_COLS: usize = 9;

/// The heap sizes and row counts that decide every column width.
struct Layout {
    rows: [u32; 64],
    str_wide: bool,
    guid_wide: bool,
    blob_wide: bool,
}

impl Layout {
    fn width(&self, c: Col) -> usize {
        let wide = |b: bool| if b { 4 } else { 2 };
        match c {
            U2 => 2,
            U4 => 4,
            Str => wide(self.str_wide),
            Guid => wide(self.guid_wide),
            Blob => wide(self.blob_wide),
            Idx(t) => wide(self.rows[t as usize & 63] > 0xFFFF),
            Ci(k) => {
                let limit = 1u64 << (16 - k.tag_bits());
                let max = k
                    .tables()
                    .iter()
                    .filter(|&&t| t != NONE)
                    .map(|&t| self.rows[t as usize & 63] as u64)
                    .max()
                    .unwrap_or(0);
                wide(max >= limit)
            }
        }
    }
}

#[derive(Clone, Copy, Default)]
struct TableInfo {
    present: bool,
    rows: u32,
    /// Offset of row 1 inside the `#~` stream.
    start: usize,
    row_size: usize,
    col_off: [u8; MAX_COLS],
    col_width: [u8; MAX_COLS],
}

/// The decoded view of a metadata stream.
pub struct Metadata<'a> {
    tables_data: &'a [u8],
    tables: [TableInfo; 64],
    strings: &'a [u8],
    blobs: &'a [u8],
    guids: &'a [u8],
    /// The 20-byte portable-PDB id (GUID + stamp), when the `#Pdb` stream is present.
    pdb_id: Option<[u8; 20]>,
}

impl<'a> Metadata<'a> {
    /// Parse a metadata root (`BSJB`), which is either a PE image's CLI metadata or a whole
    /// portable PDB file.
    pub fn parse(root: &'a [u8]) -> Result<Metadata<'a>> {
        if root.get(..4) != Some(b"BSJB") {
            return invalid("not a metadata root (missing BSJB signature)");
        }
        let truncated = || Error::InputInvalid(".NET metadata: truncated stream directory".into());
        let version_len = u32_at(root, 12).ok_or_else(truncated)? as usize;
        let mut at = 16usize
            .checked_add(version_len)
            .ok_or_else(truncated)?
            .checked_add(2)
            .ok_or_else(truncated)?;
        let nstreams = u16_at(root, at).ok_or_else(truncated)? as usize;
        at += 2;

        let empty: &[u8] = &[];
        let (mut tables, mut strings, mut blobs, mut guids, mut pdb) =
            (empty, empty, empty, empty, empty);
        for _ in 0..nstreams {
            let offset = u32_at(root, at).ok_or_else(truncated)? as usize;
            let size = u32_at(root, at + 4).ok_or_else(truncated)? as usize;
            let name_start = at + 8;
            let name_len = root
                .get(name_start..)
                .and_then(|r| r.iter().position(|&b| b == 0))
                .ok_or_else(truncated)?;
            let name = &root[name_start..name_start + name_len];
            // The header is padded to a 4-byte boundary, counting the terminating NUL.
            at = name_start + ((name_len + 1 + 3) & !3);
            let Some(slice) = offset.checked_add(size).and_then(|e| root.get(offset..e)) else {
                return invalid("a stream lies outside the file");
            };
            match name {
                b"#~" | b"#-" => tables = slice,
                b"#Strings" => strings = slice,
                b"#Blob" => blobs = slice,
                b"#GUID" => guids = slice,
                b"#Pdb" => pdb = slice,
                _ => {}
            }
        }
        if tables.is_empty() {
            return invalid("no #~ table stream");
        }

        let pdb_id = pdb.get(..20).map(|s| {
            let mut id = [0u8; 20];
            id.copy_from_slice(s);
            id
        });

        // Table stream header: reserved, version, heap sizes, reserved, valid, sorted.
        let heap_sizes = *tables.get(6).ok_or_else(truncated)?;
        let valid = u64::from_le_bytes(
            tables
                .get(8..16)
                .ok_or_else(truncated)?
                .try_into()
                .map_err(|_| truncated())?,
        );
        if valid >> 56 != 0 {
            // Table ids 56..63 do not exist; refuse rather than guess their row size.
            return invalid("unknown table ids in the valid mask");
        }
        let mut layout = Layout {
            rows: [0; 64],
            str_wide: heap_sizes & 1 != 0,
            guid_wide: heap_sizes & 2 != 0,
            blob_wide: heap_sizes & 4 != 0,
        };
        let mut at = 24usize;
        for t in 0..64 {
            if valid >> t & 1 == 1 {
                layout.rows[t] = u32_at(tables, at).ok_or_else(truncated)?;
                at += 4;
            }
        }
        if heap_sizes & 0x40 != 0 {
            at += 4; // "extra data" word
        }

        let mut infos = [TableInfo::default(); 64];
        let mut cursor = at;
        for (t, info) in infos.iter_mut().enumerate() {
            let rows = layout.rows[t];
            if valid >> t & 1 == 0 {
                continue;
            }
            let Some(cols) = schema(t) else {
                return invalid(&format!("unsupported table 0x{t:02x}"));
            };
            let mut off = 0usize;
            info.present = true;
            info.rows = rows;
            for (i, &c) in cols.iter().enumerate() {
                let w = layout.width(c);
                info.col_off[i] = off as u8;
                info.col_width[i] = w as u8;
                off += w;
            }
            info.row_size = off;
            info.start = cursor;
            let Some(end) = (rows as usize)
                .checked_mul(off)
                .and_then(|n| cursor.checked_add(n))
            else {
                return invalid("table sizes overflow");
            };
            if end > tables.len() {
                return invalid("a table extends past the end of the table stream");
            }
            cursor = end;
        }

        Ok(Metadata {
            tables_data: tables,
            tables: infos,
            strings,
            blobs,
            guids,
            pdb_id,
        })
    }

    pub fn pdb_id(&self) -> Option<[u8; 20]> {
        self.pdb_id
    }

    pub fn rows(&self, table: u8) -> u32 {
        self.tables[table as usize & 63].rows
    }

    /// One column of one row (1-based, like every metadata index). An index outside the table
    /// reads as 0, which every caller treats as null.
    fn cell(&self, table: u8, row: u32, col: usize) -> u32 {
        let t = &self.tables[table as usize & 63];
        if row == 0 || row > t.rows || col >= MAX_COLS {
            return 0;
        }
        let at = t.start + (row as usize - 1) * t.row_size + t.col_off[col] as usize;
        match t.col_width[col] {
            2 => u16_at(self.tables_data, at).unwrap_or(0) as u32,
            4 => u32_at(self.tables_data, at).unwrap_or(0),
            _ => 0,
        }
    }

    pub fn string(&self, index: u32) -> Cow<'a, str> {
        let Some(rest) = self.strings.get(index as usize..) else {
            return Cow::Borrowed("");
        };
        let end = rest.iter().position(|&b| b == 0).unwrap_or(rest.len());
        String::from_utf8_lossy(&rest[..end])
    }

    pub fn blob(&self, index: u32) -> &'a [u8] {
        let Some(rest) = self.blobs.get(index as usize..) else {
            return &[];
        };
        let mut c = Cursor::new(rest);
        c.length_prefixed().unwrap_or(&[])
    }

    pub fn guid(&self, index: u32) -> Option<[u8; 16]> {
        let at = (index as usize).checked_sub(1)?.checked_mul(16)?;
        self.guids.get(at..at + 16)?.try_into().ok()
    }

    fn coded(&self, kind: Coded, value: u32) -> Option<(u8, u32)> {
        let bits = kind.tag_bits();
        let table = *kind.tables().get((value & ((1 << bits) - 1)) as usize)?;
        (table != NONE).then_some((table, value >> bits))
    }

    // --- module / assembly identity ----------------------------------------------------------

    /// The module version id, 32 lowercase hex digits (what the runtime prints in a trace).
    pub fn mvid(&self) -> Option<String> {
        let g = self.guid(self.cell(T_MODULE, 1, 2))?;
        let d1 = u32::from_le_bytes(g[0..4].try_into().ok()?);
        let d2 = u16::from_le_bytes(g[4..6].try_into().ok()?);
        let d3 = u16::from_le_bytes(g[6..8].try_into().ok()?);
        Some(format!(
            "{d1:08x}{d2:04x}{d3:04x}{}",
            hex::encode(&g[8..16])
        ))
    }

    /// The assembly's simple name and `major.minor.build.revision`.
    pub fn assembly_identity(&self) -> Option<(String, String)> {
        if self.rows(T_ASSEMBLY) == 0 {
            return None;
        }
        let v = |c| self.cell(T_ASSEMBLY, 1, c);
        Some((
            self.string(v(7)).into_owned(),
            format!("{}.{}.{}.{}", v(1), v(2), v(3), v(4)),
        ))
    }

    // --- type system -------------------------------------------------------------------------

    pub fn type_count(&self) -> u32 {
        self.rows(T_TYPEDEF)
    }

    pub fn type_name(&self, type_row: u32) -> Cow<'a, str> {
        self.string(self.cell(T_TYPEDEF, type_row, 1))
    }

    pub fn method_count(&self) -> u32 {
        self.rows(T_METHODDEF)
    }

    pub fn method_name(&self, method_row: u32) -> Cow<'a, str> {
        self.string(self.cell(T_METHODDEF, method_row, 3))
    }

    pub fn method_rva(&self, method_row: u32) -> u32 {
        self.cell(T_METHODDEF, method_row, 0)
    }

    /// The methods a type declares, as 1-based method rows.
    pub fn type_methods(&self, type_row: u32) -> std::ops::Range<u32> {
        let start = self.cell(T_TYPEDEF, type_row, 5);
        let end = if type_row >= self.type_count() {
            self.method_count() + 1
        } else {
            self.cell(T_TYPEDEF, type_row + 1, 5)
        };
        start..end.max(start).min(self.method_count() + 1)
    }

    /// For every type row, its enclosing type row (0 for a top-level type).
    pub fn enclosing_types(&self) -> Vec<u32> {
        let mut of = vec![0u32; self.type_count() as usize + 1];
        for r in 1..=self.rows(T_NESTEDCLASS) {
            let nested = self.cell(T_NESTEDCLASS, r, 0) as usize;
            if let Some(slot) = of.get_mut(nested) {
                *slot = self.cell(T_NESTEDCLASS, r, 1);
            }
        }
        of
    }

    /// `Namespace.Name`, nested types joined to their enclosing type with `/`: Cecil's
    /// `FullName`, which is the spelling the BMF signatures are built from.
    pub fn type_full_name(&self, type_row: u32, enclosing: &[u32]) -> String {
        let mut chain = vec![type_row];
        let mut cur = type_row;
        while let Some(&parent) = enclosing.get(cur as usize) {
            // A cycle in NestedClass would otherwise spin: a nesting depth of 64 is already absurd.
            if parent == 0 || chain.len() > 64 {
                break;
            }
            chain.push(parent);
            cur = parent;
        }
        let mut out = String::new();
        for (i, &row) in chain.iter().rev().enumerate() {
            if i == 0 {
                let ns = self.string(self.cell(T_TYPEDEF, row, 2));
                if !ns.is_empty() {
                    out.push_str(&ns);
                    out.push('.');
                }
            } else {
                out.push('/');
            }
            out.push_str(&self.type_name(row));
        }
        out
    }

    fn typeref_full_name(&self, row: u32, depth: usize) -> String {
        let name = self.string(self.cell(T_TYPEREF, row, 1));
        if let Some((T_TYPEREF, outer)) =
            self.coded(Coded::ResolutionScope, self.cell(T_TYPEREF, row, 0))
        {
            if depth < 64 {
                return format!("{}/{name}", self.typeref_full_name(outer, depth + 1));
            }
        }
        let ns = self.string(self.cell(T_TYPEREF, row, 2));
        if ns.is_empty() {
            name.into_owned()
        } else {
            format!("{ns}.{name}")
        }
    }

    /// Top-level types (`NestedClass` rows excluded), in table order.
    pub fn top_level_types<'s>(&'s self, enclosing: &'s [u32]) -> impl Iterator<Item = u32> + 's {
        (1..=self.type_count())
            .filter(move |&t| enclosing.get(t as usize).copied().unwrap_or(0) == 0)
    }

    /// The types nested directly in each type, in `NestedClass` table order (Cecil's order).
    pub fn nested_types(&self) -> HashMap<u32, Vec<u32>> {
        let mut map: HashMap<u32, Vec<u32>> = HashMap::new();
        for r in 1..=self.rows(T_NESTEDCLASS) {
            map.entry(self.cell(T_NESTEDCLASS, r, 1))
                .or_default()
                .push(self.cell(T_NESTEDCLASS, r, 0));
        }
        map
    }

    /// Every TypeDef by its attribute-blob spelling (`Ns.Outer+Inner`), for resolving the type
    /// name an attribute carries without scanning the type table once per lookup.
    pub fn type_index(&self, enclosing: &[u32]) -> HashMap<String, u32> {
        (1..=self.type_count())
            .map(|t| (self.type_full_name(t, enclosing).replace('/', "+"), t))
            .collect()
    }

    /// Generic parameter names of a type or method, in declaration order.
    pub fn generic_params(&self) -> GenericParams {
        let mut owners: HashMap<(bool, u32), Vec<(u32, String)>> = HashMap::new();
        for r in 1..=self.rows(T_GENERICPARAM) {
            let Some((table, owner)) =
                self.coded(Coded::TypeOrMethodDef, self.cell(T_GENERICPARAM, r, 2))
            else {
                continue;
            };
            owners
                .entry((table == T_METHODDEF, owner))
                .or_default()
                .push((
                    self.cell(T_GENERICPARAM, r, 0),
                    self.string(self.cell(T_GENERICPARAM, r, 3)).into_owned(),
                ));
        }
        for v in owners.values_mut() {
            v.sort_by_key(|(n, _)| *n);
        }
        GenericParams { owners }
    }

    /// Parameter names by `Sequence` (1-based) for a method; missing entries are absent.
    pub fn param_names(&self, method_row: u32) -> HashMap<u32, String> {
        let start = self.cell(T_METHODDEF, method_row, 5);
        let end = if method_row >= self.method_count() {
            self.rows(T_PARAM) + 1
        } else {
            self.cell(T_METHODDEF, method_row + 1, 5)
        };
        let mut out = HashMap::new();
        for p in start..end.max(start).min(self.rows(T_PARAM) + 1) {
            let seq = self.cell(T_PARAM, p, 1);
            if seq > 0 {
                out.insert(seq, self.string(self.cell(T_PARAM, p, 2)).into_owned());
            }
        }
        out
    }

    /// The names of a type's properties, used to spell an anonymous type.
    pub fn property_names(&self, type_row: u32) -> Vec<String> {
        for m in 1..=self.rows(T_PROPERTYMAP) {
            if self.cell(T_PROPERTYMAP, m, 0) != type_row {
                continue;
            }
            let start = self.cell(T_PROPERTYMAP, m, 1);
            let end = if m >= self.rows(T_PROPERTYMAP) {
                self.rows(T_PROPERTY) + 1
            } else {
                self.cell(T_PROPERTYMAP, m + 1, 1)
            };
            return (start..end.max(start).min(self.rows(T_PROPERTY) + 1))
                .map(|p| self.string(self.cell(T_PROPERTY, p, 1)).into_owned())
                .collect();
        }
        Vec::new()
    }

    /// `AsyncStateMachineAttribute`'s type argument for every method that has one, keyed by method
    /// row. One pass over `CustomAttribute`, so the cost does not depend on the method count.
    pub fn async_state_machines(&self, enclosing: &[u32]) -> HashMap<u32, String> {
        let mut out = HashMap::new();
        for r in 1..=self.rows(T_CUSTOMATTRIBUTE) {
            let Some((T_METHODDEF, method)) = self.coded(
                Coded::HasCustomAttribute,
                self.cell(T_CUSTOMATTRIBUTE, r, 0),
            ) else {
                continue;
            };
            let Some((ctor_table, ctor)) = self.coded(
                Coded::CustomAttributeType,
                self.cell(T_CUSTOMATTRIBUTE, r, 1),
            ) else {
                continue;
            };
            let attribute_type = match ctor_table {
                T_MEMBERREF => {
                    match self.coded(Coded::MemberRefParent, self.cell(T_MEMBERREF, ctor, 0)) {
                        Some((T_TYPEREF, t)) => self.typeref_full_name(t, 0),
                        Some((T_TYPEDEF, t)) => self.type_full_name(t, enclosing),
                        _ => continue,
                    }
                }
                T_METHODDEF => match self.type_of_method(ctor) {
                    Some(t) => self.type_full_name(t, enclosing),
                    None => continue,
                },
                _ => continue,
            };
            if attribute_type != "System.Runtime.CompilerServices.AsyncStateMachineAttribute" {
                continue;
            }
            // Prolog 0x0001, then the one fixed argument: a SerString holding the type name.
            let mut c = Cursor::new(self.blob(self.cell(T_CUSTOMATTRIBUTE, r, 2)));
            if c.bytes(2) != Some(&[1, 0]) {
                continue;
            }
            let Some(len) = c.compressed_u32() else {
                continue;
            };
            if len == u32::MAX {
                continue;
            }
            if let Some(name) = c.bytes(len as usize) {
                out.insert(method, String::from_utf8_lossy(name).into_owned());
            }
        }
        out
    }

    /// The type that declares a method.
    pub fn type_of_method(&self, method_row: u32) -> Option<u32> {
        // MethodList is non-decreasing, so the owner is the last type starting at or before it.
        let (mut lo, mut hi) = (1u32, self.type_count());
        let mut found = None;
        while lo <= hi {
            let mid = lo + (hi - lo) / 2;
            if self.cell(T_TYPEDEF, mid, 5) <= method_row {
                found = Some(mid);
                lo = mid + 1;
            } else {
                hi = mid - 1;
            }
        }
        found
    }

    // --- portable PDB ------------------------------------------------------------------------

    /// The path a `Document` row names: its separator, then each part joined by it.
    pub fn document_name(&self, row: u32) -> String {
        let mut c = Cursor::new(self.blob(self.cell(T_DOCUMENT, row, 0)));
        let Some(sep) = c.u8() else {
            return String::new();
        };
        let mut out = String::new();
        let mut first = true;
        while !c.at_end() {
            let Some(part) = c.compressed_u32() else {
                break;
            };
            if !first && sep != 0 {
                out.push(sep as char);
            }
            first = false;
            if part != 0 {
                out.push_str(&String::from_utf8_lossy(self.blob(part)));
            }
        }
        out
    }

    /// A method's sequence points, hidden ones included (the caller filters), in blob order.
    /// `None` when the blob is damaged; an empty list when the method has none.
    pub fn sequence_points(&self, method_row: u32) -> Option<Vec<SeqPoint>> {
        let blob = self.blob(self.cell(T_METHODDEBUGINFO, method_row, 1));
        if blob.is_empty() {
            return Some(Vec::new());
        }
        let mut c = Cursor::new(blob);
        let _local_signature = c.compressed_u32()?;
        let mut document = self.cell(T_METHODDEBUGINFO, method_row, 0);
        if document == 0 {
            document = c.compressed_u32()?;
        }
        let mut out = Vec::new();
        let (mut il, mut first) = (0u32, true);
        let mut start: Option<(i64, i64)> = None;
        while !c.at_end() {
            let delta = c.compressed_u32()?;
            if !first && delta == 0 {
                // A document record: the following points belong to another file.
                document = c.compressed_u32()?;
                continue;
            }
            il = if first { delta } else { il.checked_add(delta)? };
            first = false;
            let delta_lines = c.compressed_u32()?;
            let delta_columns = if delta_lines == 0 {
                c.compressed_u32()? as i64
            } else {
                c.compressed_i32()? as i64
            };
            if delta_lines == 0 && delta_columns == 0 {
                out.push(SeqPoint {
                    il_offset: il,
                    line: 0,
                    document,
                    hidden: true,
                });
                continue;
            }
            let (line, column) = match start {
                None => (c.compressed_u32()? as i64, c.compressed_u32()? as i64),
                Some((l, col)) => (
                    l.checked_add(c.compressed_i32()? as i64)?,
                    col.checked_add(c.compressed_i32()? as i64)?,
                ),
            };
            start = Some((line, column));
            out.push(SeqPoint {
                il_offset: il,
                line: u32::try_from(line).ok()?,
                document,
                hidden: false,
            });
        }
        Some(out)
    }

    // --- signatures --------------------------------------------------------------------------

    /// Decode a method's signature: its generic arity and its parameter types (the return type is
    /// parsed and discarded, a BMF signature does not carry it).
    pub fn method_signature(&self, method_row: u32) -> Option<MethodSig> {
        let mut c = Cursor::new(self.blob(self.cell(T_METHODDEF, method_row, 4)));
        let conv = c.u8()?;
        if conv & 0x10 != 0 {
            let _generic_arity = c.compressed_u32()?;
        }
        let count = c.compressed_u32()? as usize;
        // A parameter is at least one byte, so a count beyond the blob is damage, not data.
        if count > c.b.len() {
            return None;
        }
        self.read_type(&mut c, 0)?; // return type
        let mut params = Vec::with_capacity(count);
        for _ in 0..count {
            params.push(self.read_type(&mut c, 0)?);
        }
        Some(MethodSig { params })
    }

    fn type_def_or_ref(&self, token: u32, depth: usize) -> Option<Ty> {
        match self.coded(Coded::TypeDefOrRef, token)? {
            (T_TYPEDEF, row) => Some(Ty::Def(row)),
            (T_TYPEREF, row) => Some(Ty::Name(self.typeref_full_name(row, 0))),
            (T_TYPESPEC, row) => {
                let mut c = Cursor::new(self.blob(self.cell(T_TYPESPEC, row, 0)));
                self.read_type(&mut c, depth + 1)
            }
            _ => None,
        }
    }

    fn read_type(&self, c: &mut Cursor<'a>, depth: usize) -> Option<Ty> {
        // Signatures nest (arrays of generics of arrays ...); a hostile blob must not recurse
        // the stack away.
        if depth > 32 {
            return None;
        }
        let prim = |s: &str| Some(Ty::Name(s.to_string()));
        match c.u8()? {
            0x01 => prim("System.Void"),
            0x02 => prim("System.Boolean"),
            0x03 => prim("System.Char"),
            0x04 => prim("System.SByte"),
            0x05 => prim("System.Byte"),
            0x06 => prim("System.Int16"),
            0x07 => prim("System.UInt16"),
            0x08 => prim("System.Int32"),
            0x09 => prim("System.UInt32"),
            0x0A => prim("System.Int64"),
            0x0B => prim("System.UInt64"),
            0x0C => prim("System.Single"),
            0x0D => prim("System.Double"),
            0x0E => prim("System.String"),
            0x16 => prim("System.TypedReference"),
            0x18 => prim("System.IntPtr"),
            0x19 => prim("System.UIntPtr"),
            0x1C => prim("System.Object"),
            0x0F => Some(Ty::Ptr(Box::new(self.read_type(c, depth + 1)?))),
            0x10 => Some(Ty::ByRef(Box::new(self.read_type(c, depth + 1)?))),
            0x11 | 0x12 => {
                let token = c.compressed_u32()?;
                self.type_def_or_ref(token, depth)
            }
            0x13 => Some(Ty::Var(c.compressed_u32()?)),
            0x1E => Some(Ty::MVar(c.compressed_u32()?)),
            0x14 => {
                let element = self.read_type(c, depth + 1)?;
                let rank = c.compressed_u32()?;
                let nsizes = c.compressed_u32()? as usize;
                let mut sizes = Vec::new();
                for _ in 0..nsizes.min(rank as usize) {
                    sizes.push(c.compressed_u32()?);
                }
                let nlo = c.compressed_u32()? as usize;
                let mut lows = Vec::new();
                for _ in 0..nlo.min(rank as usize) {
                    lows.push(c.compressed_i32()?);
                }
                let dims = (0..rank.min(32) as usize)
                    .map(|i| {
                        let low = lows.get(i).copied();
                        let up = sizes
                            .get(i)
                            .map(|&s| low.unwrap_or(0).wrapping_add(s as i32).wrapping_sub(1));
                        (low, up)
                    })
                    .collect();
                Some(Ty::Array(Box::new(element), dims))
            }
            0x1D => Some(Ty::Array(
                Box::new(self.read_type(c, depth + 1)?),
                vec![(None, None)],
            )),
            0x15 => {
                let _kind = c.u8()?;
                let token = c.compressed_u32()?;
                let open = self.type_def_or_ref(token, depth)?;
                let n = c.compressed_u32()? as usize;
                if n > c.b.len() {
                    return None;
                }
                let mut args = Vec::with_capacity(n);
                for _ in 0..n {
                    args.push(self.read_type(c, depth + 1)?);
                }
                Some(Ty::Generic(Box::new(open), args))
            }
            0x1B => {
                let _conv = c.u8()?;
                let n = c.compressed_u32()? as usize;
                if n > c.b.len() {
                    return None;
                }
                let ret = self.read_type(c, depth + 1)?;
                let mut params = Vec::with_capacity(n);
                for _ in 0..n {
                    params.push(self.read_type(c, depth + 1)?);
                }
                Some(Ty::FnPtr(Box::new(ret), params))
            }
            0x1F | 0x20 => {
                // The modifier comes BEFORE the type it decorates in a signature.
                let required = c.b.get(c.pos.checked_sub(1)?) == Some(&0x1F);
                let token = c.compressed_u32()?;
                let modifier = self.type_def_or_ref(token, depth)?;
                let inner = self.read_type(c, depth + 1)?;
                Some(Ty::Mod(Box::new(inner), required, Box::new(modifier)))
            }
            0x45 => Some(Ty::Pinned(Box::new(self.read_type(c, depth + 1)?))),
            0x41 => self.read_type(c, depth + 1), // SENTINEL: not part of the name
            _ => None,
        }
    }
}

/// One sequence point of a portable PDB.
#[derive(Debug, Clone, PartialEq)]
pub struct SeqPoint {
    pub il_offset: u32,
    pub line: u32,
    /// A `Document` row.
    pub document: u32,
    pub hidden: bool,
}

/// A decoded type signature, kept symbolic so it can be spelled two ways (see [`Ty::full_name`]).
#[derive(Debug, Clone, PartialEq)]
pub enum Ty {
    /// A fully spelled name: a primitive or a TypeRef.
    Name(String),
    /// A TypeDef of this module, by row (spelled through the module's own nesting).
    Def(u32),
    Generic(Box<Ty>, Vec<Ty>),
    Array(Box<Ty>, Vec<(Option<i32>, Option<i32>)>),
    ByRef(Box<Ty>),
    Ptr(Box<Ty>),
    Pinned(Box<Ty>),
    /// `modreq` (true) or `modopt` (false) on a type.
    Mod(Box<Ty>, bool, Box<Ty>),
    FnPtr(Box<Ty>, Vec<Ty>),
    Var(u32),
    MVar(u32),
}

pub struct MethodSig {
    pub params: Vec<Ty>,
}

/// Generic parameter names, by owner.
pub struct GenericParams {
    owners: HashMap<(bool, u32), Vec<(u32, String)>>,
}

impl GenericParams {
    pub fn of_type(&self, type_row: u32) -> Vec<String> {
        self.names(false, type_row)
    }

    pub fn of_method(&self, method_row: u32) -> Vec<String> {
        self.names(true, method_row)
    }

    fn names(&self, method: bool, row: u32) -> Vec<String> {
        self.owners
            .get(&(method, row))
            .map(|v| v.iter().map(|(_, n)| n.clone()).collect())
            .unwrap_or_default()
    }
}

/// What a signature needs to spell its type and method generic parameters.
pub struct NameContext<'m, 'a> {
    pub md: &'m Metadata<'a>,
    pub enclosing: &'m [u32],
    pub type_params: &'m [String],
    pub method_params: &'m [String],
}

impl NameContext<'_, '_> {
    /// Cecil's `TypeReference.FullName`.
    pub fn full_name(&self, ty: &Ty) -> String {
        match ty {
            Ty::Name(n) => n.clone(),
            Ty::Def(row) => self.md.type_full_name(*row, self.enclosing),
            Ty::Generic(open, args) => format!(
                "{}<{}>",
                self.full_name(open),
                args.iter()
                    .map(|a| self.full_name(a))
                    .collect::<Vec<_>>()
                    .join(",")
            ),
            Ty::Array(inner, dims) => {
                let unsized_vector = dims.len() == 1 && dims[0] == (None, None);
                let suffix = if unsized_vector {
                    "[]".to_string()
                } else {
                    let spelled: Vec<String> = dims
                        .iter()
                        .map(|&(lo, up)| {
                            if lo.is_none() && up.is_none() {
                                String::new()
                            } else {
                                format!(
                                    "{}...{}",
                                    lo.map(|v| v.to_string()).unwrap_or_default(),
                                    up.map(|v| v.to_string()).unwrap_or_default()
                                )
                            }
                        })
                        .collect();
                    format!("[{}]", spelled.join(","))
                };
                format!("{}{suffix}", self.full_name(inner))
            }
            Ty::ByRef(inner) => format!("{}&", self.full_name(inner)),
            Ty::Ptr(inner) => format!("{}*", self.full_name(inner)),
            Ty::Pinned(inner) => format!("{} pinned", self.full_name(inner)),
            Ty::Mod(inner, required, modifier) => format!(
                "{} {}({})",
                self.full_name(inner),
                if *required { "modreq" } else { "modopt" },
                self.full_name(modifier)
            ),
            Ty::FnPtr(ret, params) => format!(
                "method {} *({})",
                self.full_name(ret),
                params
                    .iter()
                    .map(|p| self.full_name(p))
                    .collect::<Vec<_>>()
                    .join(",")
            ),
            Ty::Var(n) => self
                .type_params
                .get(*n as usize)
                .cloned()
                .unwrap_or_else(|| format!("!{n}")),
            Ty::MVar(n) => self
                .method_params
                .get(*n as usize)
                .cloned()
                .unwrap_or_else(|| format!("!!{n}")),
        }
    }

    /// The spelling `BaseParser.GetTypeFullName` uses: a generic INSTANCE is reduced to its open
    /// type (`List<int>` -> `List`1`), anything else is the full name.
    pub fn reduced_name(&self, ty: &Ty) -> String {
        match ty {
            Ty::Generic(open, _) => self.full_name(open),
            other => self.full_name(other),
        }
    }
}

// --- the PE container ------------------------------------------------------------------------

struct Section {
    virtual_address: u32,
    raw_size: u32,
    raw_pointer: u32,
}

/// A `.dll` as the converter needs it: the CLI metadata, method body sizes, and the CodeView
/// entry that names its portable PDB.
pub struct Assembly<'a> {
    data: &'a [u8],
    sections: Vec<Section>,
    pub metadata: Metadata<'a>,
    pub codeview: Option<CodeView>,
    pub has_embedded_pdb: bool,
}

/// A CodeView debug-directory entry: the pair `(guid, stamp)` is the portable-PDB id.
#[derive(Debug, Clone, PartialEq)]
pub struct CodeView {
    pub guid: [u8; 16],
    pub stamp: u32,
    pub path: String,
}

impl CodeView {
    pub fn pdb_id(&self) -> [u8; 20] {
        let mut id = [0u8; 20];
        id[..16].copy_from_slice(&self.guid);
        id[16..].copy_from_slice(&self.stamp.to_le_bytes());
        id
    }
}

/// What [`Assembly::parse`] says about a file that is not a managed assembly at all (a native
/// `.dll`, or not a PE file), so a directory scan can skip those instead of failing on them.
const NOT_PE: &str = "not a PE file";
const NO_CLI_HEADER: &str = "no CLI header, this is not a .NET assembly";

/// Whether `err` (from [`Assembly::parse`]) means "this file is not a .NET assembly", as opposed
/// to a damaged one.
pub fn is_not_an_assembly(err: &Error) -> bool {
    matches!(err, Error::InputInvalid(m) if m.ends_with(NOT_PE) || m.ends_with(NO_CLI_HEADER))
}

impl<'a> Assembly<'a> {
    pub fn parse(data: &'a [u8]) -> Result<Assembly<'a>> {
        let short = || Error::InputInvalid("PE: truncated header".into());
        if data.get(..2) != Some(b"MZ") {
            return invalid(NOT_PE);
        }
        let pe = u32_at(data, 0x3c).ok_or_else(short)? as usize;
        // A header that runs off the end of the file is a truncated assembly, not a non-assembly.
        if data.get(pe..pe.saturating_add(4)).ok_or_else(short)? != b"PE\0\0" {
            return invalid(NOT_PE);
        }
        let nsections = u16_at(data, pe + 6).ok_or_else(short)? as usize;
        let opt_size = u16_at(data, pe + 20).ok_or_else(short)? as usize;
        let opt = pe + 24;
        let dirs = match u16_at(data, opt).ok_or_else(short)? {
            0x10b => opt + 96,
            0x20b => opt + 112,
            _ => return invalid("unknown optional-header magic"),
        };
        let ndirs = u32_at(data, dirs - 4).ok_or_else(short)? as usize;
        let dir = |i: usize| -> Option<(u32, u32)> {
            (i < ndirs.min(16)).then_some(())?;
            Some((u32_at(data, dirs + 8 * i)?, u32_at(data, dirs + 8 * i + 4)?))
        };

        let mut sections = Vec::new();
        let table = opt + opt_size;
        for i in 0..nsections.min(96) {
            let at = table + 40 * i;
            sections.push(Section {
                virtual_address: u32_at(data, at + 12).ok_or_else(short)?,
                raw_size: u32_at(data, at + 16).ok_or_else(short)?,
                raw_pointer: u32_at(data, at + 20).ok_or_else(short)?,
            });
        }
        let mut asm = Assembly {
            data,
            sections,
            metadata: Metadata {
                tables_data: &[],
                tables: [TableInfo::default(); 64],
                strings: &[],
                blobs: &[],
                guids: &[],
                pdb_id: None,
            },
            codeview: None,
            has_embedded_pdb: false,
        };

        let (cli_rva, _) = dir(14)
            .filter(|&(r, _)| r != 0)
            .ok_or_else(|| Error::InputInvalid(format!(".NET metadata: {NO_CLI_HEADER}")))?;
        let cli = asm.offset(cli_rva).ok_or_else(short)?;
        let md_rva = u32_at(data, cli + 8).ok_or_else(short)?;
        let md_size = u32_at(data, cli + 12).ok_or_else(short)? as usize;
        let md_off = asm
            .offset(md_rva)
            .ok_or_else(|| Error::InputInvalid("PE: metadata outside any section".into()))?;
        let root = md_off
            .checked_add(md_size)
            .and_then(|e| data.get(md_off..e))
            .ok_or_else(|| Error::InputInvalid("PE: metadata extends past the file".into()))?;
        asm.metadata = Metadata::parse(root)?;

        if let Some((rva, size)) = dir(6).filter(|&(r, s)| r != 0 && s != 0) {
            if let Some(at) = asm.offset(rva) {
                for e in 0..(size as usize / 28).min(16) {
                    let entry = at + 28 * e;
                    let (Some(stamp), Some(kind), Some(len), Some(raw)) = (
                        u32_at(data, entry + 4),
                        u32_at(data, entry + 12),
                        u32_at(data, entry + 16),
                        u32_at(data, entry + 24),
                    ) else {
                        break;
                    };
                    match kind {
                        2 => {
                            let body = (raw as usize)
                                .checked_add(len as usize)
                                .and_then(|e| data.get(raw as usize..e));
                            if let Some(b) = body.filter(|b| b.len() >= 24 && &b[..4] == b"RSDS") {
                                let end =
                                    b[24..].iter().position(|&c| c == 0).unwrap_or(b.len() - 24);
                                asm.codeview = Some(CodeView {
                                    guid: b[4..20].try_into().unwrap_or([0; 16]),
                                    stamp,
                                    path: String::from_utf8_lossy(&b[24..24 + end]).into_owned(),
                                });
                            }
                        }
                        17 => asm.has_embedded_pdb = true,
                        _ => {}
                    }
                }
            }
        }
        Ok(asm)
    }

    fn offset(&self, rva: u32) -> Option<usize> {
        self.sections.iter().find_map(|s| {
            let within = rva.checked_sub(s.virtual_address)?;
            (within < s.raw_size).then(|| s.raw_pointer as usize + within as usize)
        })
    }

    /// The IL size of a method body, from its tiny or fat header; `None` for a method without a
    /// body (abstract, extern) or a header that does not parse.
    pub fn il_size(&self, method_row: u32) -> Option<u32> {
        let rva = self.metadata.method_rva(method_row);
        if rva == 0 {
            return None;
        }
        let at = self.offset(rva)?;
        let first = *self.data.get(at)?;
        match first & 3 {
            2 => Some((first >> 2) as u32),
            3 => u32_at(self.data, at + 4),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compressed_integers_follow_the_spec_examples() {
        // ECMA-335 II.23.2.
        let u = |b: &[u8]| Cursor::new(b).compressed_u32();
        assert_eq!(u(&[0x03]), Some(3));
        assert_eq!(u(&[0x7F]), Some(0x7F));
        assert_eq!(u(&[0x80, 0x80]), Some(0x80));
        assert_eq!(u(&[0xAE, 0x57]), Some(0x2E57));
        assert_eq!(u(&[0xBF, 0xFF]), Some(0x3FFF));
        assert_eq!(u(&[0xC0, 0x00, 0x40, 0x00]), Some(0x4000));
        assert_eq!(u(&[0xDF, 0xFF, 0xFF, 0xFF]), Some(0x1FFF_FFFF));
        assert_eq!(u(&[0xE0, 0, 0, 0]), None);
        assert_eq!(u(&[0x80]), None);
        let i = |b: &[u8]| Cursor::new(b).compressed_i32();
        assert_eq!(i(&[0x06]), Some(3));
        assert_eq!(i(&[0x7B]), Some(-3));
        assert_eq!(i(&[0x80, 0x80]), Some(64));
        assert_eq!(i(&[0x01]), Some(-64));
        assert_eq!(i(&[0xC0, 0x00, 0x40, 0x00]), Some(8192));
        assert_eq!(i(&[0x80, 0x01]), Some(-8192));
    }

    #[test]
    fn garbage_is_an_error_never_a_panic() {
        for input in [
            &b""[..],
            b"MZ",
            b"BSJB",
            b"not a dll at all, just text",
            &[0u8; 512],
            &[0xFFu8; 512],
        ] {
            assert!(Assembly::parse(input).is_err());
            assert!(Metadata::parse(input).is_err());
        }
    }
}
