//! Game values: reading, setting and calling into the game by names a table defines, to put the game in the state a
//! capture needs (move the player, start an effect). locum knows no game: `table.txt` in its folder says where a
//! game's values and functions are (see [`table`]), the way a Cheat Engine table does, and a file named `do` asks for
//! something to be done, the way `capture` asks for a capture. locum takes `do` (removes it), runs its lines in order
//! on a thread of its own, stops at the first that fails, and writes what each did to `do.out` and the log:
//!
//! - `get NAME` / `set NAME VALUE`: a table value (set writes, then reads back);
//! - `ACTION [ARG ...]`: a table action, its arguments in order;
//! - `read TYPE ADDRESS`, `write TYPE ADDRESS VALUE`, `call ADDRESS [ARG ...]`: the same without a table entry;
//! - `check A OP B` (`==` `!=` `<` `<=` `>` `>=`): fails unless it holds, so a request can wait for a state;
//! - `wait MS`; `symbols`: every table symbol's address (and its offset in the executable).
//!
//! Arguments and values are expressions ([`expr`](locum_capture::values::expr)). Reads and writes touch only memory mapped for them, writes
//! never touch code, and calls go only to code in a loaded image; a called function that faults takes the game down.

mod memory;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Once;
use std::time::{Duration, SystemTime};

use locum_capture::values::expr::{Expr, Num, Scope};
use locum_capture::values::image;
use locum_capture::values::table::{self, Find, Step, Table, Type};

use crate::log::{folder, log};

/// Starts the thread that watches for `do` (once per process).
pub fn start() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let Some(dir) = folder() else { return };
        let spawned = std::thread::Builder::new()
            .name("locum-values".into())
            .spawn(move || watch(&dir));
        if let Err(e) = spawned {
            log!("values: no thread to watch for do: {e}");
        }
    });
}

fn watch(dir: &Path) {
    let mut tables = Tables::default();
    loop {
        std::thread::sleep(Duration::from_millis(200));
        let request = dir.join("do");
        let Ok(text) = std::fs::read_to_string(&request) else {
            continue;
        };
        let _ = std::fs::remove_file(&request);
        let out = run(&text, tables.get(dir));
        let _ = std::fs::write(dir.join("do.out"), &out);
        for line in out.lines() {
            log!("values: {line}");
        }
    }
}

/// `table.txt`, read again when it changes, with its symbols found once per version.
struct Tables {
    stamp: Option<(SystemTime, u64)>,
    loaded: Result<Loaded, String>,
}

impl Default for Tables {
    fn default() -> Self {
        Tables {
            stamp: None,
            loaded: Err("no table.txt".into()),
        }
    }
}

struct Loaded {
    table: Table,
    symbols: HashMap<String, Result<u64, String>>,
}

impl Tables {
    fn get(&mut self, dir: &Path) -> &Result<Loaded, String> {
        let path = dir.join("table.txt");
        let stamp = std::fs::metadata(&path)
            .ok()
            .map(|m| (m.modified().unwrap_or(SystemTime::UNIX_EPOCH), m.len()));
        if stamp != self.stamp || stamp.is_none() {
            self.stamp = stamp;
            self.loaded = load(&path);
            match &self.loaded {
                Ok(l) => log!(
                    "values: table.txt: {} symbols ({} found), {} values, {} actions",
                    l.table.symbols.len(),
                    l.symbols.values().filter(|s| s.is_ok()).count(),
                    l.table.values.len(),
                    l.table.actions.len()
                ),
                Err(e) => log!("values: {e}"),
            }
        }
        &self.loaded
    }
}

fn load(path: &PathBuf) -> Result<Loaded, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("table.txt: {e}"))?;
    let table = table::parse(&text)?;
    if let Some(want) = &table.exe_sha256 {
        let have = exe_sha256()?;
        if &have != want {
            return Err(format!(
                "table.txt is for the executable with sha256 {want}, this one's is {have}"
            ));
        }
    }
    let symbols = table
        .symbols
        .iter()
        .map(|(name, find)| {
            let found = resolve(find);
            if let Err(e) = &found {
                log!("values: symbol {name}: {e}");
            }
            (name.clone(), found)
        })
        .collect();
    Ok(Loaded { table, symbols })
}

fn exe_sha256() -> Result<String, String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let bytes = std::fs::read(&exe).map_err(|e| format!("{}: {e}", exe.display()))?;
    Ok(image::sha256(&bytes))
}

fn resolve(find: &Find) -> Result<u64, String> {
    image::resolve(&memory::code()?, find)
}

/// One request's state: the table (or why there is none), an action's parameters and buffers, and the last call's
/// result.
struct Run<'a> {
    loaded: &'a Result<Loaded, String>,
    params: HashMap<String, Num>,
    buffers: HashMap<String, Vec<u128>>,
    result: Option<u64>,
}

impl Scope for Run<'_> {
    fn name(&self, name: &str) -> Result<u64, String> {
        if let Some(b) = self.buffers.get(name) {
            return Ok(b.as_ptr() as u64);
        }
        if name == "result" {
            return self.result.ok_or_else(|| "no call has returned yet".into());
        }
        let loaded = self.table()?;
        if let Some(found) = loaded.symbols.get(name) {
            return found.clone().map_err(|e| format!("{name}: {e}"));
        }
        if let Some((_, _, address)) = loaded.table.values.iter().find(|v| v.0 == name) {
            return address.eval(self)?.int();
        }
        Err(format!("{name} is not in table.txt"))
    }

    fn param(&self, name: &str) -> Result<Num, String> {
        self.params
            .get(name)
            .copied()
            .ok_or_else(|| format!("${name} is not a parameter here"))
    }

    fn value(&self, name: &str) -> Result<Num, String> {
        let (ty, address) = self.table_value(name)?;
        memory::read(ty, address)
    }

    fn pointer(&self, address: u64) -> Result<u64, String> {
        memory::pointer(address)
    }
}

impl<'a> Run<'a> {
    /// The table, or the reason it did not load (missing, a mistake in it, another executable's).
    fn table(&self) -> Result<&'a Loaded, String> {
        self.loaded.as_ref().map_err(Clone::clone)
    }

    fn table_value(&self, name: &str) -> Result<(Type, u64), String> {
        let loaded = self.table()?;
        let (_, ty, address) = loaded
            .table
            .values
            .iter()
            .find(|v| v.0 == name)
            .ok_or_else(|| format!("{name} is not a value in table.txt"))?;
        Ok((*ty, address.eval(self)?.int()?))
    }

    fn eval(&self, text: &str) -> Result<Num, String> {
        Expr::parse(text)?.eval(self)
    }

    fn call(&mut self, function: u64, args: &[u64], out: &mut String) -> Result<(), String> {
        let r = memory::call(function, args)?;
        self.result = Some(r);
        out.push_str(&format!(
            "  call {function:#x}{}: returned {r:#x}\n",
            rva(function)
        ));
        Ok(())
    }

    fn action(&mut self, name: &str, args: &[&str], out: &mut String) -> Result<(), String> {
        let loaded = self.table()?;
        let (_, action) = loaded
            .table
            .actions
            .iter()
            .find(|a| a.0 == name)
            .ok_or_else(|| format!("{name:?} is neither a request nor an action in table.txt"))?;
        if args.len() != action.params.len() {
            return Err(format!(
                "{name} takes {} argument(s) ({}), given {}",
                action.params.len(),
                action.params.join(" "),
                args.len()
            ));
        }
        let values = args.iter().map(|a| self.eval(a)).collect::<Result<Vec<_>, _>>()?;
        let mut inner = Run {
            loaded: self.loaded,
            params: HashMap::new(),
            buffers: HashMap::new(),
            result: None,
        };
        inner.params = action.params.iter().cloned().zip(values).collect();
        for step in &action.steps {
            match step {
                Step::Buffer { name, size } => {
                    inner.buffers.insert(name.clone(), vec![0u128; size.div_ceil(16)]);
                }
                Step::Set { ty, address, value } => {
                    let a = address.eval(&inner)?.int()?;
                    memory::write(*ty, a, value.eval(&inner)?)?;
                }
                Step::Call { function, args } => {
                    let f = function.eval(&inner)?.int()?;
                    let a = args
                        .iter()
                        .map(|e| e.eval(&inner).and_then(Num::int))
                        .collect::<Result<Vec<_>, _>>()?;
                    inner.call(f, &a, out)?;
                }
                Step::Wait { ms } => std::thread::sleep(Duration::from_millis(*ms)),
            }
        }
        self.result = inner.result;
        Ok(())
    }

    fn line(&mut self, line: &str, out: &mut String) -> Result<(), String> {
        let (verb, rest) = line.split_once(char::is_whitespace).unwrap_or((line, ""));
        let rest = rest.trim();
        match verb {
            "get" => {
                let (ty, address) = self.table_value(rest)?;
                out.push_str(&format!(
                    "  {rest} = {} at {address:#x}\n",
                    memory::read(ty, address)?
                ));
            }
            "set" => {
                let (name, value) = rest.split_once(char::is_whitespace).ok_or("set NAME VALUE")?;
                let (ty, address) = self.table_value(name)?;
                let before = memory::read(ty, address)?;
                let after = memory::write(ty, address, self.eval(value)?)?;
                out.push_str(&format!("  {name} = {after} (was {before})\n"));
            }
            "read" => {
                let (ty, address) = rest.split_once(char::is_whitespace).ok_or("read TYPE ADDRESS")?;
                let a = self.eval(address)?.int()?;
                out.push_str(&format!("  {} at {a:#x}\n", memory::read(Type::parse(ty)?, a)?));
            }
            "write" => {
                let (ty, rest) = rest
                    .split_once(char::is_whitespace)
                    .ok_or("write TYPE ADDRESS VALUE")?;
                let (address, value) = table::split_last(rest).ok_or("write TYPE ADDRESS VALUE")?;
                let a = self.eval(address)?.int()?;
                let after = memory::write(Type::parse(ty)?, a, self.eval(value)?)?;
                out.push_str(&format!("  {after} at {a:#x}\n"));
            }
            "call" => {
                let mut words = rest.split_whitespace();
                let f = self.eval(words.next().ok_or("call ADDRESS [ARG ...]")?)?.int()?;
                let args = words
                    .map(|w| self.eval(w).and_then(Num::int))
                    .collect::<Result<Vec<_>, _>>()?;
                if args.len() > table::MAX_ARGS {
                    return Err(format!("call takes at most {} arguments", table::MAX_ARGS));
                }
                self.call(f, &args, out)?;
            }
            "wait" => std::thread::sleep(Duration::from_millis(self.eval(rest)?.int()?)),
            "check" => {
                let (a, op, b) = check_parts(rest).ok_or("check A OP B, OP one of == != < <= > >=")?;
                let (x, y) = (self.eval(a)?, self.eval(b)?);
                if !compare(x, op, y) {
                    return Err(format!("{a} is {x}: not {op} {y}"));
                }
                out.push_str(&format!("  {a} is {x}: {op} {y}\n"));
            }
            "symbols" => {
                let loaded = self.table()?;
                for (name, _) in &loaded.table.symbols {
                    match &loaded.symbols[name] {
                        Ok(a) => out.push_str(&format!("  {name} = {a:#x}{}\n", rva(*a))),
                        Err(e) => out.push_str(&format!("  {name}: {e}\n")),
                    }
                }
            }
            action => {
                let args: Vec<&str> = rest.split_whitespace().collect();
                self.action(action, &args, out)?;
            }
        }
        Ok(())
    }
}

const OPERATORS: [&str; 6] = ["==", "!=", "<=", ">=", "<", ">"];

/// `A OP B` of a `check`, the operator standing alone between spaces.
fn check_parts(text: &str) -> Option<(&str, &'static str, &str)> {
    OPERATORS.iter().find_map(|op| {
        let (a, b) = text.split_once(&format!(" {op} "))?;
        Some((a.trim(), *op, b.trim()))
    })
}

/// Integers compare as signed (a negative `i32` read is below zero), anything with a float as floats.
fn compare(x: Num, op: &str, y: Num) -> bool {
    let order = match (x, y) {
        (Num::Int(a), Num::Int(b)) => (a as i64).cmp(&(b as i64)),
        _ => match x.float().partial_cmp(&y.float()) {
            Some(o) => o,
            None => return op == "!=",
        },
    };
    match op {
        "==" => order.is_eq(),
        "!=" => order.is_ne(),
        "<" => order.is_lt(),
        "<=" => order.is_le(),
        ">" => order.is_gt(),
        _ => order.is_ge(),
    }
}

/// ` (exe+RVA)` for an address inside the executable's image.
fn rva(address: u64) -> String {
    match memory::exe_image() {
        Ok((module, size)) if address >= module && address - module < size => {
            format!(" (exe+{:#x})", address - module)
        }
        _ => String::new(),
    }
}

/// Runs a request's lines; what each did, as text.
fn run(text: &str, loaded: &Result<Loaded, String>) -> String {
    let mut out = String::new();
    let mut r = Run {
        loaded,
        params: HashMap::new(),
        buffers: HashMap::new(),
        result: None,
    };
    for line in text.lines() {
        let line = line.split('#').next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        out.push_str(&format!("> {line}\n"));
        if let Err(e) = r.line(line, &mut out) {
            out.push_str(&format!("  error: {e}\n  (the rest of the request is not run)\n"));
            break;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A function the tests call by address, as a game function would be.
    extern "system" fn add_into(out: *mut u32, a: u64, b: u64) -> u64 {
        unsafe { *out = (a + b) as u32 };
        a * b
    }

    static mut GLOBAL: [f32; 4] = [1.0, 2.0, 3.0, 4.0];

    fn loaded(text: &str) -> Loaded {
        let table = table::parse(text).unwrap();
        let symbols = table
            .symbols
            .iter()
            .map(|(n, f)| (n.clone(), resolve(f)))
            .collect();
        Loaded { table, symbols }
    }

    #[test]
    fn requests_read_write_and_call_by_table_names() {
        let global = &raw mut GLOBAL as u64;
        let f = add_into as *const () as u64;
        let table = format!(
            "value g.y f32 = {global:#x}+4\n\
             action add a b\n buffer o 16\n call {f:#x} o $a $b\n set u32 {global:#x}+8 result+[o]-13\nend\n"
        );
        let l = Ok(loaded(&table));
        let out = run("get g.y\nset g.y {g.y}+0.5\nadd 6 7\nread u32 0x10\n", &l);
        assert!(out.contains("g.y = 2 at"), "{out}");
        assert!(out.contains("g.y = 2.5 (was 2)"), "{out}");
        assert!(out.contains("returned 0x2a"), "{out}");
        assert!(out.contains("not mapped"), "{out}");
        // the action wrote the call's result (42) plus the sum it left in the buffer (13), less 13, over the third
        // float's bits
        assert_eq!(unsafe { (&raw const GLOBAL).read()[2].to_bits() }, 42);
    }

    #[test]
    fn a_failing_line_stops_the_request() {
        let l = Ok(loaded("value v u8 = 0x10\n"));
        let out = run("get v\nwait 1\n", &l);
        assert!(out.contains("not mapped"), "{out}");
        assert!(!out.contains("> wait"), "{out}");
        let out = run("nonesuch 1\n", &l);
        assert!(out.contains("neither a request nor an action"), "{out}");
    }

    #[test]
    fn checks_fail_unless_they_hold() {
        let l = Ok(loaded(""));
        let out = run("check 0xffffffff != 4294967295\n", &l);
        assert!(
            out.contains("error: 0xffffffff is 4294967295 (0xffffffff): not != 4294967295"),
            "{out}"
        );
        let out = run("check -1 < 0\ncheck 2.5 >= 2\ncheck 3 == 1+2\ncheck 1 <> 2\n", &l);
        assert!(out.contains("-1 is -1 (0xffffffffffffffff): < 0"), "{out}");
        assert!(out.contains("2.5 is 2.5: >= 2"), "{out}");
        assert!(out.contains("3 is 3 (0x3): == 3"), "{out}");
        assert!(out.contains("error: check A OP B"), "{out}");
    }

    #[test]
    fn a_table_that_did_not_load_says_why() {
        let l = Err("table.txt line 35: missing ]".to_string());
        let out = run("get v\n", &l);
        assert!(out.contains("error: table.txt line 35: missing ]"), "{out}");
    }

    #[test]
    fn symbols_are_found_in_the_executable() {
        // the first 16 bytes of this test function's code are in the test executable once (or more: then refused)
        let f = add_into as *const () as u64;
        let bytes = memory::read_bytes(f, 16).unwrap();
        let p: String = bytes.iter().map(|b| format!("{b:02x} ")).collect();
        let l = loaded(&format!("code here = {p}+2\n"));
        match &l.symbols["here"] {
            Ok(a) => assert_eq!(*a, f + 2),
            Err(e) => assert!(e.contains("places, not one"), "{e}"),
        }
    }
}
