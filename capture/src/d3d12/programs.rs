//! Raytracing state objects as the programs DispatchRays runs. A shader table's record is a shader identifier (32
//! bytes) and the local root arguments of the local root signature its export runs with: which one that is follows
//! the DXR spec's association rules ("Subobject association behavior"), from the state object's subobjects, those of
//! the collections and the state object it was made of (AddToStateObject), and those its DXIL libraries declare.
//! Capture reads the arguments to learn what a dispatch names (its root CBVs, SRVs, UAVs and descriptor tables), and
//! replay rewrites them, and the identifiers, to its own.

use std::sync::Arc;

use dxil_reader::rdat::{Subobject, export_name};
use serde_json::Value;

/// The bytes of a shader identifier, at the start of every shader record.
pub const IDENTIFIER: usize = 32;

// D3D12_STATE_SUBOBJECT_TYPE.
const LOCAL_ROOT_SIGNATURE: u64 = 2;
const DXIL_LIBRARY: u64 = 5;
const EXISTING_COLLECTION: u64 = 6;
const SUBOBJECT_TO_EXPORTS_ASSOCIATION: u64 = 7;
const DXIL_SUBOBJECT_TO_EXPORTS_ASSOCIATION: u64 = 8;
const HIT_GROUP: u64 = 11;

/// What a DXIL library declares, read from its container: the names of its shaders (`dx.entryPoints`) and the
/// subobjects of its runtime data (hit groups among them, which are exports too). An export may be named mangled
/// (`?gen@@YAXXZ`, as `dx.entryPoints` names it) or not (`gen`), so every function name here, as everywhere in
/// resolution, is unmangled.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Declared {
    pub shaders: Vec<String>,
    pub subobjects: Vec<Subobject>,
}

impl Declared {
    /// What the library `container` declares.
    pub fn read(container: &[u8]) -> Result<Declared, String> {
        let parts = dxil_reader::parts(container).map_err(|e| e.to_string())?;
        let subobjects = match parts.iter().find(|p| &p.kind == b"RDAT") {
            Some(p) => dxil_reader::rdat::subobjects(p.data).map_err(|e| e.to_string())?,
            None => Vec::new(),
        };
        let module = dxil_reader::module(&parts, b"DXIL")
            .map_err(|e| e.to_string())?
            .ok_or("the library has no DXIL part")?;
        let string = |id: Option<usize>| match id.and_then(|i| module.metadata.get(i)) {
            Some(dxil_reader::module::Metadata::String(s)) => Some(String::from_utf8_lossy(s).into_owned()),
            _ => None,
        };
        let mut shaders = Vec::new();
        for (name, nodes) in &module.named_metadata {
            if name != "dx.entryPoints" {
                continue;
            }
            // Each entry: its function, its name, signatures, resources, properties. A library's first names no
            // function: the library's own resources.
            for &node in nodes {
                let Some(operands) = module.metadata.get(node).and_then(|m| m.operands()) else {
                    continue;
                };
                if let Some(name) = string(operands.get(1).copied().flatten()).filter(|n| !n.is_empty()) {
                    shaders.push(export_name(&name));
                }
            }
        }
        // Function names, there and in what names them, compare unmangled.
        let subobjects = subobjects
            .into_iter()
            .map(|s| match s {
                Subobject::ExportsAssociation {
                    name,
                    subobject,
                    exports,
                } => Subobject::ExportsAssociation {
                    name,
                    subobject,
                    exports: exports.iter().map(|e| export_name(e)).collect(),
                },
                Subobject::HitGroup {
                    name,
                    kind,
                    any_hit,
                    closest_hit,
                    intersection,
                } => Subobject::HitGroup {
                    name,
                    kind,
                    any_hit: any_hit.as_deref().map(export_name),
                    closest_hit: closest_hit.as_deref().map(export_name),
                    intersection: intersection.as_deref().map(export_name),
                },
                s => s,
            })
            .collect();
        Ok(Declared { shaders, subobjects })
    }

    /// Every export the library has: its shaders and its hit groups.
    fn exports(&self) -> impl Iterator<Item = &str> {
        self.shaders
            .iter()
            .map(String::as_str)
            .chain(self.subobjects.iter().filter_map(|s| match s {
                Subobject::HitGroup { name, .. } => Some(name.as_str()),
                _ => None,
            }))
    }

    /// The serialized local root signature the library declares as `name`.
    fn local_root(&self, name: &str) -> Option<&[u8]> {
        self.subobjects.iter().find_map(|s| match s {
            Subobject::LocalRootSignature { name: n, signature } if n == name => Some(signature.as_slice()),
            _ => None,
        })
    }

    /// The library's own associations of a local root signature (by its name) to exports: the explicit ones (to
    /// `export`) and the default (to every export of the library), as the spec's two ways of declaring one go.
    fn associations(&self, export: &str) -> (Vec<String>, Vec<String>) {
        let (mut explicit, mut default) = (Vec::new(), Vec::new());
        let mut named = std::collections::BTreeSet::new();
        for s in &self.subobjects {
            if let Subobject::ExportsAssociation {
                subobject, exports, ..
            } = s
            {
                if self.local_root(subobject).is_none() {
                    continue;
                }
                if exports.is_empty() {
                    default.push(subobject.clone());
                } else {
                    named.insert(subobject.clone());
                    if exports.iter().any(|e| e == export) {
                        explicit.push(subobject.clone());
                    }
                }
            }
        }
        // A local root signature no explicit association of the library names is the library's default.
        for s in &self.subobjects {
            if let Subobject::LocalRootSignature { name, .. } = s
                && !named.contains(name)
            {
                default.push(name.clone());
            }
        }
        (explicit, default)
    }
}

/// A local root signature an export runs with: one the application made (its root signature object), or one a DXIL
/// library declares (as D3D serialized it).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Local {
    Object(u64),
    Declared(Vec<u8>),
}

/// Where the descriptions resolution reads come from: state objects' (and their collections') as capture describes
/// them, and what the DXIL libraries they hold declare (by the library's blob key).
pub trait Source {
    fn description(&self, id: u64) -> Option<Value>;
    fn declared(&self, blob: &str) -> Result<Arc<Declared>, String>;
}

/// The local root signature export `export` of state object `state_object` runs with, or None where it runs with
/// none (its records hold only the identifier). Fails where the state object holds no such export, or its
/// associations conflict (the runtime would have refused to make it).
pub fn local(source: &dyn Source, state_object: u64, export: &str) -> Result<Option<Local>, String> {
    match resolve(source, state_object, &export_name(export), 0)? {
        Some(found) => Ok(found),
        None => Err(format!("state object #{state_object} has no export {export:?}")),
    }
}

/// A state object's subobjects as capture describes them: each one's type and value.
struct Scope {
    id: u64,
    subobjects: Vec<(u64, Value)>,
    libraries: Vec<Library>,
    parent: u64,
}

/// A library of a state object: what it declares, and its exports' names in the state object and in the library.
struct Library {
    declared: Arc<Declared>,
    names: Vec<(String, String)>,
}

impl Library {
    /// `export`'s name in the library, where the library is where the state object takes it from.
    fn name_of(&self, export: &str) -> Option<&str> {
        self.names
            .iter()
            .find(|(n, _)| n == export)
            .map(|(_, l)| l.as_str())
    }
}

/// A state object's export list: (its name there, its name in what it comes from).
fn renames(exports: &Value) -> Vec<(String, String)> {
    exports
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|e| {
            let name = export_name(e["name"].as_str()?);
            let from = e["rename"].as_str().map_or_else(|| name.clone(), export_name);
            Some((name, from))
        })
        .collect()
}

impl Scope {
    fn read(source: &dyn Source, id: u64) -> Result<Scope, String> {
        let d = source
            .description(id)
            .ok_or_else(|| format!("state object #{id} is not described"))?;
        if d["kind"] != "state_object" {
            return Err(format!("#{id} is not a state object"));
        }
        let subobjects: Vec<(u64, Value)> = d["subobjects"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|s| (s["type"].as_u64().unwrap_or(u64::MAX), s["value"].clone()))
            .collect();
        let mut libraries = Vec::new();
        for (t, v) in &subobjects {
            if *t != DXIL_LIBRARY {
                continue;
            }
            let blob = v["library"]["blob"].as_str().ok_or_else(|| {
                format!("state object #{id}: a DXIL library's bytes are not in the capture")
            })?;
            let declared = source.declared(blob)?;
            let mut names = renames(&v["exports"]);
            if names.is_empty() {
                names = declared
                    .exports()
                    .map(|e| (e.to_string(), e.to_string()))
                    .collect();
            }
            libraries.push(Library { declared, names });
        }
        Ok(Scope {
            id,
            subobjects,
            libraries,
            parent: d["parent"].as_u64().unwrap_or(0),
        })
    }

    /// The local root signature subobject `index` of this state object is, where it is one.
    fn local_at(&self, index: &Value) -> Option<Local> {
        let (t, v) = self.subobjects.get(index.as_u64()? as usize)?;
        (*t == LOCAL_ROOT_SIGNATURE).then(|| Local::Object(v.as_u64().unwrap_or(0)))
    }

    /// The local root signature a library of this state object declares as `name`.
    fn declared_local(&self, name: &str) -> Option<Local> {
        self.libraries
            .iter()
            .find_map(|l| l.declared.local_root(name))
            .map(|s| Local::Declared(s.to_vec()))
    }

    /// The local root signatures this state object's own associations give `export`: the explicit ones, and the
    /// defaults (an association with no exports, or a local root signature no explicit association here names).
    fn associations(&self, export: &str) -> (Vec<Local>, Vec<Local>) {
        let (mut explicit, mut default) = (Vec::new(), Vec::new());
        let mut named = std::collections::BTreeSet::new();
        for (t, v) in &self.subobjects {
            let exports: Vec<String> = v["exports"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .map(export_name)
                .collect();
            let local = match *t {
                SUBOBJECT_TO_EXPORTS_ASSOCIATION => {
                    if !exports.is_empty()
                        && let Some(i) = v["subobject"].as_u64()
                    {
                        named.insert(i);
                    }
                    self.local_at(&v["subobject"])
                }
                DXIL_SUBOBJECT_TO_EXPORTS_ASSOCIATION => {
                    v["subobject"].as_str().and_then(|n| self.declared_local(n))
                }
                _ => continue,
            };
            let Some(local) = local else {
                continue;
            };
            if exports.is_empty() {
                default.push(local);
            } else if exports.iter().any(|e| e == export) {
                explicit.push(local);
            }
        }
        for (i, (t, v)) in self.subobjects.iter().enumerate() {
            if *t == LOCAL_ROOT_SIGNATURE && !named.contains(&(i as u64)) {
                default.push(Local::Object(v.as_u64().unwrap_or(0)));
            }
        }
        (explicit, default)
    }

    /// The one of `found`, where they agree; none where there are none.
    fn one(&self, export: &str, what: &str, mut found: Vec<Local>) -> Result<Option<Local>, String> {
        found.dedup();
        found.sort_by_key(|l| format!("{l:?}"));
        found.dedup();
        match found.len() {
            0 => Ok(None),
            1 => Ok(found.pop()),
            _ => Err(format!(
                "state object #{}: {export:?} has {} {what} local root signatures, which conflict",
                self.id,
                found.len()
            )),
        }
    }

    /// The local root signature this state object's associations give `export` (in `library`, under its name
    /// there, where it is a library's): its own explicit ones, then its defaults (which override any a DXIL library
    /// declares), then the libraries' explicit ones, then its library's default.
    fn associated(&self, export: &str, library: Option<(&Library, &str)>) -> Result<Option<Local>, String> {
        let (explicit, default) = self.associations(export);
        if let Some(l) = self.one(export, "explicit", explicit)? {
            return Ok(Some(l));
        }
        if let Some(l) = self.one(export, "default", default)? {
            return Ok(Some(l));
        }
        let Some((own, name)) = library else {
            return Ok(None);
        };
        // Explicit associations reach any export of the state object: a library's names it by the name it has
        // there (its own library's), or as the state object names it (another library's).
        let mut explicit = Vec::new();
        for l in &self.libraries {
            let here = if core::ptr::eq(l, own) { name } else { export };
            for n in l.declared.associations(here).0 {
                let declared = l.declared.local_root(&n).or_else(|| own.declared.local_root(&n));
                explicit.extend(declared.map(|s| Local::Declared(s.to_vec())));
            }
        }
        if let Some(l) = self.one(export, "explicit", explicit)? {
            return Ok(Some(l));
        }
        let default = own
            .declared
            .associations(name)
            .1
            .into_iter()
            .filter_map(|n| own.declared.local_root(&n).map(|s| Local::Declared(s.to_vec())))
            .collect();
        self.one(export, "default", default)
    }

    /// A hit group of this state object's own subobjects, or of its libraries' declarations, named `export`: its
    /// any-hit, closest-hit and intersection shaders, and the library declaring it.
    fn hit_group(&self, export: &str) -> Option<HitGroup<'_>> {
        for (t, v) in &self.subobjects {
            if *t == HIT_GROUP && v["name"].as_str().map(export_name).as_deref() == Some(export) {
                let shaders = ["any_hit", "closest_hit", "intersection"]
                    .iter()
                    .filter_map(|k| v[*k].as_str().map(export_name))
                    .collect();
                return Some(HitGroup {
                    shaders,
                    library: None,
                });
            }
        }
        for l in &self.libraries {
            let Some(name) = l.name_of(export) else {
                continue;
            };
            for s in &l.declared.subobjects {
                if let Subobject::HitGroup {
                    name: n,
                    any_hit,
                    closest_hit,
                    intersection,
                    ..
                } = s
                    && n == name
                {
                    let shaders = [any_hit, closest_hit, intersection]
                        .into_iter()
                        .flatten()
                        .cloned()
                        .collect();
                    return Some(HitGroup {
                        shaders,
                        library: Some((l, name.to_string())),
                    });
                }
            }
        }
        None
    }
}

/// A hit group's shaders (any-hit, closest-hit, intersection), and the library declaring it with its name there, where
/// a library does.
struct HitGroup<'a> {
    shaders: Vec<String>,
    library: Option<(&'a Library, String)>,
}

/// The most state objects deep (collections and the objects AddToStateObject grew from) resolution follows.
const DEEPEST: u32 = 64;

/// `export`'s local root signature in state object `id`: None where the state object has no such export, else what
/// it runs with.
fn resolve(source: &dyn Source, id: u64, export: &str, depth: u32) -> Result<Option<Option<Local>>, String> {
    if depth > DEEPEST {
        return Err(format!(
            "state object #{id}: more than {DEEPEST} state objects deep"
        ));
    }
    let scope = Scope::read(source, id)?;
    // A hit group runs with what it is associated with, or else with what its shaders are (which must agree).
    if let Some(HitGroup { shaders, library }) = scope.hit_group(export) {
        let library = library.as_ref().map(|(l, n)| (*l, n.as_str()));
        if let Some(l) = scope.associated(export, library)? {
            return Ok(Some(Some(l)));
        }
        let mut found = Vec::new();
        for s in &shaders {
            if let Some(l) = resolve(source, id, s, depth + 1)?.flatten() {
                found.push(l);
            }
        }
        return scope.one(export, "shader", found).map(Some);
    }
    for l in &scope.libraries {
        if let Some(name) = l.name_of(export) {
            return scope.associated(export, Some((l, name))).map(Some);
        }
    }
    // A collection keeps what it gave its exports; one it gave none takes this state object's.
    for (t, v) in &scope.subobjects {
        if *t != EXISTING_COLLECTION {
            continue;
        }
        let collection = v["collection"].as_u64().unwrap_or(0);
        let names = renames(&v["exports"]);
        let name = if names.is_empty() {
            Some(export.to_string())
        } else {
            names.iter().find(|(n, _)| n == export).map(|(_, c)| c.clone())
        };
        let Some(name) = name else {
            continue;
        };
        match resolve(source, collection, &name, depth + 1)? {
            Some(Some(l)) => return Ok(Some(Some(l))),
            Some(None) => return scope.associated(export, None).map(Some),
            None => continue,
        }
    }
    // What a state object grew from keeps its exports' associations: what AddToStateObject adds reaches only its own.
    if scope.parent != 0 {
        return resolve(source, scope.parent, export, depth + 1);
    }
    Ok(None)
}

/// What a local root argument is, by its parameter's type.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Kind {
    /// A descriptor table: a GPU descriptor handle.
    Table,
    /// Root constants: `words` 4-byte values.
    Constants {
        words: u32,
    },
    /// A root CBV, SRV or UAV: a GPU virtual address.
    ConstantBuffer,
    ShaderResource,
    UnorderedAccess,
}

/// A local root argument: its root parameter, where it is in a record (from the record's start), and what it is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Argument {
    pub parameter: u32,
    pub at: usize,
    pub kind: Kind,
}

/// Where the arguments of local root signature `decoded` (as capture decodes root signatures) are in a record, in
/// the signature's order after the identifier: constants aligned to 4 bytes, handles and addresses (8 bytes) to 8;
/// and the bytes they end at.
/// What [`arguments`] reads of a serialized root signature (the RTS0 part a DXIL library's subobject holds, or a
/// container of one, as `D3D12SerializeVersionedRootSignature` makes): its version and each parameter's type and
/// visibility, a root constants parameter with its register and count, a descriptor its register.
pub fn root_layout(serialized: &[u8]) -> Result<Value, String> {
    let rts0 = if serialized.starts_with(b"DXBC") {
        let parts = dxil_reader::parts(serialized).map_err(|e| e.to_string())?;
        parts
            .iter()
            .find(|p| &p.kind == b"RTS0")
            .map(|p| p.data)
            .ok_or("the container has no RTS0 part")?
    } else {
        serialized
    };
    let u = |at: usize| {
        rts0.get(at..at + 4)
            .map(|b| u32::from_le_bytes(b.try_into().expect("4 bytes")))
            .ok_or_else(|| format!("the root signature ends before byte {}", at + 4))
    };
    // D3D_ROOT_SIGNATURE_VERSION: 1.0, 1.1, 1.2.
    let version = u(0)?;
    if !(1..=3).contains(&version) {
        return Err(format!("root signature version {version:#x}"));
    }
    let (count, start) = (u(4)? as usize, u(8)? as usize);
    let mut parameters = Vec::with_capacity(count.min(64));
    for i in 0..count {
        let at = start + 12 * i;
        let (kind, visibility, payload) = (u(at)?, u(at + 4)?, u(at + 8)? as usize);
        let value = match kind {
            0 => serde_json::json!({"NumDescriptorRanges": u(payload)?}),
            1 => serde_json::json!({"ShaderRegister": u(payload)?, "RegisterSpace": u(payload + 4)?,
                "Num32BitValues": u(payload + 8)?}),
            2..=4 => serde_json::json!({"ShaderRegister": u(payload)?, "RegisterSpace": u(payload + 4)?}),
            k => return Err(format!("root parameter {i} of type {k}")),
        };
        parameters.push(serde_json::json!({"type": kind, "visibility": visibility, "value": value}));
    }
    Ok(serde_json::json!({"version": version, "parameters": parameters}))
}

pub fn arguments(decoded: &Value) -> Result<(Vec<Argument>, usize), String> {
    let mut at = IDENTIFIER;
    let mut out = Vec::new();
    for (i, p) in decoded["parameters"]
        .as_array()
        .ok_or("no parameters decoded")?
        .iter()
        .enumerate()
    {
        let (kind, bytes, align) = match p["type"].as_u64() {
            Some(0) => (Kind::Table, 8, 8),
            Some(1) => {
                let words = p["value"]["Num32BitValues"]
                    .as_u64()
                    .ok_or("root constants without a count")? as u32;
                (Kind::Constants { words }, 4 * words as usize, 4)
            }
            Some(2) => (Kind::ConstantBuffer, 8, 8),
            Some(3) => (Kind::ShaderResource, 8, 8),
            Some(4) => (Kind::UnorderedAccess, 8, 8),
            t => return Err(format!("root parameter {i} of type {t:?}")),
        };
        at = at.next_multiple_of(align);
        out.push(Argument {
            parameter: i as u32,
            at,
            kind,
        });
        at += bytes;
    }
    Ok((out, at))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::collections::HashMap;

    #[derive(Default)]
    struct Fake {
        objects: HashMap<u64, Value>,
        libraries: HashMap<String, Arc<Declared>>,
    }

    impl Source for Fake {
        fn description(&self, id: u64) -> Option<Value> {
            self.objects.get(&id).cloned()
        }
        fn declared(&self, blob: &str) -> Result<Arc<Declared>, String> {
            self.libraries
                .get(blob)
                .cloned()
                .ok_or_else(|| format!("no library {blob}"))
        }
    }

    fn library(blob: &str, exports: &[(&str, Option<&str>)]) -> Value {
        let exports: Vec<Value> = exports
            .iter()
            .map(|(n, r)| json!({"name": n, "rename": r, "flags": 0}))
            .collect();
        json!({"type": DXIL_LIBRARY, "value": {"library": {"blob": blob, "size": 1}, "exports": exports}})
    }

    fn so(subobjects: Vec<Value>, parent: u64) -> Value {
        json!({"kind": "state_object", "type": 3, "parent": parent, "subobjects": subobjects, "identifiers": {}})
    }

    fn local_rs(id: u64) -> Value {
        json!({"type": LOCAL_ROOT_SIGNATURE, "value": id})
    }

    fn association(index: Option<u64>, exports: &[&str]) -> Value {
        json!({"type": SUBOBJECT_TO_EXPORTS_ASSOCIATION, "value": {"subobject": index, "exports": exports}})
    }

    fn shaders(names: &[&str]) -> Arc<Declared> {
        Arc::new(Declared {
            shaders: names.iter().map(|s| s.to_string()).collect(),
            subobjects: Vec::new(),
        })
    }

    #[test]
    fn explicit_then_default_associations_of_the_state_object() {
        let mut f = Fake::default();
        f.libraries.insert("lib".into(), shaders(&["gen", "miss", "hit"]));
        // Subobject 1 (root #50) is associated with `gen` only; 2 (root #60) is named by nothing, so the default.
        f.objects.insert(
            1,
            so(
                vec![
                    library("lib", &[]),
                    local_rs(50),
                    local_rs(60),
                    association(Some(1), &["gen"]),
                ],
                0,
            ),
        );
        assert_eq!(local(&f, 1, "gen"), Ok(Some(Local::Object(50))));
        assert_eq!(local(&f, 1, "miss"), Ok(Some(Local::Object(60))));
        assert!(local(&f, 1, "nothing").is_err());
        // Two defaults conflict.
        f.objects
            .insert(2, so(vec![library("lib", &[]), local_rs(50), local_rs(60)], 0));
        assert!(local(&f, 2, "miss").unwrap_err().contains("conflict"));
        // An association with no exports is a default too, and so is the subobject it names.
        f.objects.insert(
            3,
            so(
                vec![library("lib", &[]), local_rs(70), association(Some(1), &[])],
                0,
            ),
        );
        assert_eq!(local(&f, 3, "hit"), Ok(Some(Local::Object(70))));
        // None at all: the export runs with no local root signature.
        f.objects.insert(4, so(vec![library("lib", &[])], 0));
        assert_eq!(local(&f, 4, "hit"), Ok(None));
    }

    #[test]
    fn renamed_exports_are_found_by_their_state_object_names() {
        let mut f = Fake::default();
        f.libraries.insert("lib".into(), shaders(&["gen"]));
        f.objects.insert(
            1,
            so(
                vec![
                    library("lib", &[("Gen2", Some("gen"))]),
                    local_rs(5),
                    association(Some(1), &["Gen2"]),
                ],
                0,
            ),
        );
        assert_eq!(local(&f, 1, "Gen2"), Ok(Some(Local::Object(5))));
        assert!(
            local(&f, 1, "gen").is_err(),
            "the library's name is not the state object's"
        );
    }

    #[test]
    fn a_state_objects_associations_override_a_librarys() {
        let mut f = Fake::default();
        let declared = Declared {
            shaders: vec!["gen".into(), "miss".into()],
            subobjects: vec![
                Subobject::LocalRootSignature {
                    name: "lrs".into(),
                    signature: vec![1, 2, 3],
                },
                Subobject::LocalRootSignature {
                    name: "other".into(),
                    signature: vec![4],
                },
                Subobject::ExportsAssociation {
                    name: "a".into(),
                    subobject: "lrs".into(),
                    exports: vec!["gen".into()],
                },
            ],
        };
        f.libraries.insert("lib".into(), Arc::new(declared));
        // The library alone: `gen` explicitly, `miss` by its default (`other`, which nothing names).
        f.objects.insert(1, so(vec![library("lib", &[])], 0));
        assert_eq!(local(&f, 1, "gen"), Ok(Some(Local::Declared(vec![1, 2, 3]))));
        assert_eq!(local(&f, 1, "miss"), Ok(Some(Local::Declared(vec![4]))));
        // A default of the state object's wins over the library's explicit association.
        f.objects.insert(2, so(vec![library("lib", &[]), local_rs(9)], 0));
        assert_eq!(local(&f, 2, "gen"), Ok(Some(Local::Object(9))));
        // A DXIL association at state object scope names the library's subobject.
        f.objects.insert(
            3,
            so(
                vec![
                    library("lib", &[]),
                    json!({"type": DXIL_SUBOBJECT_TO_EXPORTS_ASSOCIATION,
                        "value": {"subobject": "other", "exports": ["gen"]}}),
                ],
                0,
            ),
        );
        assert_eq!(local(&f, 3, "gen"), Ok(Some(Local::Declared(vec![4]))));
    }

    #[test]
    fn hit_groups_run_with_their_own_or_their_shaders_local_root_signature() {
        let mut f = Fake::default();
        f.libraries.insert("lib".into(), shaders(&["closest", "any"]));
        let group = json!({"type": HIT_GROUP, "value": {"name": "group", "type": 0, "any_hit": "any",
            "closest_hit": "closest", "intersection": null}});
        f.objects.insert(
            1,
            so(
                vec![
                    library("lib", &[]),
                    group.clone(),
                    local_rs(7),
                    association(Some(2), &["closest"]),
                ],
                0,
            ),
        );
        assert_eq!(local(&f, 1, "group"), Ok(Some(Local::Object(7))));
        f.objects.insert(
            2,
            so(
                vec![
                    library("lib", &[]),
                    group,
                    local_rs(7),
                    association(Some(2), &["group"]),
                ],
                0,
            ),
        );
        assert_eq!(local(&f, 2, "group"), Ok(Some(Local::Object(7))));
        assert_eq!(local(&f, 2, "closest"), Ok(None));
    }

    #[test]
    fn collections_and_parents_keep_their_own_associations() {
        let mut f = Fake::default();
        f.libraries.insert("a".into(), shaders(&["gen"]));
        f.libraries.insert("b".into(), shaders(&["miss", "free"]));
        f.objects.insert(10, so(vec![library("a", &[]), local_rs(1)], 0));
        // A collection without a local root signature for `free`: the pipeline's default reaches it.
        f.objects.insert(
            11,
            so(
                vec![library("b", &[]), local_rs(2), association(Some(1), &["miss"])],
                0,
            ),
        );
        let collection =
            |id: u64| json!({"type": EXISTING_COLLECTION, "value": {"collection": id, "exports": []}});
        f.objects
            .insert(20, so(vec![collection(10), collection(11), local_rs(3)], 0));
        assert_eq!(local(&f, 20, "gen"), Ok(Some(Local::Object(1))));
        assert_eq!(local(&f, 20, "miss"), Ok(Some(Local::Object(2))));
        assert_eq!(local(&f, 20, "free"), Ok(Some(Local::Object(3))));
        // What AddToStateObject adds does not reach what the state object grew from.
        f.libraries.insert("c".into(), shaders(&["added"]));
        f.objects.insert(30, so(vec![library("c", &[]), local_rs(4)], 20));
        assert_eq!(local(&f, 30, "added"), Ok(Some(Local::Object(4))));
        assert_eq!(local(&f, 30, "gen"), Ok(Some(Local::Object(1))));
        assert_eq!(local(&f, 30, "free"), Ok(Some(Local::Object(3))));
    }

    /// dxc 10.0.26100 `lib_6_3` of shaders `gen` (raygeneration), `miss` and `closest` (closesthit), with
    /// `LocalRootSignature lrs = { "RootConstants(num32BitConstants = 1, b0)" }`, `LocalRootSignature other = {
    /// "RootConstants(num32BitConstants = 2, b0), SRV(t1)" }`, `SubobjectToExportsAssociation gen_lrs = { "lrs",
    /// "gen;group" }` and `TriangleHitGroup group = { "", "closest" }`.
    const LIBRARY: &[u8] = include_bytes!("../testdata/dxil-library.cso");

    #[test]
    fn a_compiled_librarys_exports_and_associations() {
        let declared = Declared::read(LIBRARY).unwrap();
        // dx.entryPoints names them mangled (`\u{1}?gen@@YAXXZ`).
        let mut shaders = declared.shaders.clone();
        shaders.sort_unstable();
        assert_eq!(shaders, ["closest", "gen", "miss"]);
        let mut exports: Vec<&str> = declared.exports().collect();
        exports.sort_unstable();
        assert_eq!(exports, ["closest", "gen", "group", "miss"]);
        let lrs = declared.local_root("lrs").unwrap().to_vec();
        let lrs_bytes = lrs.clone();
        let other = declared.local_root("other").unwrap().to_vec();
        assert_ne!(lrs, other);
        let mut f = Fake::default();
        f.libraries.insert("lib".into(), Arc::new(declared));
        f.objects.insert(1, so(vec![library("lib", &[])], 0));
        // `gen` and `group` by the library's explicit association; `miss` and `closest` by its default, `other`,
        // which no explicit association names.
        assert_eq!(local(&f, 1, "gen"), Ok(Some(Local::Declared(lrs.clone()))));
        assert_eq!(local(&f, 1, "group"), Ok(Some(Local::Declared(lrs))));
        assert_eq!(local(&f, 1, "miss"), Ok(Some(Local::Declared(other.clone()))));
        assert_eq!(
            local(&f, 1, "\u{1}?miss@@YAXUPayload@@@Z"),
            Ok(Some(Local::Declared(other.clone())))
        );
        // `lrs`: one constant at b0; `other`: two at b0, then a root SRV at t1.
        let kinds = |serialized: &[u8]| {
            let (args, end) = arguments(&root_layout(serialized).unwrap()).unwrap();
            (args.iter().map(|a| (a.at, a.kind)).collect::<Vec<_>>(), end)
        };
        assert_eq!(kinds(&lrs_bytes), (vec![(32, Kind::Constants { words: 1 })], 36));
        assert_eq!(
            kinds(&other),
            (
                vec![(32, Kind::Constants { words: 2 }), (40, Kind::ShaderResource)],
                48
            )
        );
        let layout = root_layout(&other).unwrap();
        assert_eq!(layout["parameters"][1]["value"]["ShaderRegister"], 1);
        assert!(root_layout(&other[..10]).is_err());
    }

    #[test]
    fn arguments_pack_in_order_aligned_to_their_size() {
        let decoded = json!({"parameters": [
            {"type": 1, "value": {"Num32BitValues": 3}},
            {"type": 3, "value": {}},
            {"type": 1, "value": {"Num32BitValues": 1}},
            {"type": 0, "value": []},
            {"type": 4, "value": {}},
        ]});
        let (args, end) = arguments(&decoded).unwrap();
        let at: Vec<(usize, Kind)> = args.iter().map(|a| (a.at, a.kind)).collect();
        assert_eq!(
            at,
            vec![
                (32, Kind::Constants { words: 3 }),
                // 44 rounds up to 48.
                (48, Kind::ShaderResource),
                (56, Kind::Constants { words: 1 }),
                (64, Kind::Table),
                (72, Kind::UnorderedAccess),
            ]
        );
        assert_eq!(end, 80);
    }
}
