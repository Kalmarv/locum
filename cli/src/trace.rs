//! `trace` and `shader --trace`: what an output lane is made of. The program is lifted through iridia and its value
//! graph ([`Values`]) walked back from the lane to its sources: texture channels, constant-buffer words and input
//! lanes, each by the name its reflection gives it, and how it reaches the lane. At a draw, a pixel program's inputs
//! are followed into the vertex program by semantic, down to the vertex attributes, and each source is shown with
//! what the draw had: the bound resource, the constant's value, the input-layout element.
//!
//! The walk is a may-reach over-approximation, as iridia's own [`iridia::flow::Flow`] is: a constant that is zero
//! at run time still carries what it multiplies. Writes to storage buffers and images are not followed into later
//! reads of them.

use std::cmp::Reverse;
use std::collections::{BTreeMap, HashMap};

use iridia::flow::{At, Node, NodeId, Op, ScalarKind, Supplied, Values};
use iridia::{
    D3dBinding, D3dClass, Interface, Label, Program, ResourceId, ResourceKind, ScalarType, SystemValue,
    Varying,
};
use locum_capture::{format, rdef};
use serde_json::{Value, json};

use crate::describe::{self, short};
use crate::open::Capture;
use crate::out;

/// How a source reaches a lane, weakest first; a source reached several ways is shown with the strongest.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
enum How {
    /// Only through a condition: a branch's, a loop's exit, a select's.
    Control,
    /// Only as what decides which texel or element is read: a coordinate, level, offset or index.
    Address,
    /// The lane is computed from it.
    Value,
}

impl How {
    fn word(self) -> &'static str {
        match self {
            How::Control => "control",
            How::Address => "address",
            How::Value => "value",
        }
    }
}

/// Where a lane's value comes from.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Source {
    /// A lane of a texel a texture read returns.
    Texel {
        stage: usize,
        resource: Option<ResourceId>,
        lane: u8,
    },
    /// A word of a constant buffer, a buffer or the program's constant data, at a fixed place.
    Word {
        stage: usize,
        resource: Option<ResourceId>,
        word: u32,
    },
    /// A lane of an element of one chosen at run time.
    Indexed {
        stage: usize,
        resource: Option<ResourceId>,
        lane: u32,
    },
    /// A lane of an input no earlier program is followed into: a vertex attribute, a system value.
    Input { stage: usize, input: usize, lane: u8 },
    /// A pixel input lane the vertex program does not write.
    Unfed { stage: usize, input: usize, lane: u8 },
    /// A value the pipeline supplies that is not an input varying.
    Supplied { stage: usize, value: Supplied, lane: u8 },
    /// A resource's size, levels or samples.
    Query {
        stage: usize,
        resource: Option<ResourceId>,
    },
}

/// One program, lifted, with its reflection's constant-buffer names.
struct Stage {
    name: &'static str,
    hash: String,
    program: Program,
    values: Values,
    cbs: Vec<rdef::ConstantBuffer>,
    debug_name: Option<String>,
}

impl Stage {
    fn read(c: &Capture, hash: &str, name: &'static str) -> Result<Stage, String> {
        let key: Option<String> = c
            .row("SELECT blob FROM shaders WHERE hash = ?1", [hash], |r| r.get(0))
            .flatten();
        let key = key.ok_or_else(|| {
            format!(
                "{name} {}: its bytecode was not found when the capture was written",
                short(hash)
            )
        })?;
        Stage::of_code(&c.blob(&key)?, &c.names, hash, name)
    }

    fn of_code(
        code: &[u8],
        names: &crate::join::Names,
        hash: &str,
        name: &'static str,
    ) -> Result<Stage, String> {
        let program = crate::dxil::lift(code)
            .map_err(|e| format!("{name} {}: iridia cannot lift it: {e}", short(hash)))?;
        let values = Values::of_program(&program)
            .map_err(|e| format!("{name} {}: iridia cannot follow it: {e}", short(hash)))?;
        let reflected = crate::describe::reflected(names, code);
        Ok(Stage {
            name,
            hash: hash.to_string(),
            program,
            values,
            cbs: reflected.constant_buffers.unwrap_or_default(),
            debug_name: reflected.name,
        })
    }

    fn interface(&self) -> &Interface {
        &self.program.interface
    }

    fn title(&self) -> String {
        match &self.debug_name {
            Some(n) => format!("{} {} ({n})", self.name, short(&self.hash)),
            None => format!("{} {}", self.name, short(&self.hash)),
        }
    }

    /// A resource's D3D address and name.
    fn resource(&self, id: Option<ResourceId>) -> (Option<D3dBinding>, Option<&iridia::Resource>) {
        let r = id.and_then(|id| self.interface().resources.iter().find(|r| r.id == id));
        (r.and_then(|r| r.provenance.d3d), r)
    }
}

/// A binding's place: stage, kind (`cb`, `srv`), register and space.
type Slot = (String, String, u32, u32);

/// What a draw had bound in one place.
struct Bound {
    resource: Option<i64>,
    /// The key of a constant buffer's bytes as the GPU read them.
    content: Option<String>,
}

/// What a draw had: its bindings, constant bytes and input layout.
struct Draw {
    n: i64,
    bindings: HashMap<Slot, Bound>,
    layout: Vec<crate::vertices::Element>,
    bytes: HashMap<String, Vec<u8>>,
}

impl Draw {
    fn read(c: &Capture, n: i64, layout: Option<i64>) -> Draw {
        let bindings: Vec<(Slot, Bound)> = c.rows(
            "SELECT stage, kind, slot, space, resource, content FROM bindings WHERE draw = ?1",
            [n],
            |r| {
                Ok((
                    (r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?),
                    Bound {
                        resource: r.get(4)?,
                        content: r.get(5)?,
                    },
                ))
            },
        );
        Draw {
            n,
            bindings: bindings.into_iter().collect(),
            layout: crate::vertices::elements(c, layout),
            bytes: HashMap::new(),
        }
    }

    fn bound(&self, stage: &str, b: D3dBinding) -> Option<&Bound> {
        let kind = match b.class {
            D3dClass::ConstantBuffer => "cb",
            D3dClass::ShaderResource => "srv",
            D3dClass::UnorderedAccess => "uav",
            D3dClass::Sampler => "sampler",
        };
        self.bindings
            .get(&(stage.to_string(), kind.to_string(), b.register, b.space))
    }

    /// Word `word` of the constant buffer bound at `b`, as the GPU read it.
    fn word(&mut self, c: &Capture, stage: &str, b: D3dBinding, word: u32) -> Option<[u8; 4]> {
        let key = self.bound(stage, b)?.content.clone()?;
        let bytes = self.bytes.entry(key.clone()).or_insert_with(|| {
            c.row("SELECT bytes FROM cb_data WHERE hash = ?1", [&key], |r| r.get(0))
                .unwrap_or_default()
        });
        let at = word as usize * 4;
        bytes.get(at..at + 4).map(|w| [w[0], w[1], w[2], w[3]])
    }
}

/// What feeds input `input` lane `lane` of stage `s`: the earlier stage's output node, nothing (the earlier stage
/// does not write it), or no earlier stage followed (a vertex program's attributes, a pixel system value).
enum Fed {
    By(NodeId),
    Unwritten,
    No,
}

fn fed(stages: &[Stage], s: usize, input: usize, lane: u8) -> Fed {
    let (Some(from), Some(v)) = (
        s.checked_sub(1).and_then(|f| stages.get(f)),
        stages[s].interface().inputs.get(input),
    ) else {
        return Fed::No;
    };
    // The rasteriser makes SV_Position, the front-face flag and the sample index; the rest are the earlier stage's.
    if !matches!(
        v.system,
        None | Some(
            SystemValue::ClipDistance
                | SystemValue::CullDistance
                | SystemValue::RenderTargetArrayIndex
                | SystemValue::ViewportArrayIndex
        )
    ) {
        return Fed::No;
    }
    let Some(o) = from
        .interface()
        .outputs
        .iter()
        .position(|o| same(&o.label, &v.label))
    else {
        return Fed::Unwritten;
    };
    from.values.output(o, lane).map_or(Fed::Unwritten, Fed::By)
}

fn same(a: &Label, b: &Label) -> bool {
    a.index == b.index && a.name.eq_ignore_ascii_case(&b.name)
}

/// Every source that may reach `roots` of stage `start`, each with the strongest way it does.
fn reach(stages: &[Stage], start: usize, roots: &[NodeId], how: How) -> BTreeMap<Source, How> {
    let mut out: BTreeMap<Source, How> = BTreeMap::new();
    let mut best: HashMap<(usize, NodeId), How> = HashMap::new();
    let mut work: Vec<(usize, NodeId, How)> = Vec::new();
    let push =
        |best: &mut HashMap<(usize, NodeId), How>, work: &mut Vec<(usize, NodeId, How)>, s, id, how| {
            if best.get(&(s, id)).is_none_or(|b| *b < how) {
                best.insert((s, id), how);
                work.push((s, id, how));
            }
        };
    let found = |out: &mut BTreeMap<Source, How>, source: Source, how: How| {
        let e = out.entry(source).or_insert(how);
        *e = (*e).max(how);
    };
    for &r in roots {
        push(&mut best, &mut work, start, r, how);
    }
    while let Some((s, id, how)) = work.pop() {
        if best.get(&(s, id)).is_some_and(|b| *b > how) {
            continue;
        }
        let stage = s;
        match stages[s].values.node(id) {
            Node::Input { input, lane } => match fed(stages, s, *input, *lane) {
                Fed::By(node) => push(&mut best, &mut work, s - 1, node, how),
                Fed::Unwritten => found(
                    &mut out,
                    Source::Unfed {
                        stage,
                        input: *input,
                        lane: *lane,
                    },
                    how,
                ),
                Fed::No => found(
                    &mut out,
                    Source::Input {
                        stage,
                        input: *input,
                        lane: *lane,
                    },
                    how,
                ),
            },
            Node::Supplied { value, lane } => found(
                &mut out,
                Source::Supplied {
                    stage,
                    value: *value,
                    lane: *lane,
                },
                how,
            ),
            Node::Global { resource, at } => match at {
                At::Component(word) => found(
                    &mut out,
                    Source::Word {
                        stage,
                        resource: *resource,
                        word: *word,
                    },
                    how,
                ),
                At::Indexed { lane, index } => {
                    found(
                        &mut out,
                        Source::Indexed {
                            stage,
                            resource: *resource,
                            lane: *lane,
                        },
                        how,
                    );
                    for &i in index {
                        push(&mut best, &mut work, s, i, how.min(How::Address));
                    }
                }
            },
            Node::Texel {
                resource,
                lane,
                coordinate,
                controls,
                ..
            } => {
                found(
                    &mut out,
                    Source::Texel {
                        stage,
                        resource: *resource,
                        lane: *lane,
                    },
                    how,
                );
                for &c in coordinate.iter().chain(controls) {
                    push(&mut best, &mut work, s, c, how.min(How::Address));
                }
            }
            Node::Query { resource } => found(
                &mut out,
                Source::Query {
                    stage,
                    resource: *resource,
                },
                how,
            ),
            Node::Literal(_) => {}
            Node::Op { op, operands } => {
                for (i, &o) in operands.iter().enumerate() {
                    // A select's first operand is its condition.
                    let how = if matches!(op, Op::Select) && i == 0 {
                        how.min(How::Control)
                    } else {
                        how
                    };
                    push(&mut best, &mut work, s, o, how);
                }
            }
            Node::Merge { condition, arms }
            | Node::Loop {
                arms,
                exit: condition,
            } => {
                for &a in arms {
                    push(&mut best, &mut work, s, a, how);
                }
                for &c in condition {
                    push(&mut best, &mut work, s, c, how.min(How::Control));
                }
            }
        }
    }
    out
}

const XYZW: [char; 4] = ['x', 'y', 'z', 'w'];
const RGBA: [char; 4] = ['r', 'g', 'b', 'a'];

/// A label as HLSL writes it: `TEXCOORD3`, `SV_Target1`, `SV_Position`.
fn label(l: &Label) -> String {
    let system = l.name.len() > 3 && l.name[..3].eq_ignore_ascii_case("SV_");
    if l.index == 0 && system && !l.name.eq_ignore_ascii_case("SV_Target") {
        l.name.clone()
    } else {
        format!("{}{}", l.name, l.index)
    }
}

/// What a lane spec asks for.
#[derive(Debug)]
enum Ask {
    Lanes(Vec<(usize, u8)>),
    Discard,
}

/// The output lanes of `stage` a spec names: `SV_Target1.z`, `rt1` (every lane), `o1.z` (the DXBC register's lane),
/// `depth`, `TEXCOORD3.xy`, `discard`. `None` when the stage has no such output.
fn ask(stage: &Stage, spec: &str) -> Result<Option<Ask>, String> {
    if spec.eq_ignore_ascii_case("discard") {
        return Ok((stage.name == "ps").then_some(Ask::Discard));
    }
    let (name, lanes) = match spec.rsplit_once('.') {
        Some((n, l)) if !l.is_empty() && l.chars().all(|ch| "xyzwrgba".contains(ch.to_ascii_lowercase())) => {
            let lanes: Vec<u8> = l
                .chars()
                .map(|ch| "xyzw".find(ch).or_else(|| "rgba".find(ch)).unwrap_or(0) as u8)
                .collect();
            (n, Some(lanes))
        }
        _ => (spec, None),
    };
    let outputs = &stage.interface().outputs;
    let lower = name.to_ascii_lowercase();
    let every = |i: usize, v: &Varying| -> Vec<(usize, u8)> {
        match &lanes {
            Some(ls) => ls
                .iter()
                .filter(|&&l| l < v.value.width)
                .map(|&l| (i, l))
                .collect(),
            None => (0..v.value.width).map(|l| (i, l)).collect(),
        }
    };
    let picked: Vec<(usize, u8)> = if let Some(r) =
        lower.strip_prefix('o').and_then(|r| r.parse::<u32>().ok())
    {
        // A DXBC output register: its lanes are the register's, each varying in it has some of them.
        let mut picked = Vec::new();
        for (i, v) in outputs.iter().enumerate() {
            let Some(d) = v.provenance.d3d.filter(|d| d.register == r) else {
                continue;
            };
            let first = d.mask.trailing_zeros() as u8;
            for l in 0..v.value.width {
                if lanes.as_ref().is_none_or(|ls| ls.contains(&(first + l))) {
                    picked.push((i, l));
                }
            }
        }
        picked
    } else if lower == "depth" || lower == "odepth" {
        outputs
            .iter()
            .enumerate()
            .filter(|(_, v)| {
                matches!(
                    v.system,
                    Some(SystemValue::Depth | SystemValue::DepthGreaterEqual | SystemValue::DepthLessEqual)
                )
            })
            .flat_map(|(i, v)| every(i, v))
            .collect()
    } else if let Some(t) = lower.strip_prefix("rt").and_then(|t| t.parse::<u32>().ok()) {
        outputs
            .iter()
            .enumerate()
            .filter(|(_, v)| v.system == Some(SystemValue::Target(t)))
            .flat_map(|(i, v)| every(i, v))
            .collect()
    } else {
        let digits = name.len() - name.trim_end_matches(|ch: char| ch.is_ascii_digit()).len();
        let (base, index) = name.split_at(name.len() - digits);
        let wanted = Label {
            name: base.to_string(),
            index: index.parse().unwrap_or(0),
        };
        outputs
            .iter()
            .enumerate()
            .filter(|(_, v)| same(&v.label, &wanted))
            .flat_map(|(i, v)| every(i, v))
            .collect()
    };
    Ok((!picked.is_empty()).then_some(Ask::Lanes(picked)))
}

/// Lanes of an output as `SV_Target1.xy`, with the DXBC register's lanes (`o1.xy`) where it has them.
fn output_name(stage: &Stage, output: usize, lanes: &[u8]) -> (String, Option<String>) {
    let v = &stage.interface().outputs[output];
    let pick = |first: usize| -> String {
        lanes
            .iter()
            .map(|&l| XYZW[(first + usize::from(l)).min(3)])
            .collect()
    };
    let name = if v.value.width == 1 {
        label(&v.label)
    } else {
        format!("{}.{}", label(&v.label), pick(0))
    };
    let register = v
        .provenance
        .d3d
        .map(|d| format!("o{}.{}", d.register, pick(d.mask.trailing_zeros() as usize)));
    (name, register)
}

fn register(b: D3dBinding) -> String {
    let letter = match b.class {
        D3dClass::ConstantBuffer => 'b',
        D3dClass::ShaderResource => 't',
        D3dClass::UnorderedAccess => 'u',
        D3dClass::Sampler => 's',
    };
    if b.space == 0 {
        format!("{letter}{}", b.register)
    } else {
        format!("{letter}{} space{}", b.register, b.space)
    }
}

/// One source as text, split so the lanes of sources alike can be shown together (`TEXCOORD0.xy`), and as JSON.
struct Shown {
    prefix: String,
    lane: Option<char>,
    /// A constant's value at the draw, shown after the lanes.
    value: Option<String>,
    suffix: String,
    json: Value,
}

fn show(c: &Capture, stages: &[Stage], draw: &mut Option<Draw>, source: &Source) -> Shown {
    let plain = |prefix: String, lane: Option<char>, json: Value| Shown {
        prefix,
        lane,
        value: None,
        suffix: String::new(),
        json,
    };
    match *source {
        Source::Texel {
            stage,
            resource,
            lane,
        } => {
            let st = &stages[stage];
            let (b, r) = st.resource(resource);
            let name = r.map_or("?".to_string(), |r| r.provenance.name.clone());
            let at = b.map_or("an unbound texture".to_string(), register);
            let bound = b.and_then(|b| draw.as_ref()?.bound(st.name, b)?.resource);
            let suffix = bound.map_or(String::new(), |id| match describe::shape(c, id) {
                Some(s) => format!("  {s}"),
                None => format!("  #{id}"),
            });
            Shown {
                prefix: format!("{} texture {at} {name}", st.name),
                lane: Some(RGBA[usize::from(lane.min(3))]),
                value: None,
                suffix,
                json: json!({"kind":"texture","stage":st.name,"register":b.map(register),"name":name,
                    "channel":RGBA[usize::from(lane.min(3))].to_string(),"resource":bound}),
            }
        }
        Source::Word {
            stage,
            resource,
            word,
        } => {
            let st = &stages[stage];
            let (b, r) = st.resource(resource);
            let offset = word * 4;
            let cell = format!("c{}.{}", word / 4, XYZW[(word % 4) as usize]);
            match (b, r) {
                (Some(b), Some(r)) if matches!(r.kind, ResourceKind::ConstantBuffer { .. }) => {
                    let cb = st
                        .cbs
                        .iter()
                        .find(|cb| cb.slot == Some(b.register) && cb.space == b.space);
                    let named = cb.and_then(|cb| cb.at(offset));
                    let name = match (&named, cb) {
                        (Some((n, _)), _) => n.clone(),
                        (None, Some(cb)) => format!("{} +{offset}", cb.name),
                        (None, None) => format!("{} +{offset}", r.provenance.name),
                    };
                    let bytes = draw.as_mut().and_then(|d| d.word(c, st.name, b, word));
                    let base = named.as_ref().map(|n| n.1);
                    let json = json!({"kind":"constant","stage":st.name,"register":register(b),"offset":offset,"name":name,
                        "value":bytes.map(|w| crate::draw::number(w, base)),
                        "hex":bytes.map(|w| format!("{:08x}", u32::from_le_bytes(w)))});
                    // `v.x` to `v.w` of one register are shown together, as `c2 v.xyzw`.
                    let lane = XYZW[(word % 4) as usize];
                    let (prefix, lane) = match name.strip_suffix(&format!(".{lane}")) {
                        Some(stem) => (
                            format!("{} {} c{} {stem}", st.name, register(b), word / 4),
                            Some(lane),
                        ),
                        None => (format!("{} {} {cell} {name}", st.name, register(b)), None),
                    };
                    Shown {
                        prefix,
                        lane,
                        value: bytes.map(|w| describe::value(w, base)),
                        suffix: String::new(),
                        json,
                    }
                }
                (b, Some(r)) => plain(
                    format!(
                        "{} buffer {} {} word {word} (+{offset} bytes)",
                        st.name,
                        b.map_or("?".into(), register),
                        r.provenance.name
                    ),
                    None,
                    json!({"kind":"buffer","stage":st.name,"register":b.map(register),"name":r.provenance.name,"offset":offset}),
                ),
                (_, None) => plain(
                    format!("{} the program's constant data, word {word}", st.name),
                    None,
                    json!({"kind":"constant_data","stage":st.name,"offset":offset}),
                ),
            }
        }
        Source::Indexed {
            stage,
            resource,
            lane,
        } => {
            let st = &stages[stage];
            let (b, r) = st.resource(resource);
            let name = match (b, r) {
                (Some(b), Some(r)) if matches!(r.kind, ResourceKind::ConstantBuffer { .. }) => st
                    .cbs
                    .iter()
                    .find(|cb| cb.slot == Some(b.register) && cb.space == b.space)
                    .map_or(r.provenance.name.clone(), |cb| cb.name.clone()),
                (_, Some(r)) => r.provenance.name.clone(),
                (_, None) => "the program's constant data".into(),
            };
            let at = b.map_or(String::new(), |b| format!(" {}", register(b)));
            Shown {
                prefix: format!("{}{at} {name}[run-time index]", st.name),
                lane: Some(XYZW[(lane as usize).min(3)]),
                value: None,
                suffix: String::new(),
                json: json!({"kind":"indexed","stage":st.name,"register":b.map(register),"name":name,
                    "lane":XYZW[(lane as usize).min(3)].to_string()}),
            }
        }
        Source::Input { stage, input, lane } | Source::Unfed { stage, input, lane } => {
            let st = &stages[stage];
            let v = &st.interface().inputs[input];
            let unfed = matches!(source, Source::Unfed { .. });
            let element = (st.name == "vs" && v.system.is_none())
                .then(|| {
                    draw.as_ref()?
                        .layout
                        .iter()
                        .find(|e| e.index == v.label.index && e.semantic.eq_ignore_ascii_case(&v.label.name))
                })
                .flatten();
            let mut suffix = match element {
                Some(e) => format!(
                    "  vb{} +{} {}{}",
                    e.slot,
                    e.offset,
                    format::name(e.format),
                    if e.per_instance { " per instance" } else { "" }
                ),
                None => String::new(),
            };
            if unfed {
                let from = stages
                    .get(stage.wrapping_sub(1))
                    .map_or("the earlier stage", |s| s.name);
                suffix.push_str(&format!(", which {from} does not write"));
            }
            Shown {
                prefix: format!("{} input {}", st.name, label(&v.label)),
                lane: (v.value.width > 1).then(|| XYZW[usize::from(lane.min(3))]),
                json: json!({"kind":"input","stage":st.name,"semantic":label(&v.label),"lane":XYZW[usize::from(lane.min(3))].to_string(),
                    "unwritten_by_earlier_stage":unfed,
                    "element":element.map(|e| json!({"slot":e.slot,"offset":e.offset,"format":format::name(e.format),"per_instance":e.per_instance}))}),
                value: None,
                suffix,
            }
        }
        Source::Supplied { stage, value, lane } => {
            let st = &stages[stage];
            let name = match value {
                Supplied::OutputControlPoint => "SV_OutputControlPointID".to_string(),
                Supplied::Primitive => "SV_PrimitiveID".to_string(),
                Supplied::DomainLocation => "SV_DomainLocation".to_string(),
                Supplied::PatchConstant(i) => format!("patch constant {i}"),
            };
            Shown {
                prefix: format!("{} {name}", st.name),
                lane: Some(XYZW[usize::from(lane.min(3))]),
                value: None,
                suffix: String::new(),
                json: json!({"kind":"supplied","stage":st.name,"name":name,"lane":XYZW[usize::from(lane.min(3))].to_string()}),
            }
        }
        Source::Query { stage, resource } => {
            let st = &stages[stage];
            let (b, r) = st.resource(resource);
            let name = r.map_or("?".to_string(), |r| r.provenance.name.clone());
            plain(
                format!(
                    "{} the size of {} {name}",
                    st.name,
                    b.map_or("?".into(), register)
                ),
                None,
                json!({"kind":"query","stage":st.name,"register":b.map(register),"name":name}),
            )
        }
    }
}

/// One traced lane.
struct Traced {
    stage: usize,
    /// The output and lane; `None` for the discard.
    output: Option<(usize, u8)>,
    /// `SV_Target1.y`, or `discard`.
    name: String,
    register: Option<String>,
    /// Not written by the program.
    unwritten: bool,
    /// The lane is this constant.
    constant: Option<f64>,
    sources: BTreeMap<Source, How>,
}

fn traced(stages: &[Stage], s: usize, ask: &Ask) -> Vec<Traced> {
    let st = &stages[s];
    match ask {
        Ask::Discard => vec![Traced {
            stage: s,
            output: None,
            name: "discard".into(),
            register: None,
            unwritten: st.values.discards().is_empty(),
            constant: None,
            sources: reach(stages, s, &st.values.discards().concat(), How::Control),
        }],
        Ask::Lanes(lanes) => lanes
            .iter()
            .map(|&(o, l)| {
                let (name, register) = output_name(st, o, &[l]);
                let node = st.values.output(o, l);
                Traced {
                    stage: s,
                    output: Some((o, l)),
                    name,
                    register,
                    unwritten: node.is_none(),
                    constant: node.and_then(|n| constant(st, o, n)),
                    sources: node.map_or_else(BTreeMap::new, |n| reach(stages, s, &[n], How::Value)),
                }
            })
            .collect(),
    }
}

/// The constant output `output`'s node `n` is, as the output's type reads its bits: the lifters write every output
/// word through a bit cast, so a float output's `1.0` is the integer literal 0x3f800000.
fn constant(st: &Stage, output: usize, n: NodeId) -> Option<f64> {
    let Node::Literal(v) = st.values.node(n) else {
        return None;
    };
    let integer = matches!(st.values.kind(n), Some(ScalarKind::Uint | ScalarKind::Sint));
    let float = st.interface().outputs[output].value.scalar == ScalarType::Float;
    Some(if integer && float {
        f64::from(f32::from_bits(v.0 as i64 as u32))
    } else {
        v.0
    })
}

/// Every lane the last stage writes, and its discard when it has one.
fn every(stage: &Stage) -> Vec<Ask> {
    let mut asks = vec![Ask::Lanes(stage.values.outputs().keys().copied().collect())];
    if !stage.values.discards().is_empty() {
        asks.push(Ask::Discard);
    }
    asks
}

fn print(
    c: &Capture,
    stages: &[Stage],
    draw: &mut Option<Draw>,
    lanes: &[Traced],
    as_json: bool,
) -> Result<(), String> {
    if as_json {
        let rows: Vec<Value> = lanes
            .iter()
            .map(|t| {
                let sources: Vec<Value> = t
                    .sources
                    .iter()
                    .map(|(s, how)| {
                        let mut j = show(c, stages, draw, s).json;
                        j["how"] = how.word().into();
                        j
                    })
                    .collect();
                json!({"stage":stages[t.stage].name,"lane":t.name,"register":t.register,"written":!t.unwritten,
                    "constant":t.constant,"sources":sources})
            })
            .collect();
        let head = json!({
            "draw": draw.as_ref().map(|d| d.n),
            "shaders": stages.iter().map(|s| json!({"stage":s.name,"hash":s.hash,"name":s.debug_name})).collect::<Vec<_>>(),
            "lanes": rows,
        });
        out!("{}", serde_json::to_string(&head).map_err(|e| e.to_string())?);
        return Ok(());
    }
    // Lanes of one output made of the same sources the same ways are shown once, as `SV_Target0.xyz`.
    let mut merged: Vec<(&Traced, Vec<u8>)> = Vec::new();
    for t in lanes {
        if let (Some((last, ls)), Some((o, l))) = (merged.last_mut(), t.output)
            && last.stage == t.stage
            && last.output.is_some_and(|(lo, _)| lo == o)
            && last.unwritten == t.unwritten
            && last.constant == t.constant
            && last.sources == t.sources
        {
            ls.push(l);
            continue;
        }
        merged.push((t, t.output.map(|(_, l)| l).into_iter().collect()));
    }
    for (t, ls) in merged {
        let st = &stages[t.stage];
        let (name, register) = match t.output {
            Some((o, _)) => output_name(st, o, &ls),
            None => (t.name.clone(), t.register.clone()),
        };
        let t = &Traced {
            stage: t.stage,
            output: t.output,
            name,
            register,
            unwritten: t.unwritten,
            constant: t.constant,
            sources: t.sources.clone(),
        };
        let reg = t.register.as_ref().map_or(String::new(), |r| format!(" ({r})"));
        if t.unwritten {
            out!(
                "{} {}{reg}: {}",
                st.name,
                t.name,
                if t.name == "discard" {
                    "the program does not discard"
                } else {
                    "not written"
                }
            );
            continue;
        }
        if let (Some(v), true) = (t.constant, t.sources.is_empty()) {
            out!(
                "{} {}{reg}: the constant {}",
                st.name,
                t.name,
                describe::float(v as f32)
            );
            continue;
        }
        if t.sources.is_empty() {
            out!("{} {}{reg}: constants only", st.name, t.name);
            continue;
        }
        out!("{} {}{reg}:", st.name, t.name);
        // Sources alike but for their lane on one line, strongest way first, in the order the walk keeps them.
        type Group = (
            (Reverse<How>, usize),
            String,
            String,
            Vec<(Option<char>, Option<String>)>,
        );
        let mut groups: Vec<Group> = Vec::new();
        for (k, (s, how)) in t.sources.iter().enumerate() {
            let shown = show(c, stages, draw, s);
            let lane = (shown.lane, shown.value);
            match groups
                .iter_mut()
                .find(|g| g.0.0 == Reverse(*how) && g.1 == shown.prefix && g.2 == shown.suffix)
            {
                Some(g) if shown.lane.is_some() => g.3.push(lane),
                _ => groups.push(((Reverse(*how), k), shown.prefix, shown.suffix, vec![lane])),
            }
        }
        groups.sort_by_key(|g| g.0);
        for ((Reverse(how), _), prefix, suffix, mut lanes) in groups {
            lanes.sort_by_key(|(l, _)| l.and_then(|l| "xyzwrgba".find(l)));
            lanes.dedup_by_key(|(l, _)| *l);
            let names: String = lanes.iter().filter_map(|(l, _)| *l).collect();
            let dot = if names.is_empty() { "" } else { "." };
            let values: Vec<&str> = lanes.iter().filter_map(|(_, v)| v.as_deref()).collect();
            let values = match values.as_slice() {
                [] => String::new(),
                [v] => format!(" = {v}"),
                vs => format!(" = ({})", vs.join(", ")),
            };
            out!("  {:<8} {prefix}{dot}{names}{values}{suffix}", how.word());
        }
    }
    Ok(())
}

/// The lanes `specs` name among `stages`, each tried against the last stage first; none: every lane the last writes.
fn asks(stages: &[Stage], specs: &[String]) -> Result<Vec<(usize, Ask)>, String> {
    let last = stages.len() - 1;
    if specs.is_empty() {
        return Ok(every(&stages[last]).into_iter().map(|a| (last, a)).collect());
    }
    let mut out = Vec::new();
    for spec in specs {
        let mut hit = None;
        for s in (0..stages.len()).rev() {
            if let Some(a) = ask(&stages[s], spec)? {
                hit = Some((s, a));
                break;
            }
        }
        let Some(hit) = hit else {
            let outputs: Vec<String> = stages
                .iter()
                .map(|s| {
                    let names: Vec<String> = s.interface().outputs.iter().map(|v| label(&v.label)).collect();
                    format!("{}: {}", s.name, names.join(", "))
                })
                .collect();
            // A tessellated pipeline's position: the hull program passes control points on, and the domain (or a
            // geometry) program writes SV_Position.
            let position = spec
                .split('.')
                .next()
                .is_some_and(|n| n.eq_ignore_ascii_case("sv_position"));
            let later = stages.iter().any(|s| matches!(s.name, "ds" | "gs"));
            let note = if position && !later && stages.iter().any(|s| matches!(s.name, "vs" | "hs")) {
                "; in a draw with hull and domain programs the domain (or a geometry) program writes SV_Position: \
                 `trace DRAW SV_Position` follows the draw's stages back to the vertex attributes"
            } else {
                ""
            };
            return Err(format!(
                "no output lane {spec:?}; the outputs are {}{note}",
                outputs.join("; ")
            ));
        };
        out.push(hit);
    }
    Ok(out)
}

/// `trace`: lanes of what a draw's pixel (or vertex) program writes, followed back to the vertex attributes.
pub fn draw(c: &Capture, which: &str, specs: &[String], as_json: bool) -> Result<(), String> {
    let n = crate::replay::draw_number(c, which)?;
    type Row = (
        Option<String>,
        Option<String>,
        Option<String>,
        Option<String>,
        Option<String>,
        Option<String>,
        Option<i64>,
    );
    let (vs, hs, ds, gs, ps, cs, layout): Row = c
        .row(
            "SELECT vs, hs, ds, gs, ps, cs, layout FROM draws WHERE n = ?1",
            [n],
            |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                    r.get(6)?,
                ))
            },
        )
        .ok_or_else(|| format!("no draw {n}"))?;
    if cs.is_some() {
        return Err(format!(
            "draw {n} is a dispatch: a compute program has no output lanes (what it writes to UAVs is not traced)"
        ));
    }
    // Every program of the pipeline in order, each stage's inputs followed into the outputs of the one before by
    // their names: a domain program's control-point inputs into the hull program's control-point outputs, and those
    // into the vertex program's. A program that cannot be read ends the chain there: the stages after it are traced.
    let mut stages = Vec::new();
    let mut notes = Vec::new();
    let pipeline = [("vs", &vs), ("hs", &hs), ("ds", &ds), ("gs", &gs), ("ps", &ps)];
    for (name, hash) in pipeline {
        let Some(hash) = hash else { continue };
        match Stage::read(c, hash, name) {
            Ok(s) => stages.push(s),
            Err(e) if name != "ps" && ps.is_some() => {
                notes.push(format!(
                    "{e}: the inputs of the stage after it are not followed further"
                ));
                stages.clear();
            }
            Err(e) => return Err(e),
        }
    }
    if hs.is_some() && stages.iter().any(|s| s.name == "ds") {
        notes.push(
            "a domain program's patch constants (tessellation factors and the rest the hull program's patch \
             function writes) are shown as supplied, not followed into the hull program"
                .into(),
        );
    }
    if stages.is_empty() {
        return Err(format!("draw {n} has no program locum can trace"));
    }
    let wanted = asks(&stages, specs)?;
    let lanes: Vec<Traced> = wanted.iter().flat_map(|(s, a)| traced(&stages, *s, a)).collect();
    let mut draw = Some(Draw::read(c, n, layout));
    if !as_json {
        let titles: Vec<String> = stages.iter().map(Stage::title).collect();
        out!("draw {n}: {}", titles.join(", "));
        for note in &notes {
            out!("({note})");
        }
    }
    print(c, &stages, &mut draw, &lanes, as_json)?;
    if !as_json {
        let discards = stages
            .iter()
            .any(|s| s.name == "ps" && !s.values.discards().is_empty());
        if discards && !lanes.iter().any(|t| t.name == "discard") {
            out!("(the pixel program can discard: `discard` traces what decides it)");
        }
    }
    Ok(())
}

/// `shader --trace`: lanes of one program's outputs, to its own inputs.
pub fn program(c: &Capture, hash: &str, specs: &[String], as_json: bool) -> Result<(), String> {
    let (h, stage): (String, String) = c
        .row(
            "SELECT hash, stage FROM shaders WHERE hash LIKE ?1 || '%' ORDER BY hash LIMIT 1",
            [hash],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .ok_or_else(|| format!("no shader {hash}"))?;
    let name = match stage.as_str() {
        "vs" => "vs",
        "hs" => "hs",
        "ds" => "ds",
        "gs" => "gs",
        "ps" => "ps",
        _ => {
            return Err(format!(
                "{stage} {}: a compute program has no output lanes",
                short(&h)
            ));
        }
    };
    let stages = vec![Stage::read(c, &h, name)?];
    let wanted = asks(&stages, specs)?;
    let lanes: Vec<Traced> = wanted.iter().flat_map(|(s, a)| traced(&stages, *s, a)).collect();
    if !as_json {
        out!("{}", stages[0].title());
    }
    print(c, &stages, &mut None, &lanes, as_json)
}

#[cfg(test)]
mod tests {
    use super::*;

    const VS: &str = "cbuffer Scene : register(b0) { float4x4 viewProj; float4 tint; };
struct I { float3 pos : POSITION; float2 uv : TEXCOORD0; float4 col : COLOR1; };
struct O { float4 pos : SV_Position; float2 uv : TEXCOORD0; float shade : TEXCOORD1; };
O main(I i) { O o; o.pos = mul(float4(i.pos, 1), viewProj); o.uv = i.uv; o.shade = i.col.x * tint.y; return o; }";

    const PS: &str = "cbuffer Material : register(b2) { float4 scale; float alphaRef; };
Texture2D albedo : register(t1); SamplerState s : register(s0);
struct O { float4 c0 : SV_Target0; float4 c1 : SV_Target1; };
O main(float4 pos : SV_Position, float2 uv : TEXCOORD0, float shade : TEXCOORD1) {
  O o; float4 t = albedo.Sample(s, uv * scale.xy); if (t.a < alphaRef) discard;
  o.c0 = float4(t.rgb, 1); o.c1 = float4(shade, 0, 0, 0); return o; }";

    fn stages() -> Vec<Stage> {
        let vs = crate::shaders::compile(VS, c"vs_5_0").unwrap();
        let ps = crate::shaders::compile(PS, c"ps_5_0").unwrap();
        vec![
            Stage::of_code(&vs, &Default::default(), "vs", "vs").unwrap(),
            Stage::of_code(&ps, &Default::default(), "ps", "ps").unwrap(),
        ]
    }

    fn lanes(stages: &[Stage], spec: &str) -> Vec<Traced> {
        let asks = asks(stages, &[spec.to_string()]).unwrap();
        asks.iter().flat_map(|(s, a)| traced(stages, *s, a)).collect()
    }

    /// The vertex input a source is, by semantic and lane.
    fn input(stages: &[Stage], source: &Source) -> Option<(String, u8)> {
        let Source::Input { stage, input, lane } = *source else {
            return None;
        };
        Some((label(&stages[stage].interface().inputs[input].label), lane))
    }

    #[test]
    fn a_target_lane_comes_from_its_texel_channel_and_coordinate() {
        let st = stages();
        let albedo = st[1]
            .interface()
            .resources
            .iter()
            .find(|r| r.provenance.name == "albedo")
            .unwrap()
            .id;
        let t = lanes(&st, "rt0.y");
        assert_eq!(t.len(), 1);
        assert_eq!(
            (t[0].name.as_str(), t[0].register.as_deref()),
            ("SV_Target0.y", Some("o0.y"))
        );
        let s = &t[0].sources;
        assert_eq!(
            s.get(&Source::Texel {
                stage: 1,
                resource: Some(albedo),
                lane: 1
            }),
            Some(&How::Value)
        );
        assert!(!s.contains_key(&Source::Texel {
            stage: 1,
            resource: Some(albedo),
            lane: 0
        }));
        // The coordinate: scale.xy (Material b2, words 0 and 1) and the vertex program's TEXCOORD0 attribute.
        let material = st[1]
            .interface()
            .resources
            .iter()
            .find(|r| {
                r.provenance
                    .d3d
                    .is_some_and(|d| d.class == D3dClass::ConstantBuffer && d.register == 2)
            })
            .map(|r| r.id);
        for word in [0, 1] {
            assert_eq!(
                s.get(&Source::Word {
                    stage: 1,
                    resource: material,
                    word
                }),
                Some(&How::Address)
            );
        }
        let inputs: Vec<_> = s
            .iter()
            .filter_map(|(k, how)| Some((input(&st, k)?, *how)))
            .collect();
        assert_eq!(
            inputs,
            [
                (("TEXCOORD0".to_string(), 0), How::Address),
                (("TEXCOORD0".to_string(), 1), How::Address)
            ]
        );
        assert_eq!(lanes(&st, "SV_Target0.w")[0].constant, Some(1.0));
    }

    #[test]
    fn a_pixel_input_is_followed_into_the_vertex_program() {
        let st = stages();
        let t = lanes(&st, "o1.x");
        let scene = st[0]
            .interface()
            .resources
            .iter()
            .find(|r| {
                r.provenance
                    .d3d
                    .is_some_and(|d| d.class == D3dClass::ConstantBuffer && d.register == 0)
            })
            .map(|r| r.id);
        let s = &t[0].sources;
        // tint.y: the float4x4 takes bytes 0 to 63, so tint is word 16 and its y word 17.
        assert_eq!(
            s.get(&Source::Word {
                stage: 0,
                resource: scene,
                word: 17
            }),
            Some(&How::Value)
        );
        let inputs: Vec<_> = s
            .iter()
            .filter_map(|(k, how)| Some((input(&st, k)?, *how)))
            .collect();
        assert_eq!(inputs, [(("COLOR1".to_string(), 0), How::Value)]);
        assert_eq!(s.len(), 2, "{s:?}");
    }

    #[test]
    fn the_discard_is_decided_by_alpha_and_its_reference() {
        let st = stages();
        let t = lanes(&st, "discard");
        let s = &t[0].sources;
        assert!(
            s.iter()
                .any(|(k, h)| matches!(k, Source::Texel { lane: 3, .. }) && *h == How::Control)
        );
        assert!(s.iter().any(|(k, h)| matches!(
            k,
            Source::Word {
                stage: 1,
                word: 4,
                ..
            }
        ) && *h == How::Control));
        assert!(s.values().all(|h| *h != How::Value));
    }

    #[test]
    fn lane_specs() {
        let st = stages();
        assert_eq!(lanes(&st, "rt1").len(), 4);
        assert_eq!(lanes(&st, "SV_Target1.xy").len(), 2);
        // Not a pixel output: the vertex program's.
        let p = lanes(&st, "SV_Position.z");
        assert_eq!((p[0].stage, p[0].name.as_str()), (0, "SV_Position.z"));
        assert!(
            asks(&st, &["rt5".to_string()])
                .unwrap_err()
                .contains("SV_Target1")
        );
    }
}
