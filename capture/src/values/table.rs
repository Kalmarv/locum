//! `table.txt`: what a game's values and actions are, written by you for one executable. One definition per line,
//! `#` starts a comment:
//!
//! - `exe sha256 HEX`: the table is for that executable only; locum refuses it for any other;
//! - `code NAME = PATTERN [+N | -N]`: the address of the one match of a byte pattern (hex bytes, `??` any byte) in
//!   the executable's code sections, plus or minus N;
//! - `global NAME = PATTERN @ AT END`: the address a RIP-relative operand in the match refers to: its 32-bit
//!   displacement is at byte AT of the match and its instruction ends at byte END;
//! - `value NAME TYPE = ADDRESS`: a value of TYPE (`i8` `i16` `i32` `i64` `u8` `u16` `u32` `u64` `f32` `f64`, or
//!   `bitN`: bit N of the value there, 0 to 63) at an address expression;
//! - `action NAME [PARAM ...]`, then steps, then `end`: `buffer NAME SIZE` (zeroed memory for the run, freed after
//!   it), `set TYPE ADDRESS VALUE`, `call ADDRESS [ARG ...]` (integer and pointer arguments; `result` is what it
//!   returned) and `wait MS`.
//!
//! Every name an expression uses must be defined: in a value, a symbol or another value (`{v}` reads it, a bare
//! name is its address), and no value may depend on itself; in an action, those, its parameters, the buffers it
//! made before, and `result`.

use super::expr::{Expr, Num, Ref, number};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Type {
    I8,
    I16,
    I32,
    I64,
    U8,
    U16,
    U32,
    U64,
    F32,
    F64,
    /// One bit, 0 or 1, counted from the low bit of the little-endian value at the address (Cheat Engine's
    /// Binary entries: their bit start); setting it leaves the other bits as they are.
    Bit(u8),
}

impl Type {
    pub fn parse(t: &str) -> Result<Type, String> {
        if let Some(n) = t.strip_prefix("bit") {
            return match n.parse::<u8>() {
                Ok(n) if n < 64 => Ok(Type::Bit(n)),
                _ => Err(format!("{t:?}: bits are bit0 to bit63")),
            };
        }
        Ok(match t {
            "i8" => Type::I8,
            "i16" => Type::I16,
            "i32" => Type::I32,
            "i64" => Type::I64,
            "u8" => Type::U8,
            "u16" => Type::U16,
            "u32" => Type::U32,
            "u64" | "ptr" => Type::U64,
            "f32" => Type::F32,
            "f64" => Type::F64,
            _ => return Err(format!("unknown type {t:?}")),
        })
    }

    pub fn size(self) -> usize {
        match self {
            Type::I8 | Type::U8 => 1,
            Type::I16 | Type::U16 => 2,
            Type::I32 | Type::U32 | Type::F32 => 4,
            Type::I64 | Type::U64 | Type::F64 => 8,
            Type::Bit(n) => n as usize / 8 + 1,
        }
    }

    /// A value from its little-endian bytes (`size` of them).
    pub fn decode(self, b: &[u8]) -> Num {
        let mut w = [0u8; 8];
        w[..b.len()].copy_from_slice(b);
        let raw = u64::from_le_bytes(w);
        match self {
            Type::I8 => Num::Int(raw as u8 as i8 as i64 as u64),
            Type::I16 => Num::Int(raw as u16 as i16 as i64 as u64),
            Type::I32 => Num::Int(raw as u32 as i32 as i64 as u64),
            Type::I64 | Type::U64 | Type::U8 | Type::U16 | Type::U32 => Num::Int(raw),
            Type::F32 => Num::Float(f32::from_bits(raw as u32) as f64),
            Type::F64 => Num::Float(f64::from_bits(raw)),
            Type::Bit(n) => Num::Int(raw >> n & 1),
        }
    }

    /// The little-endian bytes to write for a value over `current` (the `size` bytes there now, which only a bit
    /// keeps): integers are cut to the type's size, floats rounded to it; a float with a fraction is no integer.
    pub fn encode(self, value: Num, current: &[u8]) -> Result<Vec<u8>, String> {
        Ok(match self {
            Type::F32 => (value.float() as f32).to_le_bytes().to_vec(),
            Type::F64 => value.float().to_le_bytes().to_vec(),
            Type::Bit(n) => {
                let bit = match value.int()? {
                    v @ (0 | 1) => v,
                    v => return Err(format!("a bit is 0 or 1, not {v}")),
                };
                let mut w = [0u8; 8];
                w[..current.len()].copy_from_slice(current);
                let raw = u64::from_le_bytes(w) & !(1 << n) | bit << n;
                raw.to_le_bytes()[..self.size()].to_vec()
            }
            _ => value.int()?.to_le_bytes()[..self.size()].to_vec(),
        })
    }
}

/// A byte pattern: `None` matches any byte.
pub type Pattern = Vec<Option<u8>>;

pub fn pattern(text: &str) -> Result<Pattern, String> {
    let p: Pattern = text
        .split_whitespace()
        .map(|t| match t {
            "?" | "??" => Ok(None),
            _ if t.len() == 2 => u8::from_str_radix(t, 16)
                .map(Some)
                .map_err(|_| format!("bad byte {t:?}")),
            _ => Err(format!("bad byte {t:?}")),
        })
        .collect::<Result<_, _>>()?;
    match p.first() {
        None => Err("empty pattern".into()),
        Some(None) if p.iter().all(Option::is_none) => Err("a pattern of wildcards only".into()),
        _ => Ok(p),
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum Find {
    /// The match's address plus an offset.
    Code { pattern: Pattern, offset: i64 },
    /// The target of a RIP-relative operand in the match.
    Global { pattern: Pattern, at: usize, end: usize },
}

#[derive(Clone, Debug, PartialEq)]
pub enum Step {
    Buffer { name: String, size: usize },
    Set { ty: Type, address: Expr, value: Expr },
    Call { function: Expr, args: Vec<Expr> },
    Wait { ms: u64 },
}

#[derive(Clone, Debug, PartialEq)]
pub struct Action {
    pub params: Vec<String>,
    pub steps: Vec<Step>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Table {
    pub exe_sha256: Option<String>,
    pub symbols: Vec<(String, Find)>,
    pub values: Vec<(String, Type, Expr)>,
    pub actions: Vec<(String, Action)>,
}

/// The most arguments `call` passes.
pub const MAX_ARGS: usize = 8;

fn name(t: &str) -> Result<String, String> {
    let ok = !t.is_empty()
        && !t.starts_with(|c: char| c.is_ascii_digit())
        && t.chars().all(|c| c.is_ascii_alphanumeric() || "_.:".contains(c));
    if ok {
        Ok(t.to_string())
    } else {
        Err(format!("bad name {t:?}"))
    }
}

/// `NAME = REST`
fn assignment(rest: &str) -> Result<(String, &str), String> {
    let (n, body) = rest.split_once('=').ok_or("expected NAME = …")?;
    Ok((name(n.trim())?, body.trim()))
}

pub fn parse(text: &str) -> Result<Table, String> {
    let mut table = Table::default();
    let mut open: Option<(String, Action)> = None;
    for (n, raw) in text.lines().enumerate() {
        let line = raw.split('#').next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        let at = |e: String| format!("table.txt line {}: {e}", n + 1);
        let (word, rest) = line.split_once(char::is_whitespace).unwrap_or((line, ""));
        let rest = rest.trim();
        if let Some((_, action)) = open.as_mut() {
            let step = match word {
                "end" => {
                    let (name, action) = open.take().expect("open action");
                    table.actions.push((name, action));
                    continue;
                }
                "buffer" => {
                    let (b, size) = rest
                        .split_once(char::is_whitespace)
                        .ok_or_else(|| at("buffer NAME SIZE".into()))?;
                    let size = number(size).and_then(|s| s.int()).map_err(at)? as usize;
                    if size == 0 || size > 1 << 20 {
                        return Err(at(format!("buffer size {size} is not 1 to 1 MiB")));
                    }
                    Step::Buffer {
                        name: name(b).map_err(at)?,
                        size,
                    }
                }
                "set" => {
                    let mut parts = rest.splitn(2, char::is_whitespace);
                    let ty = Type::parse(parts.next().unwrap_or("")).map_err(at)?;
                    let (address, value) = split_last(parts.next().unwrap_or(""))
                        .ok_or_else(|| at("set TYPE ADDRESS VALUE".into()))?;
                    Step::Set {
                        ty,
                        address: Expr::parse(address).map_err(at)?,
                        value: Expr::parse(value).map_err(at)?,
                    }
                }
                "call" => {
                    let mut parts = rest.split_whitespace();
                    let function =
                        Expr::parse(parts.next().ok_or_else(|| at("call ADDRESS [ARG ...]".into()))?)
                            .map_err(at)?;
                    let args = parts
                        .map(Expr::parse)
                        .collect::<Result<Vec<_>, _>>()
                        .map_err(at)?;
                    if args.len() > MAX_ARGS {
                        return Err(at(format!("call takes at most {MAX_ARGS} arguments")));
                    }
                    Step::Call { function, args }
                }
                "wait" => Step::Wait {
                    ms: number(rest).and_then(|s| s.int()).map_err(at)?,
                },
                other => return Err(at(format!("unknown step {other:?} (or a missing end)"))),
            };
            action.steps.push(step);
            continue;
        }
        match word {
            "exe" => {
                let hex = rest
                    .strip_prefix("sha256")
                    .map(str::trim)
                    .ok_or_else(|| at("exe sha256 HEX".into()))?;
                if hex.len() != 64 || !hex.chars().all(|c| c.is_ascii_hexdigit()) {
                    return Err(at("exe sha256 needs 64 hex digits".into()));
                }
                table.exe_sha256 = Some(hex.to_ascii_lowercase());
            }
            "code" => {
                let (n, body) = assignment(rest).map_err(at)?;
                let mut tokens: Vec<&str> = body.split_whitespace().collect();
                let mut offset = 0i64;
                if let Some(last) = tokens.last()
                    && let Some(sign) = last.chars().next().filter(|c| *c == '+' || *c == '-')
                {
                    let v = number(&last[1..]).and_then(|v| v.int()).map_err(at)? as i64;
                    offset = if sign == '-' { -v } else { v };
                    tokens.pop();
                }
                table.symbols.push((
                    n,
                    Find::Code {
                        pattern: pattern(&tokens.join(" ")).map_err(at)?,
                        offset,
                    },
                ));
            }
            "global" => {
                let (n, body) = assignment(rest).map_err(at)?;
                let (p, operand) = body
                    .split_once('@')
                    .ok_or_else(|| at("global NAME = PATTERN @ AT END".into()))?;
                let nums: Vec<usize> = operand
                    .split_whitespace()
                    .map(|t| number(t).and_then(|v| v.int()).map(|v| v as usize))
                    .collect::<Result<_, _>>()
                    .map_err(at)?;
                let pattern = pattern(p).map_err(at)?;
                let [at_, end] = nums[..] else {
                    return Err(at("global needs @ AT END".into()));
                };
                if at_ + 4 > pattern.len() || end > pattern.len() || end < at_ + 4 {
                    return Err(at(
                        "the operand must lie inside the pattern, before its instruction's end".into(),
                    ));
                }
                table.symbols.push((
                    n,
                    Find::Global {
                        pattern,
                        at: at_,
                        end,
                    },
                ));
            }
            "value" => {
                let (head, body) = rest
                    .split_once('=')
                    .ok_or_else(|| at("value NAME TYPE = ADDRESS".into()))?;
                let mut h = head.split_whitespace();
                let (Some(n), Some(t), None) = (h.next(), h.next(), h.next()) else {
                    return Err(at("value NAME TYPE = ADDRESS".into()));
                };
                table.values.push((
                    name(n).map_err(at)?,
                    Type::parse(t).map_err(at)?,
                    Expr::parse(body).map_err(at)?,
                ));
            }
            "action" => {
                let mut parts = rest.split_whitespace();
                let n = name(parts.next().unwrap_or("")).map_err(at)?;
                let params = parts.map(name).collect::<Result<Vec<_>, _>>().map_err(at)?;
                open = Some((
                    n,
                    Action {
                        params,
                        steps: Vec::new(),
                    },
                ));
            }
            other => return Err(at(format!("unknown definition {other:?}"))),
        }
    }
    if let Some((n, _)) = open {
        return Err(format!("table.txt: action {n} has no end"));
    }
    let mut names: Vec<&str> = table.symbols.iter().map(|s| s.0.as_str()).collect();
    names.extend(table.values.iter().map(|v| v.0.as_str()));
    names.extend(table.actions.iter().map(|a| a.0.as_str()));
    names.sort_unstable();
    if let Some(w) = names.windows(2).find(|w| w[0] == w[1]) {
        return Err(format!("table.txt: {} is defined twice", w[0]));
    }
    check(&table)?;
    Ok(table)
}

/// The names every expression uses are defined where it is used, and no value depends on itself.
fn check(table: &Table) -> Result<(), String> {
    let symbol = |n: &str| table.symbols.iter().any(|s| s.0 == n);
    let value = |n: &str| table.values.iter().position(|v| v.0 == n);
    let mut depends: Vec<Vec<usize>> = Vec::new();
    for (name, _, address) in &table.values {
        let mut on = Vec::new();
        for r in address.refs() {
            match r {
                Ref::Name(n) | Ref::Value(n) if value(n).is_some() => on.extend(value(n)),
                Ref::Name(n) if symbol(n) => {}
                Ref::Name(n) => return Err(format!("table.txt: value {name}: {n} is not defined")),
                Ref::Value(n) => return Err(format!("table.txt: value {name}: {{{n}}} is not a value")),
                Ref::Param(p) => {
                    return Err(format!(
                        "table.txt: value {name}: ${p}: values take no parameters"
                    ));
                }
            }
        }
        depends.push(on);
    }
    // a value whose address reads itself would recurse without end: walk each value's dependencies
    for start in 0..depends.len() {
        let mut seen = vec![false; depends.len()];
        let mut todo = depends[start].clone();
        while let Some(v) = todo.pop() {
            if v == start {
                return Err(format!(
                    "table.txt: value {} depends on itself",
                    table.values[start].0
                ));
            }
            if !std::mem::replace(&mut seen[v], true) {
                todo.extend(&depends[v]);
            }
        }
    }
    for (name, action) in &table.actions {
        let mut buffers: Vec<&str> = Vec::new();
        let uses = |e: &Expr, buffers: &[&str]| -> Result<(), String> {
            for r in e.refs() {
                let ok = match r {
                    Ref::Name(n) => n == "result" || symbol(n) || value(n).is_some() || buffers.contains(&n),
                    Ref::Value(n) => value(n).is_some(),
                    Ref::Param(p) => action.params.iter().any(|q| q == p),
                };
                if !ok {
                    let what = match r {
                        Ref::Name(n) => format!("{n} is not defined (nor a buffer made before)"),
                        Ref::Value(n) => format!("{{{n}}} is not a value"),
                        Ref::Param(p) => format!("${p} is not one of its parameters"),
                    };
                    return Err(format!("table.txt: action {name}: {what}"));
                }
            }
            Ok(())
        };
        for step in &action.steps {
            match step {
                Step::Buffer { name, .. } => buffers.push(name),
                Step::Set { address, value, .. } => {
                    uses(address, &buffers)?;
                    uses(value, &buffers)?;
                }
                Step::Call { function, args } => {
                    for e in std::iter::once(function).chain(args) {
                        uses(e, &buffers)?;
                    }
                }
                Step::Wait { .. } => {}
            }
        }
    }
    Ok(())
}

/// Splits off the last whitespace-separated word: `ADDRESS VALUE` where the address may hold spaces.
pub fn split_last(text: &str) -> Option<(&str, &str)> {
    let t = text.trim();
    let (a, v) = t.rsplit_once(char::is_whitespace)?;
    (!a.trim().is_empty()).then_some((a.trim(), v))
}

#[cfg(test)]
mod tests {
    use super::*;

    const TABLE: &str = "
        # a made-up game
        exe sha256 00112233445566778899AABBCCDDEEFF00112233445566778899AABBCCDDEEFF
        code Spawn = 40 53 55 ?? 57 +0x10
        code Other = 74 ?? 48 85 d2 -0xD
        global Root = 48 8B 05 ?? ?? ?? ?? 48 85 C0 @ 3 7
        value thing.x f32 = [[[Root]+0x100]+0]+0x40
        action spawn id x y z
          buffer b 0x200
          set i32 b+0x14 $id
          set f32 b+0x80 $x
          call Spawn [Root] b+0x180 b b+0x1C0
          wait 10
        end
    ";

    #[test]
    fn a_table_parses() {
        let t = parse(TABLE).unwrap();
        assert_eq!(
            t.exe_sha256.as_deref(),
            Some("00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff")
        );
        assert_eq!(t.symbols.len(), 3);
        assert!(matches!(&t.symbols[0].1, Find::Code { offset: 16, pattern } if pattern[3].is_none()));
        assert!(matches!(&t.symbols[1].1, Find::Code { offset: -13, .. }));
        assert!(matches!(&t.symbols[2].1, Find::Global { at: 3, end: 7, .. }));
        assert_eq!(t.values[0].1, Type::F32);
        let (n, a) = &t.actions[0];
        assert_eq!((n.as_str(), a.params.len(), a.steps.len()), ("spawn", 4, 5));
        assert!(matches!(&a.steps[3], Step::Call { args, .. } if args.len() == 4));
    }

    #[test]
    fn mistakes_name_their_line() {
        for (bad, says) in [
            ("code X = 4G", "line 1"),
            ("global X = 48 8B @ 3 7", "inside the pattern"),
            ("value X q32 = 1", "unknown type"),
            ("action a\n set f32 b", "line 2"),
            ("action a\n call f", "no end"),
            ("code X = 48\ncode X = 49", "defined twice"),
            ("code X = ?? ??", "wildcards only"),
            ("value v f32 = [[0x10]+4", "missing ]"),
            ("value v f32 = Nowhere+4", "Nowhere is not defined"),
            ("value v f32 = {w}", "{w} is not a value"),
            ("value v f32 = $p", "take no parameters"),
            ("value v u64 = [w]\nvalue w u64 = {v}+8", "depends on itself"),
            ("action a x\n set u8 b 1\n buffer b 8\nend", "b is not defined"),
            (
                "action a x\n call 0x10 $y\nend",
                "$y is not one of its parameters",
            ),
        ] {
            let e = parse(bad).unwrap_err();
            assert!(e.contains(says), "{bad:?}: {e}");
        }
        // names defined later, buffers made before, result and a value's address by its bare name are fine
        parse("value v u64 = [w]+8\nvalue w u64 = 0x10\naction a\n buffer b 8\n call w b {v}\n set u8 b result\nend")
            .unwrap();
    }

    #[test]
    fn values_round_trip_their_types() {
        let round = |ty: Type, v: Num| ty.decode(&ty.encode(v, &[0; 8][..ty.size()]).unwrap());
        assert_eq!(round(Type::I32, Num::Int(u64::MAX)), Num::Int(u64::MAX));
        assert_eq!(round(Type::U8, Num::Int(0x1ff)), Num::Int(0xff));
        assert_eq!(round(Type::F32, Num::Int(3)), Num::Float(3.0));
        assert!(Type::I32.encode(Num::Float(1.5), &[0; 4]).is_err());
    }

    #[test]
    fn bits_change_only_themselves() {
        let ty = Type::parse("bit9").unwrap();
        assert_eq!((ty, ty.size()), (Type::Bit(9), 2));
        assert_eq!(ty.decode(&[0xff, 0x02]), Num::Int(1));
        assert_eq!(ty.encode(Num::Int(0), &[0xff, 0x03]).unwrap(), vec![0xff, 0x01]);
        assert_eq!(Type::Bit(1).encode(Num::Int(1), &[0x01]).unwrap(), vec![0x03]);
        assert!(Type::Bit(1).encode(Num::Int(2), &[0]).is_err());
        assert!(Type::parse("bit64").is_err());
    }
}
