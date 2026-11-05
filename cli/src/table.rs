//! `table`: a game-values table (`table.txt`) checked against the executable it is for, on disk, before a game is
//! started for it: that it parses (every name it uses defined), that it is that executable's, and where each of its
//! symbols lands, as locum would find them in the running game.

use std::path::Path;

use locum_capture::values::image::{self, Section};
use locum_capture::values::table;

use crate::out;

/// The section holding an offset in the image, and whether it is code.
fn section_of(sections: &[Section], rva: u64) -> String {
    sections
        .iter()
        .find(|s| rva >= s.rva as u64 && rva < s.rva as u64 + s.size.max(s.file_size) as u64)
        .map_or("outside every section".into(), |s| {
            format!("{}, {}", s.name, if s.executes() { "code" } else { "data" })
        })
}

pub fn table(exe: &Path, table_path: &Path) -> Result<(), String> {
    let text = std::fs::read_to_string(table_path).map_err(|e| format!("{}: {e}", table_path.display()))?;
    let t = table::parse(&text)?;
    let file = std::fs::read(exe).map_err(|e| format!("{}: {e}", exe.display()))?;
    let mut problems = 0;
    let sha = image::sha256(&file);
    match &t.exe_sha256 {
        Some(want) if *want == sha => out!("exe sha256 {sha}: the table's"),
        Some(want) => {
            problems += 1;
            out!("exe sha256 {sha}: not the table's ({want}); locum would refuse it");
        }
        None => out!("exe sha256 {sha} (the table names no executable)"),
    }
    let disk = image::on_disk(&file)?;
    let (code, sections) = (disk.code(), &disk.sections);
    for (name, find) in &t.symbols {
        match image::resolve(&code, find) {
            Ok(rva) => out!("{name} = exe+{rva:#x} ({})", section_of(sections, rva)),
            Err(e) => {
                problems += 1;
                out!("{name}: {e}");
            }
        }
    }
    let values: Vec<&str> = t.values.iter().map(|v| v.0.as_str()).collect();
    let actions: Vec<String> = t
        .actions
        .iter()
        .map(|(n, a)| {
            std::iter::once(n.as_str())
                .chain(a.params.iter().map(String::as_str))
                .collect::<Vec<_>>()
                .join(" ")
        })
        .collect();
    out!("values: {}", values.join(", "));
    out!("actions: {}", actions.join(", "));
    match problems {
        0 => Ok(()),
        n => Err(format!("{}: {n} problem(s)", table_path.display())),
    }
}
