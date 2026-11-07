//! The slim shader set (`oracles.corpus.general/slim`) through what locum reads of the programs it captures and
//! replays: every DXBC and DXIL program's container parses and its reflection reads, as the replay and the in-game
//! output tracking read them, and a root signature a container holds reads. The set's Direct3D 9 programs are counted
//! and not read: locum wraps Direct3D 11 and 12, whose programs are DXBC and DXIL. Skips where the set is not
//! configured.
mod inputs;

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use inputs::{Game, OracleSet};
use locum_capture::d3d12::programs::{Declared, root_layout};
use locum_capture::{dxbc, rdef};

/// The set's programs by family, as primase-testdata records it (0.1.17 on).
const PROGRAMS: [(&str, usize); 3] = [("d3d9", 557), ("dxbc", 3_670), ("dxil", 4_059)];

/// What locum read of a program: where its reflection came from (`RDEF`, its declarations when it has none, the
/// `STAT` or `DXIL` module), or `root signature alone` for a container holding nothing else; an error names the
/// part it could not read.
fn read(family: &str, code: &[u8]) -> Result<&'static str, String> {
    let how = match family {
        "dxbc" => {
            let parts = dxbc_reader::parts(code).map_err(|e| format!("container: {e}"))?;
            if parts.iter().any(|p| &p.kind == b"RDEF") {
                dxbc_reader::rdef(code)
                    .map_err(|e| format!("RDEF: {e}"))?
                    .ok_or("RDEF: dx reads none")?;
                rdef::constant_buffers(code).ok_or("no constant buffers from RDEF")?;
                "RDEF"
            } else {
                let words = dxbc_reader::code(code).map_err(|e| format!("code: {e}"))?;
                dxbc_reader::decode(&words).map_err(|e| format!("decode: {e}"))?;
                rdef::declared(code).ok_or("no declarations")?;
                "declarations"
            }
        }
        "dxil" => {
            let parts = dxil_reader::parts(code).map_err(|e| format!("container: {e}"))?;
            if !parts.iter().any(|p| &p.kind == b"DXIL") {
                if !parts.iter().any(|p| &p.kind == b"RTS0") {
                    return Err("neither a program nor a root signature".into());
                }
                root_layout(code).map_err(|e| format!("root signature: {e}"))?;
                if rdef::reflects(code) {
                    return Err("a root signature alone reflects a program".into());
                }
                return Ok("root signature alone");
            }
            let kind = if parts.iter().any(|p| &p.kind == b"STAT") {
                b"STAT"
            } else {
                b"DXIL"
            };
            let what = String::from_utf8_lossy(kind);
            let module = dxil_reader::module(&parts, kind)
                .map_err(|e| format!("{what} module: {e}"))?
                .ok_or_else(|| format!("no {what} module"))?;
            dxil_reader::reflection::ranges(&module).map_err(|e| format!("{what} resources: {e}"))?;
            rdef::constant_buffers(code).ok_or("no constant buffers from the module")?;
            if parts.iter().any(|p| &p.kind == b"RDAT") {
                Declared::read(code).map_err(|e| format!("library: {e}"))?;
            }
            if kind == b"STAT" { "STAT" } else { "DXIL" }
        }
        _ => unreachable!("{family}"),
    };
    rdef::bindings(code).ok_or("no bindings")?;
    rdef::uav_registers(code).ok_or("no UAV registers")?;
    rdef::debug_name(code);
    dxbc::order_dependent(code).map_err(|e| format!("decode: {e}"))?;
    if dxil_reader::parts(code).is_ok_and(|parts| parts.iter().any(|p| &p.kind == b"RTS0")) {
        root_layout(code).map_err(|e| format!("root signature: {e}"))?;
    }
    Ok(how)
}

#[test]
fn slim_programs_parse_and_reflect() {
    let Some(tree) = inputs::oracle_tree(OracleSet::Corpus, Game::General, "slim") else {
        return;
    };
    let programs: Vec<(&str, &str)> = tree
        .names()
        .into_iter()
        .filter_map(|name| {
            let (family, file) = name.strip_prefix("programs/")?.split_once('/')?;
            file.strip_suffix(&format!(".{family}")).map(|_| (family, name))
        })
        .collect();
    // The programs Microsoft's own tools refuse: the only ones locum may refuse too.
    let refused =
        String::from_utf8(tree.read("microsoft-refused.tsv").expect("microsoft-refused.tsv")).unwrap();
    let refused: BTreeSet<&str> = refused
        .lines()
        .skip(1)
        .filter_map(|l| l.split('\t').next())
        .collect();

    let counts = Mutex::new(BTreeMap::<(&str, &str), usize>::new());
    let failed = Mutex::new(Vec::new());
    let next = AtomicUsize::new(0);
    // Reading the set's files over a network share is most of the time: a few threads hide the latency.
    std::thread::scope(|s| {
        for _ in 0..16 {
            s.spawn(|| {
                while let Some(&(family, name)) = programs.get(next.fetch_add(1, Ordering::Relaxed)) {
                    let outcome = if family == "d3d9" {
                        "not read"
                    } else {
                        let code = tree.read(name).unwrap_or_else(|e| panic!("{name}: {e}"));
                        read(family, &code).unwrap_or_else(|e| {
                            failed.lock().unwrap().push((name, e));
                            "refused"
                        })
                    };
                    *counts.lock().unwrap().entry((family, outcome)).or_default() += 1;
                }
            });
        }
    });
    let counts = counts.into_inner().unwrap();
    let mut failed = failed.into_inner().unwrap();
    failed.sort();
    for ((family, outcome), n) in &counts {
        println!("{family} {outcome}: {n}");
    }
    for (name, why) in &failed {
        println!("refused {name}: {why}");
    }

    for (family, expected) in PROGRAMS {
        let found: usize = counts
            .iter()
            .filter(|((f, _), _)| *f == family)
            .map(|(_, n)| n)
            .sum();
        assert_eq!(found, expected, "{family} programs in the set");
    }
    let unexpected: Vec<_> = failed
        .iter()
        .filter(|(name, _)| !refused.contains(name))
        .collect();
    assert!(
        unexpected.is_empty(),
        "{} programs Microsoft's tools read and locum does not: {unexpected:#?}",
        unexpected.len()
    );
}
