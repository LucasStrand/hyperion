// Pure-Rust `.bos` parser — a faithful port of `bos_explore.py`.
//
// A `.bos` file is a .NET BinaryFormatter (MS-NRBF) serialized object graph.
// `bos_explore.py` parses that graph with the `nrbf` Python library (with the
// library's deep cyclic resolve DISABLED — `p._resolve = lambda v: v`), so it
// works directly on the raw object table:
//
//   * every object id maps to a value (dict / list / str / int / float / bool / null);
//   * a .NET class instance is a dict carrying `__class__` (its type name) plus
//     its members; member values that are object references are the marker dict
//     `{"__ref__": id}`; `System.Collections.Generic.List<T>` and enums are NOT
//     unwrapped (the Explorer unwraps them by hand);
//   * strings and arrays are also registered in the object table by id.
//
// This module reimplements (1) a minimal MS-NRBF reader that reproduces that
// exact object table as `serde_json::Value`s, and (2) the Explorer logic
// (`deref` / `as_list` / `find_refs` / `command` / `program_detail` /
// `summarize` / `richness`) that turns it into the same `Vec<Value>` node array
// `bos_explore.py` emits. The output JSON schema is identical — the frontend
// depends on it.
//
// IMPORTANT: this relies on `serde_json`'s `preserve_order` feature so object
// member order matches Python dict insertion order (binding/ref ordering, and
// the `settings`/`values` key order, depend on it).

use std::collections::{HashMap, HashSet};

use serde_json::{json, Map, Value};

const NAME: &str = "NodeSettingsInfo+<Name>k__BackingField";
const VAL: &str = "<Value>k__BackingField";
const VNAME: &str = "NodeValueInfo+<Name>k__BackingField";
const NODEHOST: &str = "BOSCommon.Node.Common.NodeHost";

/// A shared `null` so `deref` can return a borrowed value when a reference is
/// dangling (mirrors Python's `dict.get` returning `None`).
static NULL: Value = Value::Null;

// ComfortClick.Tasks.TaskCommands.If+ConditionTypes (0 = Equals is confirmed).
fn if_cond(v: Option<i64>) -> &'static str {
    match v {
        Some(0) => "=",
        Some(1) => "!=",
        Some(2) => ">",
        Some(3) => ">=",
        Some(4) => "<",
        Some(5) => "<=",
        _ => "?",
    }
}

// IfTrigger+ConditionTypes (6 = on change is confirmed).
fn trig_cond(v: Option<i64>) -> &'static str {
    match v {
        Some(0) => "=",
        Some(1) => "!=",
        Some(2) => ">",
        Some(3) => ">=",
        Some(4) => "<",
        Some(5) => "<=",
        Some(6) => "on change",
        _ => "?",
    }
}

// =====================================================================
// MS-NRBF reader
// =====================================================================

#[derive(Clone)]
struct ClassDef {
    name: String,
    member_names: Vec<String>,
    binary_types: Vec<u8>,
    additional: Vec<Additional>,
}

#[derive(Clone)]
enum Additional {
    Primitive(u8),
    None,
}

struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
    objects: HashMap<i32, Value>,
    order: Vec<i32>,
    class_defs: HashMap<i32, ClassDef>,
}

type R<T> = Result<T, String>;

impl<'a> Reader<'a> {
    fn new(buf: &'a [u8]) -> Self {
        Reader {
            buf,
            pos: 0,
            objects: HashMap::new(),
            order: Vec::new(),
            class_defs: HashMap::new(),
        }
    }

    fn put(&mut self, id: i32, v: Value) {
        if self.objects.insert(id, v).is_none() {
            self.order.push(id);
        }
    }

    // ---- byte-level readers ----
    fn take(&mut self, n: usize) -> R<&'a [u8]> {
        let end = self
            .pos
            .checked_add(n)
            .ok_or_else(|| "length overflow".to_string())?;
        if end > self.buf.len() {
            return Err("unexpected end of stream".into());
        }
        let s = &self.buf[self.pos..end];
        self.pos = end;
        Ok(s)
    }
    fn u8(&mut self) -> R<u8> {
        Ok(self.take(1)?[0])
    }
    fn i8(&mut self) -> R<i8> {
        Ok(self.u8()? as i8)
    }
    fn u16(&mut self) -> R<u16> {
        let b = self.take(2)?;
        Ok(u16::from_le_bytes([b[0], b[1]]))
    }
    fn i16(&mut self) -> R<i16> {
        Ok(self.u16()? as i16)
    }
    fn i32(&mut self) -> R<i32> {
        let b = self.take(4)?;
        Ok(i32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }
    fn u32(&mut self) -> R<u32> {
        Ok(self.i32()? as u32)
    }
    fn i64(&mut self) -> R<i64> {
        let b = self.take(8)?;
        Ok(i64::from_le_bytes([
            b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7],
        ]))
    }
    fn u64(&mut self) -> R<u64> {
        Ok(self.i64()? as u64)
    }
    fn f32(&mut self) -> R<f32> {
        let b = self.take(4)?;
        Ok(f32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }
    fn f64(&mut self) -> R<f64> {
        let b = self.take(8)?;
        Ok(f64::from_le_bytes([
            b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7],
        ]))
    }

    /// .NET length-prefixed string: 7-bit-encoded length then UTF-8 bytes.
    fn lps(&mut self) -> R<String> {
        let mut length: usize = 0;
        let mut shift: u32 = 0;
        loop {
            let b = self.u8()?;
            length |= ((b & 0x7F) as usize) << shift;
            if b & 0x80 == 0 {
                break;
            }
            shift += 7;
            // A .NET 7-bit length prefix is at most 5 bytes (35 bits). After the
            // 5th continuation byte `shift` is 35; a 6th byte would shift by 35,
            // which panics on a 32-bit `usize`. Reject once `shift` passes 28 so
            // a valid 5-byte length (whose final byte has no continuation bit and
            // breaks before this check) is still accepted, but a 6th byte cannot.
            if shift > 28 {
                return Err("string length varint too long".into());
            }
        }
        let bytes = self.take(length)?;
        // Strings that matter (node names/paths/types) are valid UTF-8; lossy
        // keeps us from panicking on an odd byte rather than matching Python's
        // strict decode (which would raise — and we'd then fall back to Python).
        Ok(String::from_utf8_lossy(bytes).into_owned())
    }

    /// A float that is integer-valued must still serialize as e.g. `5.0` (as
    /// Python/`json` does); `serde_json::Number::from_f64` + the `preserve_order`
    /// build give that. Non-finite floats have no JSON form — emit null (a
    /// divergence from Python's `NaN`/`Infinity`, but these never appear here).
    fn num_f64(x: f64) -> Value {
        serde_json::Number::from_f64(x).map_or(Value::Null, Value::Number)
    }

    fn primitive(&mut self, ptype: u8) -> R<Value> {
        Ok(match ptype {
            1 => Value::Bool(self.u8()? != 0),       // Boolean
            2 => json!(self.u8()?),                  // Byte
            3 => Value::String(self.read_char()?),   // Char
            5 => Value::String(self.lps()?),         // Decimal
            6 => Self::num_f64(self.f64()?),         // Double
            7 => json!(self.i16()?),                 // Int16
            8 => json!(self.i32()?),                 // Int32
            9 => json!(self.i64()?),                 // Int64
            10 => json!(self.i8()?),                 // SByte
            11 => Self::num_f64(self.f32()? as f64), // Single
            12 => json!(self.i64()?),                // TimeSpan
            13 => json!(self.u64()?),                // DateTime
            14 => json!(self.u16()?),                // UInt16
            15 => json!(self.u32()?),                // UInt32
            16 => json!(self.u64()?),                // UInt64
            other => return Err(format!("unhandled primitive type {other}")),
        })
    }

    /// BinaryFormatter stores Char as UTF-8 bytes.
    fn read_char(&mut self) -> R<String> {
        let b0 = self.u8()?;
        let n = if b0 < 0x80 {
            0
        } else if b0 < 0xE0 {
            1
        } else if b0 < 0xF0 {
            2
        } else {
            3
        };
        let mut bytes = vec![b0];
        for _ in 0..n {
            bytes.push(self.u8()?);
        }
        Ok(String::from_utf8_lossy(&bytes).into_owned())
    }

    fn parse(&mut self) -> R<()> {
        let first = self.u8()?;
        if first != 0 {
            return Err(format!(
                "stream does not start with a SerializationHeaderRecord (got 0x{first:02x})"
            ));
        }
        let _root_id = self.i32()?;
        self.i32()?; // headerId
        self.i32()?; // majorVersion
        self.i32()?; // minorVersion

        loop {
            if self.pos >= self.buf.len() {
                break;
            }
            let rec = self.u8()?;
            match rec {
                11 => break,                       // MessageEnd
                12 => self.read_binary_library()?, // BinaryLibrary
                5 => {
                    self.read_class_with_members_and_types(false)?;
                }
                4 => {
                    self.read_class_with_members_and_types(true)?;
                } // System variant: no trailing libraryId
                1 => {
                    self.read_class_with_id()?;
                }
                6 => {
                    self.read_binary_object_string_body()?;
                }
                15 => {
                    self.read_array_single_primitive()?;
                }
                16 => {
                    self.read_array_single_object()?;
                }
                17 => {
                    self.read_array_single_string()?;
                }
                7 => {
                    self.read_binary_array()?;
                }
                other => {
                    return Err(format!(
                        "unexpected top-level record {other} at offset {}",
                        self.pos - 1
                    ))
                }
            }
        }
        Ok(())
    }

    fn read_binary_library(&mut self) -> R<()> {
        let _lib_id = self.i32()?;
        let _name = self.lps()?;
        Ok(())
    }

    fn read_class_info(&mut self) -> R<(i32, String, Vec<String>)> {
        let obj_id = self.i32()?;
        let name = self.lps()?;
        let count = self.i32()?;
        if count < 0 {
            return Err("negative member count".into());
        }
        // Bound the preallocation by the bytes left in the buffer: an attacker
        // can set `count` to i32::MAX (~48 GB reservation → process abort). Each
        // member name is at least one byte, so the buffer is a hard ceiling; the
        // bounds-checked read loop below errors fast on truncation.
        let cap = (count as usize).min(self.buf.len().saturating_sub(self.pos));
        let mut member_names = Vec::with_capacity(cap);
        for _ in 0..count {
            member_names.push(self.lps()?);
        }
        Ok((obj_id, name, member_names))
    }

    fn read_member_type_info(&mut self, count: usize) -> R<(Vec<u8>, Vec<Additional>)> {
        // Bound the preallocations by the remaining buffer (each binary type is at
        // least one byte) so a huge member count can't drive a giant reservation.
        let cap = count.min(self.buf.len().saturating_sub(self.pos));
        let mut binary_types = Vec::with_capacity(cap);
        for _ in 0..count {
            binary_types.push(self.u8()?);
        }
        let mut additional = Vec::with_capacity(cap);
        for &bt in &binary_types {
            match bt {
                0 | 7 => additional.push(Additional::Primitive(self.u8()?)), // Primitive | PrimitiveArray
                3 => {
                    self.lps()?; // SystemClass: class name
                    additional.push(Additional::None);
                }
                4 => {
                    self.lps()?; // Class: class name
                    self.i32()?; // library id
                    additional.push(Additional::None);
                }
                _ => additional.push(Additional::None),
            }
        }
        Ok((binary_types, additional))
    }

    fn read_class_with_members_and_types(&mut self, system: bool) -> R<i32> {
        let (obj_id, name, member_names) = self.read_class_info()?;
        let (binary_types, additional) = self.read_member_type_info(member_names.len())?;
        if !system {
            self.i32()?; // libraryId (absent on SystemClassWithMembersAndTypes)
        }
        self.class_defs.insert(
            obj_id,
            ClassDef {
                name: name.clone(),
                member_names: member_names.clone(),
                binary_types: binary_types.clone(),
                additional: additional.clone(),
            },
        );
        let obj = self.read_object(&name, &member_names, &binary_types, &additional)?;
        self.put(obj_id, obj);
        Ok(obj_id)
    }

    fn read_class_with_id(&mut self) -> R<i32> {
        let obj_id = self.i32()?;
        let metadata_id = self.i32()?;
        let def = self
            .class_defs
            .get(&metadata_id)
            .ok_or_else(|| format!("ClassWithId references unknown metadata id {metadata_id}"))?
            .clone();
        let obj = self.read_object(
            &def.name,
            &def.member_names,
            &def.binary_types,
            &def.additional,
        )?;
        self.put(obj_id, obj);
        Ok(obj_id)
    }

    fn read_object(
        &mut self,
        name: &str,
        member_names: &[String],
        binary_types: &[u8],
        additional: &[Additional],
    ) -> R<Value> {
        let mut map = Map::new();
        for i in 0..member_names.len() {
            let bt = binary_types[i];
            let v = if bt == 0 {
                // Primitive
                match &additional[i] {
                    Additional::Primitive(pt) => self.primitive(*pt)?,
                    Additional::None => return Err("primitive member missing type".into()),
                }
            } else {
                self.read_inline_value()?
            };
            map.insert(member_names[i].clone(), v);
        }
        map.insert("__class__".to_string(), Value::String(name.to_string()));
        Ok(Value::Object(map))
    }

    fn read_inline_value(&mut self) -> R<Value> {
        // Loop (not recurse) so an arbitrarily long chain of inline BinaryLibrary
        // (type 12) records — each of which merely precedes the value it annotates
        // — cannot overflow the stack (Rust stack overflow is a non-unwinding
        // abort). Every other record type returns from the loop on its first pass,
        // preserving the original behavior exactly.
        loop {
            if self.pos >= self.buf.len() {
                return Ok(Value::Null);
            }
            let pos = self.pos;
            let rec = self.u8()?;
            return Ok(match rec {
                6 => Value::String(self.read_binary_object_string_body()?), // BinaryObjectString
                9 => json!({ "__ref__": self.i32()? }),                     // MemberReference
                10 => Value::Null,                                          // ObjectNull
                5 => json!({ "__ref__": self.read_class_with_members_and_types(false)? }),
                4 => json!({ "__ref__": self.read_class_with_members_and_types(true)? }),
                1 => json!({ "__ref__": self.read_class_with_id()? }),
                15 => self.read_array_single_primitive()?,
                16 => self.read_array_single_object()?,
                17 => self.read_array_single_string()?,
                7 => self.read_binary_array()?,
                8 => {
                    // MemberPrimitiveTyped
                    let pt = self.u8()?;
                    self.primitive(pt)?
                }
                12 => {
                    // BinaryLibrary may appear inline before the value it precedes.
                    // Consume it and read the next record in-place (no recursion).
                    self.read_binary_library()?;
                    continue;
                }
                other => {
                    return Err(format!(
                        "unexpected inline record type {other} at offset {pos}"
                    ))
                }
            });
        }
    }

    fn read_binary_object_string_body(&mut self) -> R<String> {
        let obj_id = self.i32()?;
        let s = self.lps()?;
        self.put(obj_id, Value::String(s.clone()));
        Ok(s)
    }

    fn read_array_single_primitive(&mut self) -> R<Value> {
        let obj_id = self.i32()?;
        let length = self.i32()?;
        let ptype = self.u8()?;
        let mut arr = Vec::new();
        for _ in 0..length.max(0) {
            arr.push(self.primitive(ptype)?);
        }
        let v = Value::Array(arr);
        self.put(obj_id, v.clone());
        Ok(v)
    }

    fn read_array_single_string(&mut self) -> R<Value> {
        let obj_id = self.i32()?;
        let length = self.i32()?;
        let arr = self.read_array_elements(length)?;
        let v = Value::Array(arr);
        self.put(obj_id, v.clone());
        Ok(v)
    }

    fn read_array_single_object(&mut self) -> R<Value> {
        let obj_id = self.i32()?;
        let length = self.i32()?;
        let arr = self.read_array_elements(length)?;
        let v = Value::Array(arr);
        self.put(obj_id, v.clone());
        Ok(v)
    }

    fn read_array_elements(&mut self, length: i32) -> R<Vec<Value>> {
        let mut arr = Vec::new();
        let mut i = 0i64;
        // Bound the declared element count by the bytes left in the buffer. Every
        // array element is described by at least one byte in the stream (a record
        // tag, or the multi-null filler records below), so a valid array can never
        // declare more elements than there are remaining bytes; capping here stops
        // an attacker-supplied i32::MAX length (e.g. via ArraySingleObject) from
        // driving the null-fill loop into a multi-GB allocation / process abort.
        let remaining = self.buf.len().saturating_sub(self.pos) as i64;
        let length = (length.max(0) as i64).min(remaining);
        while i < length {
            if self.pos >= self.buf.len() {
                break;
            }
            let pos = self.pos;
            let rec = self.u8()?;
            match rec {
                10 => {
                    arr.push(Value::Null);
                    i += 1;
                }
                13 => {
                    let count = self.u8()? as i64; // ObjectNullMultiple256
                    for _ in 0..count {
                        arr.push(Value::Null);
                    }
                    i += count;
                }
                14 => {
                    let count = self.i32()? as i64; // ObjectNullMultiple
                    if count < 0 {
                        return Err("negative ObjectNullMultiple count".into());
                    }
                    // Clamp to the array's remaining element budget so a crafted
                    // i32::MAX count can't push ~2B nulls (~48 GB → abort). A valid
                    // file never declares more nulls than the array has slots left,
                    // so this is a no-op on correct input.
                    let count = count.min(length - i);
                    for _ in 0..count {
                        arr.push(Value::Null);
                    }
                    i += count;
                }
                _ => {
                    self.pos = pos; // rewind; let read_inline_value consume the record
                    arr.push(self.read_inline_value()?);
                    i += 1;
                }
            }
        }
        Ok(arr)
    }

    fn read_binary_array(&mut self) -> R<Value> {
        let obj_id = self.i32()?;
        let array_type = self.u8()?;
        let rank = self.i32()?;
        if rank < 0 {
            return Err("negative array rank".into());
        }
        // Bound the preallocation by the remaining buffer: each dimension length is
        // a 4-byte i32, so the buffer caps how many a valid record can carry; an
        // attacker-supplied i32::MAX rank can't force a multi-GB reservation.
        let cap = (rank as usize).min(self.buf.len().saturating_sub(self.pos));
        let mut lengths = Vec::with_capacity(cap);
        for _ in 0..rank {
            lengths.push(self.i32()?);
        }
        if matches!(array_type, 3..=5) {
            for _ in 0..rank {
                self.i32()?; // lower bounds
            }
        }
        let bt = self.u8()?;
        let additional: Option<u8> = match bt {
            0 | 7 => Some(self.u8()?),
            3 => {
                self.lps()?;
                None
            }
            4 => {
                self.lps()?;
                self.i32()?;
                None
            }
            _ => None,
        };
        let mut total: i64 = 1;
        for &d in &lengths {
            total = total.saturating_mul(d.max(0) as i64);
        }
        let arr = if bt == 0 {
            let pt = additional.ok_or_else(|| "primitive array missing type".to_string())?;
            let mut a = Vec::new();
            for _ in 0..total {
                a.push(self.primitive(pt)?);
            }
            a
        } else {
            let total_i32 = if total > i32::MAX as i64 {
                i32::MAX
            } else {
                total as i32
            };
            self.read_array_elements(total_i32)?
        };
        let v = Value::Array(arr);
        self.put(obj_id, v.clone());
        Ok(v)
    }
}

// =====================================================================
// Explorer — port of bos_explore.py's Explorer + main()
// =====================================================================

struct Explorer {
    objects: HashMap<i32, Value>,
    order: Vec<i32>,
}

/// If `v` is exactly the reference marker `{"__ref__": id}`, return the id.
fn as_ref_id(v: &Value) -> Option<i32> {
    if let Value::Object(m) = v {
        if m.len() == 1 {
            if let Some(r) = m.get("__ref__") {
                return r.as_i64().map(|x| x as i32);
            }
        }
    }
    None
}

/// Python truthiness for a JSON value.
fn truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::String(s) => !s.is_empty(),
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                i != 0
            } else if let Some(u) = n.as_u64() {
                u != 0
            } else {
                n.as_f64().map(|f| f != 0.0).unwrap_or(false)
            }
        }
        Value::Array(a) => !a.is_empty(),
        Value::Object(m) => !m.is_empty(),
    }
}

/// Python `str(v)` for the scalar values that reach an f-string here.
fn py_str(v: &Value) -> String {
    match v {
        Value::Null => "None".into(),
        Value::Bool(b) => {
            if *b {
                "True".into()
            } else {
                "False".into()
            }
        }
        Value::String(s) => s.clone(),
        // `Number`'s Display uses the same shortest-round-trip formatting as
        // Python's float/int repr for the integers and ordinary decimals that
        // appear here (e.g. `5.0`, `1.5`, `42`).
        Value::Number(n) => n.to_string(),
        other => other.to_string(),
    }
}

fn is_scalar_or_null(v: &Value) -> bool {
    matches!(
        v,
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_)
    )
}

impl Explorer {
    fn cls(&self, o: &Value) -> String {
        match o {
            Value::Object(m) => m
                .get("__class__")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            Value::Null => "NoneType".into(),
            Value::Bool(_) => "bool".into(),
            Value::String(_) => "str".into(),
            Value::Number(n) => {
                if n.is_f64() {
                    "float".into()
                } else {
                    "int".into()
                }
            }
            Value::Array(_) => "list".into(),
        }
    }

    /// Follow `{"__ref__": id}` chains; returns the resolved value and the id of
    /// the last reference followed (for cycle-safety in `find_refs`).
    fn deref_id<'b>(&'b self, mut v: &'b Value) -> (&'b Value, Option<i32>) {
        let mut last = None;
        let mut seen = 0;
        while let Some(id) = as_ref_id(v) {
            last = Some(id);
            match self.objects.get(&id) {
                Some(nv) => v = nv,
                None => return (&NULL, last),
            }
            seen += 1;
            if seen > 50 {
                break;
            }
        }
        (v, last)
    }

    fn deref<'b>(&'b self, v: &'b Value) -> &'b Value {
        self.deref_id(v).0
    }

    fn get<'b>(&'b self, o: &'b Value, key: &str) -> &'b Value {
        match o {
            Value::Object(m) => m.get(key).unwrap_or(&NULL),
            _ => &NULL,
        }
    }

    /// Deref a member then deref the result (Python `self.deref(o.get(key))`).
    fn deref_get<'b>(&'b self, o: &'b Value, key: &str) -> &'b Value {
        self.deref(self.get(o, key))
    }

    fn as_list<'b>(&'b self, v: &'b Value) -> Vec<&'b Value> {
        let v = self.deref(v);
        if let Value::Object(m) = v {
            if self.cls(v).starts_with("System.Collections.Generic.List") {
                let items = self.deref(m.get("_items").unwrap_or(&NULL));
                let arr: &[Value] = items.as_array().map(|a| a.as_slice()).unwrap_or(&[]);
                let size = m
                    .get("_size")
                    .and_then(|x| self.deref(x).as_i64())
                    .map(|n| n.max(0) as usize)
                    .unwrap_or(arr.len());
                return arr.iter().take(size).map(|x| self.deref(x)).collect();
            }
        }
        if let Value::Array(a) = v {
            return a.iter().map(|x| self.deref(x)).collect();
        }
        Vec::new()
    }

    /// Gather every ValueReference target reachable inside `o` (cycle-safe), in
    /// member/element order. `o` is passed RAW (a possible `__ref__`) so we can
    /// capture object ids for the visited set.
    fn find_refs(&self, o: &Value, seen: &mut HashSet<i32>, depth: i32, out: &mut Vec<Value>) {
        let (o, id) = self.deref_id(o);
        match o {
            Value::Object(m) => {
                if id.map(|i| seen.contains(&i)).unwrap_or(false) || depth > 12 {
                    return;
                }
                if let Some(i) = id {
                    seen.insert(i);
                }
                if self.cls(o).contains("ValueReference") {
                    let on = self.deref_get(o, "ObjectName");
                    if let Some(s) = on.as_str() {
                        if !s.is_empty() {
                            let prop = self.deref_get(o, "PropertyName").clone();
                            out.push(json!({ "object": s, "property": prop }));
                        }
                    }
                }
                for (k, v) in m {
                    if k != "__class__" {
                        self.find_refs(v, seen, depth + 1, out);
                    }
                }
            }
            Value::Array(a) => {
                for v in a {
                    self.find_refs(v, seen, depth + 1, out);
                }
            }
            _ => {}
        }
    }

    fn find_refs_top(&self, o: &Value) -> Vec<Value> {
        let mut out = Vec::new();
        let mut seen = HashSet::new();
        self.find_refs(o, &mut seen, 0, &mut out);
        out
    }

    fn enumv(&self, e: &Value) -> Option<i64> {
        let e = self.deref(e);
        if let Value::Object(m) = e {
            m.get("value__").and_then(|v| v.as_i64())
        } else {
            None
        }
    }

    /// (objectName, propertyName, functionName) of a ValueReference — non-empty
    /// strings only.
    fn ref_parts(&self, r: &Value) -> (Option<String>, Option<String>, Option<String>) {
        let r = self.deref(r);
        if !matches!(r, Value::Object(_)) {
            return (None, None, None);
        }
        let s = |v: &Value| -> Option<String> {
            v.as_str().filter(|x| !x.is_empty()).map(|x| x.to_string())
        };
        let on = s(self.deref_get(r, "ObjectName"));
        let pn = s(self.deref_get(r, "PropertyName"));
        let fnm = s(self.deref_get(r, "FunctionName"));
        (on, pn, fnm)
    }

    fn valstr(&self, c: &Value) -> String {
        let svfv = self.deref_get(c, "SetValueFromValue");
        let vfr = self.deref_get(c, "ValueFromReference");
        if truthy(svfv) || truthy(vfr) {
            let (von, vpn, _) = self.ref_parts(self.get(c, "ValueReference"));
            return match (von, vpn) {
                (Some(a), Some(b)) => format!("{a}.{b}"),
                (Some(a), _) => a,
                _ => "(ref)".into(),
            };
        }
        py_str(self.deref_get(c, "Value"))
    }

    fn command(&self, c: &Value, depth: i32) -> Option<Value> {
        let c = self.deref(c);
        if !matches!(c, Value::Object(_)) || depth > 40 {
            return None;
        }
        let short = short_name(&self.cls(c));
        let (on, pn, fnm) = self.ref_parts(self.get(c, "Reference"));
        let refstr = match (&on, &pn) {
            (Some(a), Some(b)) => format!("{a}.{b}"),
            (Some(a), _) => a.clone(),
            _ => "?".into(),
        };
        let mut node = Map::new();
        node.insert("cmd".into(), Value::String(short.clone()));
        if let Some(o) = &on {
            node.insert("target".into(), Value::String(o.clone()));
        }
        match short.as_str() {
            "If" => {
                let op = if_cond(self.enumv(self.get(c, "Condition")));
                node.insert(
                    "text".into(),
                    Value::String(format!("If {refstr} {op} {}", self.valstr(c))),
                );
                let mut children = Vec::new();
                for k in self.as_list(self.get(c, "CommandList")) {
                    if let Some(x) = self.command(k, depth + 1) {
                        children.push(x);
                    }
                }
                node.insert("children".into(), Value::Array(children));
            }
            "SetValue" => {
                let text = if truthy(self.deref_get(c, "Calculation")) {
                    format!("{refstr} = (calculation)")
                } else {
                    format!("{refstr} = {}", self.valstr(c))
                };
                node.insert("text".into(), Value::String(text));
            }
            "Delay" => {
                let t = py_str(self.deref_get(c, "Time"));
                node.insert("text".into(), Value::String(format!("Delay: {t} Seconds")));
            }
            "Run" => {
                let text = match (&on, &fnm) {
                    (Some(o), Some(f)) => format!("{o}.{f}()"),
                    (Some(o), _) => o.clone(),
                    _ => "Run".into(),
                };
                node.insert("text".into(), Value::String(text));
            }
            "Comment" => {
                let mut txt: Option<String> = None;
                if let Value::Object(m) = c {
                    for (k, v) in m {
                        if k == "__class__" {
                            continue;
                        }
                        let dv = self.deref(v);
                        if let Some(s) = dv.as_str() {
                            if !s.is_empty() {
                                txt = Some(s.to_string());
                                break;
                            }
                        }
                    }
                }
                node.insert(
                    "text".into(),
                    Value::String(match txt {
                        Some(t) => format!("Comment: {t}"),
                        None => "Comment".into(),
                    }),
                );
            }
            _ => {
                node.insert("text".into(), Value::String(short.clone()));
            }
        }
        Some(Value::Object(node))
    }

    fn program_detail(&self, no: &Value) -> Value {
        let mut triggers: Vec<Value> = Vec::new();
        let mut commands: Vec<Value> = Vec::new();
        let mut abort: Vec<Value> = Vec::new();

        for s in self.as_list(self.get(no, "NodeSettings")) {
            let nm = self.deref_get(s, NAME);
            let nm = nm.as_str();
            let val = self.deref_get(s, VAL);
            if nm == Some("Commands") || nm == Some("AbortCommands") {
                let cl = self.get(val, "CommandList");
                let mut steps = Vec::new();
                for k in self.as_list(cl) {
                    if let Some(x) = self.command(k, 0) {
                        steps.push(x);
                    }
                }
                if nm == Some("Commands") {
                    commands = steps;
                } else {
                    abort = steps;
                }
            } else if nm == Some("Triggers") {
                if let Value::Object(_) = val {
                    for t in self.as_list(self.get(val, "TriggerList")) {
                        let (on, pn, _) = self.ref_parts(self.get(t, "Reference"));
                        let cv = self.enumv(self.get(t, "Condition"));
                        let tgt = match (&on, &pn) {
                            (Some(a), Some(b)) => format!("{a}.{b}"),
                            (Some(a), _) => a.clone(),
                            _ => "?".into(),
                        };
                        let txt = if cv == Some(6) {
                            format!("{tgt} OnChange")
                        } else {
                            format!(
                                "{tgt} {} {}",
                                trig_cond(cv),
                                py_str(self.deref_get(t, "Value"))
                            )
                        };
                        let mut tnode = Map::new();
                        tnode.insert("cmd".into(), Value::String("IfTrigger".into()));
                        tnode.insert("text".into(), Value::String(txt));
                        if let Some(o) = &on {
                            tnode.insert("target".into(), Value::String(o.clone()));
                        }
                        triggers.push(Value::Object(tnode));
                    }
                }
            }
        }

        let mut out = Map::new();
        out.insert("triggers".into(), Value::Array(triggers));
        out.insert("commands".into(), Value::Array(commands));
        out.insert("abort".into(), Value::Array(abort));
        Value::Object(out)
    }

    fn summarize(&self, host: &Value) -> Value {
        let no = self.deref_get(host, "NodeObject");
        let name = self.deref_get(host, "Name").clone();
        let path = self.deref_get(host, "Path").clone();
        let ntype = self
            .cls(no)
            .replace("ComfortClick.Tasks.", "")
            .replace(", ComfortClick.Tasks", "");

        let mut settings = Map::new();
        let mut inputs: Vec<Value> = Vec::new();
        let mut output: Value = Value::Null;
        let mut writes: Vec<Value> = Vec::new();
        let mut values = Map::new();

        if !matches!(no, Value::Object(_)) {
            return assemble_node(
                name, path, ntype, settings, inputs, output, writes, values, None,
            );
        }

        for s in self.as_list(self.get(no, "NodeSettings")) {
            let nm_v = self.deref_get(s, NAME);
            let nm = nm_v.as_str();
            let val_raw = self.get(s, VAL);
            let val = self.deref(val_raw);

            // block A — scalar settings
            if nm == Some("Type") {
                if let Value::Object(m) = val {
                    if let Some(vv) = m.get("value__") {
                        settings.insert("Type".into(), vv.clone());
                    } else if is_scalar_or_null(val) {
                        if let Some(n) = nm {
                            settings.insert(n.into(), val.clone());
                        }
                    }
                } else if is_scalar_or_null(val) {
                    settings.insert("Type".into(), val.clone());
                }
            } else if is_scalar_or_null(val) {
                if let Some(n) = nm {
                    settings.insert(n.into(), val.clone());
                }
            }

            // block B — references
            if nm == Some("InputValues") {
                inputs = self.find_refs_top(val_raw);
            } else if nm == Some("OutputValue") {
                let r = self.find_refs_top(val_raw);
                output = r.into_iter().next().unwrap_or(Value::Null);
            } else if nm != Some("Type") && nm != Some("InvertedOutputValue") {
                writes.extend(self.find_refs_top(val_raw));
            }
        }

        for v in self.as_list(self.get(no, "NodeValues")) {
            let vn = self.deref_get(v, VNAME);
            let cv = self.deref_get(v, VAL);
            if let Some(vn) = vn.as_str() {
                if !vn.is_empty() && is_scalar_or_null(cv) {
                    values.insert(vn.into(), cv.clone());
                }
            }
        }

        let program = if ntype == "Program" {
            Some(self.program_detail(no))
        } else {
            None
        };

        assemble_node(
            name, path, ntype, settings, inputs, output, writes, values, program,
        )
    }

    fn hosts(&self) -> Vec<&Value> {
        self.order
            .iter()
            .filter_map(|id| self.objects.get(id))
            .filter(|o| {
                matches!(o, Value::Object(m)
                    if m.get("__class__").and_then(|v| v.as_str()) == Some(NODEHOST))
            })
            .collect()
    }
}

/// `If+ConditionTypes` style short name: last `.`-segment, before any `+`.
fn short_name(cls: &str) -> String {
    let after_dot = cls.rsplit('.').next().unwrap_or(cls);
    after_dot.split('+').next().unwrap_or(after_dot).to_string()
}

/// Assemble a node object in the exact key order bos_explore.py emits.
#[allow(clippy::too_many_arguments)]
fn assemble_node(
    name: Value,
    path: Value,
    ntype: String,
    settings: Map<String, Value>,
    inputs: Vec<Value>,
    output: Value,
    writes: Vec<Value>,
    values: Map<String, Value>,
    program: Option<Value>,
) -> Value {
    let mut d = Map::new();
    d.insert("name".into(), name);
    d.insert("path".into(), path);
    d.insert("type".into(), Value::String(ntype));
    d.insert("settings".into(), Value::Object(settings));
    d.insert("inputs".into(), Value::Array(inputs));
    d.insert("output".into(), output);
    d.insert("writes".into(), Value::Array(writes));
    d.insert("values".into(), Value::Object(values));
    if let Some(p) = program {
        d.insert("program".into(), p);
    }
    Value::Object(d)
}

fn richness(s: &Value) -> usize {
    let len = |k: &str| match s.get(k) {
        Some(Value::Object(m)) => m.len(),
        Some(Value::Array(a)) => a.len(),
        _ => 0,
    };
    len("settings") + len("inputs") + len("writes") + len("values")
}

/// Dedup key: `str(s["path"] or s["name"])`.
fn dedup_key(s: &Value) -> String {
    let path = s.get("path").unwrap_or(&NULL);
    if truthy(path) {
        py_str(path)
    } else {
        py_str(s.get("name").unwrap_or(&NULL))
    }
}

/// Parse a `.bos` file into the node array, identical to bos_explore.py.
pub fn parse_bos_file(bytes: &[u8]) -> Result<Vec<Value>, String> {
    let mut reader = Reader::new(bytes);
    reader.parse()?;
    let ex = Explorer {
        objects: reader.objects,
        order: reader.order,
    };

    // Summaries in host (insertion) order, then dedup by path keeping the
    // richest (first wins on a tie), then a stable sort by str(path).
    let mut best: HashMap<String, Value> = HashMap::new();
    let mut best_order: Vec<String> = Vec::new();
    for host in ex.hosts() {
        let s = ex.summarize(host);
        let key = dedup_key(&s);
        match best.get(&key) {
            None => {
                best_order.push(key.clone());
                best.insert(key, s);
            }
            Some(existing) => {
                if richness(&s) > richness(existing) {
                    best.insert(key, s);
                }
            }
        }
    }
    let mut nodes: Vec<Value> = best_order
        .into_iter()
        .filter_map(|k| best.remove(&k))
        .collect();
    nodes.sort_by(|a, b| {
        py_str(a.get("path").unwrap_or(&NULL)).cmp(&py_str(b.get("path").unwrap_or(&NULL)))
    });
    Ok(nodes)
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- tiny MS-NRBF blob builder (little-endian, matching the spec) ----
    struct Blob(Vec<u8>);
    impl Blob {
        fn new() -> Self {
            Blob(Vec::new())
        }
        fn u8(&mut self, v: u8) -> &mut Self {
            self.0.push(v);
            self
        }
        fn i32(&mut self, v: i32) -> &mut Self {
            self.0.extend_from_slice(&v.to_le_bytes());
            self
        }
        /// length-prefixed UTF-8 string (assumes len < 128 for the test inputs)
        fn lps(&mut self, s: &str) -> &mut Self {
            assert!(s.len() < 128);
            self.0.push(s.len() as u8);
            self.0.extend_from_slice(s.as_bytes());
            self
        }
        fn header(&mut self, root_id: i32) -> &mut Self {
            self.u8(0).i32(root_id).i32(1).i32(1).i32(0);
            self
        }
        fn end(&mut self) -> &mut Self {
            self.u8(11);
            self
        }
    }

    // A BinaryObjectString record (id + length-prefixed string).
    fn obj_string(b: &mut Blob, id: i32, s: &str) {
        b.u8(6).i32(id).lps(s);
    }

    #[test]
    fn parses_a_node_host_with_scalar_setting() {
        // Build a NodeHost whose NodeObject has one NodeSettings list entry with
        // a scalar <Value>, and assert summarize() extracts name/path/type/settings.
        let mut b = Blob::new();
        b.header(1);

        // id 10: the setting value (a string)
        obj_string(&mut b, 10, "hello");

        // id 9: the NodeSettingsInfo object: { Name: "Greeting", <Value>: ref->10 }
        // ClassWithMembersAndTypes: id, name, memberCount, names[], memberTypes,
        // additional, libraryId, then member values.
        b.u8(5).i32(9).lps("NodeSettingsInfo");
        b.i32(2).lps(NAME).lps(VAL);
        b.u8(1).u8(1); // both members are String (BinaryType 1)
        b.i32(0); // libraryId
                  // member 0 (Name): inline BinaryObjectString id 11
        obj_string(&mut b, 11, "Greeting");
        // member 1 (<Value>): MemberReference -> 10
        b.u8(9).i32(10);

        // id 8: the List<T> holding [ref->9]
        // members: _items (ObjectArray), _size (Int32), _version (Int32)
        b.u8(5).i32(8).lps("System.Collections.Generic.List`1[[X]]");
        b.i32(3).lps("_items").lps("_size").lps("_version");
        b.u8(5).u8(0).u8(0); // _items:ObjectArray(5), _size:Primitive(0), _version:Primitive(0)
        b.u8(8).u8(8); // additional: Int32 for the two primitives
        b.i32(0); // libraryId
                  // _items: inline ArraySingleObject id 12, length 1, [ref->9]
        b.u8(16).i32(12).i32(1);
        b.u8(9).i32(9);
        // _size: 1, _version: 1
        b.i32(1).i32(1);

        // id 7: the NodeObject { NodeSettings: ref->8, NodeValues: null }
        b.u8(5).i32(7).lps("ComfortClick.Tasks.Variable.String");
        b.i32(2).lps("NodeSettings").lps("NodeValues");
        b.u8(4).u8(4); // both Class
        b.lps("System.Collections.Generic.List`1[[X]]").i32(0);
        b.lps("System.Collections.Generic.List`1[[Y]]").i32(0);
        b.i32(0); // libraryId
        b.u8(9).i32(8); // NodeSettings -> 8
        b.u8(10); // NodeValues -> ObjectNull

        // id 1: the NodeHost { Name: ref, Path: ref, NodeObject: ref->7 }
        b.u8(5).i32(1).lps(NODEHOST);
        b.i32(3).lps("Name").lps("Path").lps("NodeObject");
        b.u8(1).u8(1).u8(4); // Name:String, Path:String, NodeObject:Class
        b.lps("ComfortClick.Tasks.Variable.String").i32(0);
        b.i32(0); // libraryId
        obj_string(&mut b, 2, "Greeting"); // Name
        obj_string(&mut b, 3, "Root\\Greeting"); // Path
        b.u8(9).i32(7); // NodeObject -> 7

        b.end();

        let nodes = parse_bos_file(&b.0).expect("parse");
        assert_eq!(nodes.len(), 1, "exactly one NodeHost");
        let n = &nodes[0];
        assert_eq!(n["name"], json!("Greeting"));
        assert_eq!(n["path"], json!("Root\\Greeting"));
        assert_eq!(n["type"], json!("Variable.String"));
        assert_eq!(n["settings"]["Greeting"], json!("hello"));
        assert!(n["inputs"].as_array().unwrap().is_empty());
        assert_eq!(n["output"], Value::Null);
    }

    #[test]
    fn dedup_keeps_richest_per_path() {
        // Two NodeHosts at the same path; the one with a setting must win.
        let n_empty = json!({
            "name": "a", "path": "P", "type": "Gate",
            "settings": {}, "inputs": [], "output": Value::Null, "writes": [], "values": {}
        });
        let n_rich = json!({
            "name": "a", "path": "P", "type": "Gate",
            "settings": {"k": 1}, "inputs": [], "output": Value::Null, "writes": [], "values": {}
        });
        assert!(richness(&n_rich) > richness(&n_empty));
        assert_eq!(dedup_key(&n_rich), "P");
        let no_path = json!({ "name": "fallback", "path": Value::Null });
        assert_eq!(dedup_key(&no_path), "fallback");
    }

    #[test]
    fn py_str_matches_python() {
        assert_eq!(py_str(&json!(5)), "5");
        assert_eq!(py_str(&json!(5.0)), "5.0");
        assert_eq!(py_str(&json!(1.5)), "1.5");
        assert_eq!(py_str(&Value::Null), "None");
        assert_eq!(py_str(&json!(true)), "True");
        assert_eq!(py_str(&json!(false)), "False");
        assert_eq!(py_str(&json!("x")), "x");
    }

    #[test]
    fn ref_marker_detection() {
        assert_eq!(as_ref_id(&json!({"__ref__": 42})), Some(42));
        assert_eq!(as_ref_id(&json!({"__ref__": 1, "other": 2})), None);
        assert_eq!(as_ref_id(&json!({"__class__": "X"})), None);
    }

    #[test]
    fn short_name_strips_namespace_and_nested() {
        assert_eq!(
            short_name("ComfortClick.Tasks.TaskCommands.SetValue"),
            "SetValue"
        );
        assert_eq!(short_name("A.B.If+ConditionTypes"), "If");
        assert_eq!(short_name("Bare"), "Bare");
    }

    // ---- Malformed-input hardening (these MUST return Err, never panic/abort) ----
    //
    // Each test constructs the minimal crafted byte sequence a reviewer described
    // for a process-abort DoS in the MS-NRBF reader (Rust OOM and stack overflow
    // are non-unwinding aborts, so the parser must refuse BEFORE allocating or
    // recursing on a file-controlled size). A passing run proves the size is now
    // bounded — without the fixes these inputs crash the test binary outright.

    #[test]
    fn rejects_giant_member_count() {
        // ClassWithMembersAndTypes claiming i32::MAX members, then truncated.
        // Without the capacity bound this reserves ~48 GB and aborts.
        let mut b = Blob::new();
        b.header(1);
        b.u8(5).i32(2).lps("C").i32(i32::MAX);
        // (no member names follow — the bounds-checked read loop must Err)
        let r = parse_bos_file(&b.0);
        assert!(r.is_err(), "giant member count must Err, got {r:?}");
    }

    #[test]
    fn rejects_giant_array_rank() {
        // BinaryArray with i32::MAX rank, then truncated. Without the capacity
        // bound the `lengths` Vec reservation aborts the process.
        let mut b = Blob::new();
        b.header(1);
        b.u8(7).i32(2).u8(0).i32(i32::MAX);
        let r = parse_bos_file(&b.0);
        assert!(r.is_err(), "giant array rank must Err, got {r:?}");
    }

    #[test]
    fn bounds_object_null_multiple_count() {
        // ArraySingleObject declaring i32::MAX length whose first element is an
        // ObjectNullMultiple (type 14) claiming i32::MAX nulls. Both the length
        // and the null count are bounded to the remaining buffer, so instead of
        // pushing ~2B nulls (~48 GB) the parser stays bounded and surfaces the
        // trailing invalid top-level record as a clean Err.
        let mut b = Blob::new();
        b.header(1);
        b.u8(16).i32(2).i32(i32::MAX); // ArraySingleObject id=2, length=i32::MAX
        b.u8(14).i32(i32::MAX); // ObjectNullMultiple count=i32::MAX
        b.u8(99); // invalid top-level record after the (bounded) array
        let r = parse_bos_file(&b.0);
        assert!(
            r.is_err(),
            "huge ObjectNullMultiple must stay bounded + Err, got {r:?}"
        );
    }

    #[test]
    fn survives_long_binary_library_chain() {
        // A class whose single member's inline value is a chain of 200k inline
        // BinaryLibrary (type 12) records. The old recursive reader overflowed the
        // stack (~120 KB of chain) and aborted; the iterative reader must parse it
        // without crashing. Chain terminates in ObjectNull so the parse succeeds.
        let mut b = Blob::new();
        b.header(1);
        b.u8(5).i32(2).lps("C").i32(1).lps("m"); // class def: 1 member "m"
        b.u8(1); // member type: String (no additional info)
        b.i32(0); // libraryId
        for _ in 0..200_000 {
            b.u8(12).i32(0).lps("L"); // inline BinaryLibrary
        }
        b.u8(10); // ObjectNull terminates the inline value
        b.end();
        let r = parse_bos_file(&b.0);
        assert!(
            r.is_ok(),
            "long type-12 chain must not overflow the stack, got {r:?}"
        );
    }

    #[test]
    fn rejects_overlong_string_varint() {
        // BinaryObjectString whose 7-bit length prefix has six continuation bytes.
        // The spec caps the prefix at five bytes (35 bits); a sixth byte would
        // shift by 35 and panic on a 32-bit usize, so it must be rejected.
        let mut b = Blob::new();
        b.header(1);
        b.u8(6).i32(2); // BinaryObjectString id=2
        for _ in 0..6 {
            b.u8(0x80); // continuation bit set, six times
        }
        b.u8(0x00);
        let r = parse_bos_file(&b.0);
        assert!(r.is_err(), "over-long string varint must Err, got {r:?}");
    }

    // ---- Sample-file parity check (manual; not run in CI) ----
    //
    // Run with:
    //   HYPERION_SAMPLE_BOS=<path>.bos HYPERION_PY_JSON=<py_out>.json \
    //     cargo test --lib -- --ignored --nocapture sample_parity
    //
    // Asserts the pure-Rust parser reproduces the Python reference node array
    // exactly (count + per-node deep equality, order-insensitive for object
    // members thanks to preserve_order/IndexMap, order-sensitive for arrays).
    #[test]
    #[ignore = "needs a real .bos sample + the Python reference JSON (env vars)"]
    fn sample_parity() {
        let bos = std::env::var("HYPERION_SAMPLE_BOS").expect("set HYPERION_SAMPLE_BOS");
        let pyj = std::env::var("HYPERION_PY_JSON").expect("set HYPERION_PY_JSON");
        let bytes = std::fs::read(&bos).expect("read .bos");
        let rust_nodes = parse_bos_file(&bytes).expect("rust parse");
        let py_text = std::fs::read_to_string(&pyj).expect("read py json");
        let py_nodes: Vec<Value> = serde_json::from_str(&py_text).expect("parse py json");

        eprintln!(
            "rust nodes = {}, python nodes = {}",
            rust_nodes.len(),
            py_nodes.len()
        );
        assert_eq!(rust_nodes.len(), py_nodes.len(), "node count parity");

        let mut mismatched = 0usize;
        let mut field_mismatches = 0usize;
        for (i, (r, p)) in rust_nodes.iter().zip(py_nodes.iter()).enumerate() {
            if r != p {
                mismatched += 1;
                // Identify which top-level fields differ for the report.
                if let (Value::Object(rm), Value::Object(pm)) = (r, p) {
                    let mut keys: Vec<&String> = rm.keys().chain(pm.keys()).collect();
                    keys.sort();
                    keys.dedup();
                    for k in keys {
                        if rm.get(k) != pm.get(k) {
                            field_mismatches += 1;
                            if mismatched <= 15 {
                                eprintln!(
                                    "node[{i}] path={} field '{k}':\n  rust={}\n  py  ={}",
                                    p.get("path").map(py_str).unwrap_or_default(),
                                    rm.get(k).map(|v| v.to_string()).unwrap_or_default(),
                                    pm.get(k).map(|v| v.to_string()).unwrap_or_default(),
                                );
                            }
                        }
                    }
                }
            }
        }
        eprintln!(
            "PARITY: {}/{} nodes equal; {mismatched} nodes differ; {field_mismatches} field mismatches",
            rust_nodes.len() - mismatched,
            rust_nodes.len()
        );
        assert_eq!(mismatched, 0, "all nodes must match the Python reference");
    }
}
