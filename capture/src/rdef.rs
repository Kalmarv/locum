//! Shader reflection from a DXBC container's `RDEF` part, as dx's `dxbc-reader` reads it, or a DXIL container's
//! reflection module (`STAT`, else the program's own `DXIL`) as `dxil-reader` reads it: each constant buffer's name,
//! register and variables, so a constant can be shown as `cbTexScroll.uvScale.x` instead of `b11 c0.x`, and the name
//! and register of every resource the program reads or writes (`g_GBuffer2Texture` at t4).

#[derive(Clone, Debug, PartialEq)]
pub struct Type {
    /// D3D_SHADER_VARIABLE_CLASS: 0 scalar, 1 vector, 2 row-major matrix, 3 column-major matrix, 4 object, 5 struct.
    pub class: u16,
    /// D3D_SHADER_VARIABLE_TYPE: 1 bool, 2 int, 3 float, 19 uint, ...
    pub base: u16,
    pub rows: u16,
    pub columns: u16,
    /// Array length, 0 when not an array.
    pub elements: u16,
    pub members: Vec<Member>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Member {
    pub name: String,
    pub offset: u32,
    pub ty: Type,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Variable {
    pub name: String,
    pub offset: u32,
    pub size: u32,
    /// Whether the shader reads it.
    pub used: bool,
    pub ty: Type,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ConstantBuffer {
    pub name: String,
    /// The register (`b<slot>`), when the shader binds it.
    pub slot: Option<u32>,
    pub space: u32,
    pub size: u32,
    pub variables: Vec<Variable>,
}

impl Type {
    /// HLSL spelling, like `float4`, `float4x4` or `uint`.
    pub fn name(&self) -> String {
        let base = match self.base {
            0 => "void",
            1 => "bool",
            2 => "int",
            3 => "float",
            19 => "uint",
            20 => "uint8",
            39 => "double",
            54 => "min8float",
            55 => "min10float",
            56 => "min16float",
            57 => "min12int",
            58 => "min16int",
            59 => "min16uint",
            _ => "?",
        };
        let shape = match self.class {
            0 => base.to_string(),
            1 => format!("{base}{}", self.columns),
            2 | 3 => format!("{base}{}x{}", self.rows, self.columns),
            5 => "struct".to_string(),
            _ => base.to_string(),
        };
        if self.elements > 0 {
            format!("{shape}[{}]", self.elements)
        } else {
            shape
        }
    }
}

impl From<&dxbc_reader::RdefType> for Type {
    fn from(t: &dxbc_reader::RdefType) -> Self {
        Type {
            class: t.class,
            base: t.scalar,
            rows: t.rows,
            columns: t.columns,
            elements: t.elements,
            members: t
                .members
                .iter()
                .map(|m| Member {
                    name: m.name.clone(),
                    offset: m.offset,
                    ty: Type::from(&m.ty),
                })
                .collect(),
        }
    }
}

/// A resource the program binds: what the reflection's binding record (`D3D11_SHADER_INPUT_BIND_DESC`) says.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Binding {
    pub name: String,
    /// `D3D_SHADER_INPUT_TYPE`: 0 cbuffer, 1 tbuffer, 2 texture, 3 sampler, 4 to 13 the buffer and UAV kinds.
    pub kind: u32,
    /// The first register.
    pub slot: u32,
    /// How many registers it takes (an array); 0 for an array of unbounded size.
    pub count: u32,
    pub space: u32,
}

impl Binding {
    /// The register class it takes: `cb` (b), `srv` (t), `sampler` (s) or `uav` (u), as locum names bindings.
    pub fn class(&self) -> &'static str {
        match self.kind {
            0 => "cb",
            3 => "sampler",
            4 | 6 | 8..=11 | 13 => "uav",
            _ => "srv",
        }
    }

    /// Its name at register `slot` of its class: `name`, or `name[i]` in an array. None when it does not cover `slot`.
    pub fn name_at(&self, slot: u32) -> Option<String> {
        let i = slot.checked_sub(self.slot)?;
        match self.count {
            1 if i == 0 => Some(self.name.clone()),
            1 => None,
            n if n == 0 || i < n => Some(format!("{}[{i}]", self.name)),
            _ => None,
        }
    }
}

/// The container's `RDEF` part as dx reads it; none where the container has none (a DXIL program, stripped
/// reflection) or dx refuses it.
fn rdef(dxbc: &[u8]) -> Option<dxbc_reader::Rdef> {
    dxbc_reader::rdef(dxbc).ok().flatten()
}

fn binding(b: &dxbc_reader::RdefBinding) -> Binding {
    Binding {
        name: b.name.clone(),
        kind: b.kind,
        slot: b.register,
        count: b.count,
        space: b.space,
    }
}

/// Every resource a shader binds (constant buffers, textures, buffers, samplers, UAVs), from its DXBC or DXIL
/// bytecode, in the reflection's order. A DXBC program stripped of its reflection is read from its declarations,
/// each named by its register (`t3`, `cb0`).
pub fn bindings(code: &[u8]) -> Option<Vec<Binding>> {
    if let Some(rdef) = rdef(code) {
        return Some(rdef.bindings.iter().map(binding).collect());
    }
    let Some(m) = dxil(code) else {
        return declared(code);
    };
    let ranges = dxil_reader::reflection::ranges(&m).ok()?;
    Some(
        ranges
            .into_iter()
            .map(|(r, name, _)| Binding {
                name,
                // As D3D_SHADER_INPUT_TYPE, which `class` reads back.
                kind: match r.class {
                    1 => 4,
                    2 => 0,
                    3 => 3,
                    _ => 2,
                },
                slot: r.lower,
                count: if r.count == u32::MAX { 0 } else { r.count },
                space: r.space,
            })
            .collect(),
    )
}

/// The UAV registers a program declares, as (space, first, last) with `u32::MAX` the last of an unbounded array:
/// a DXIL program's from its `PSV0` part, which a runtime keeps however stripped the program is, and a DXBC
/// program's from its reflection. Cheap enough to run as programs are made. None where neither says.
pub fn uav_registers(code: &[u8]) -> Option<Vec<(u32, u32, u32)>> {
    if let Ok(parts) = dxil_reader::parts(code)
        && let Ok(Some(psv)) = dxil_reader::psv::part(&parts)
    {
        // 6 typed, 7 raw, 8 structured, 9 structured with a counter.
        return Some(
            psv.resources
                .iter()
                .filter(|r| (6..=9).contains(&r.resource_type))
                .map(|r| (r.space, r.lower, r.upper))
                .collect(),
        );
    }
    let bindings = match rdef(code) {
        Some(rdef) => rdef.bindings.iter().map(binding).collect(),
        None => declared(code)?,
    };
    Some(
        bindings
            .into_iter()
            .filter(|b| b.class() == "uav")
            .map(|b| {
                (
                    b.space,
                    b.slot,
                    if b.count == 0 {
                        u32::MAX
                    } else {
                        b.slot + b.count - 1
                    },
                )
            })
            .collect(),
    )
}

/// A DXBC program's registers from the declarations in its code, for a program stripped of its reflection: each
/// named by its register (`cb0`, `t3`, `s1`, `u0`; `t3 space 1` in Shader Model 5.1), in the order declared. None
/// for a container with no DXBC code.
pub fn declared(code: &[u8]) -> Option<Vec<Binding>> {
    use dxbc_reader::Declared as D;
    let words = dxbc_reader::code(code).ok()?;
    let program = dxbc_reader::decode(&words).ok()?;
    let out = program
        .declarations
        .iter()
        .filter_map(|d| {
            // D3D_SHADER_INPUT_TYPE, the register's letter, the Shader Model 5.0 register, the 5.1 range.
            let (kind, letter, slot, range) = match &d.kind {
                D::ConstantBuffer { slot, range, .. } => (0, "cb", *slot, range),
                D::Sampler { slot, range, .. } => (3, "s", *slot, range),
                D::Resource { slot, range, .. } => (2, "t", *slot, range),
                D::ResourceRaw { slot, range } => (7, "t", *slot, range),
                D::ResourceStructured { slot, range, .. } => (5, "t", *slot, range),
                D::UavTyped { slot, range, .. } => (4, "u", *slot, range),
                D::UavRaw { slot, range, .. } => (8, "u", *slot, range),
                D::UavStructured { slot, range, .. } => (6, "u", *slot, range),
                _ => return None,
            };
            let (slot, count, space) = match range {
                Some(r) => (
                    r.lower,
                    if r.upper == u32::MAX {
                        0
                    } else {
                        r.upper.saturating_sub(r.lower) + 1
                    },
                    r.space,
                ),
                None => (slot, 1, 0),
            };
            let name = match space {
                0 => format!("{letter}{slot}"),
                s => format!("{letter}{slot} space {s}"),
            };
            Some(Binding {
                name,
                kind,
                slot,
                count,
                space,
            })
        })
        .collect();
    Some(out)
}

/// A program's debug name: the file stem of the PDB its DXIL compiler named in the `ILDN` part, so
/// `...\GXLightAcc_DDirLightShadowed1.ppo.pdb` gives `GXLightAcc_DDirLightShadowed1`. FXC's DXBC has none.
pub fn debug_name(code: &[u8]) -> Option<String> {
    let parts = dxil_reader::parts(code).ok()?;
    let data = parts.iter().find(|p| &p.kind == b"ILDN")?.data;
    // u16 flags, u16 the name's length, then the name and a zero.
    let len = u16::from_le_bytes(data.get(2..4)?.try_into().ok()?) as usize;
    let name = std::str::from_utf8(data.get(4..4 + len)?).ok()?;
    let file = name.rsplit(['\\', '/']).next()?;
    file.split('.')
        .next()
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// A DXIL container's reflection module: the `STAT` part, which keeps the names and type annotations a stripped
/// program lacks, else the program's own module.
fn dxil(code: &[u8]) -> Option<dxil_reader::module::Module> {
    let parts = dxil_reader::parts(code).ok()?;
    let kind = if parts.iter().any(|p| &p.kind == b"STAT") {
        b"STAT"
    } else {
        b"DXIL"
    };
    dxil_reader::module(&parts, kind).ok().flatten()
}

/// The constant buffers a DXIL program declares: each cbuffer range with its struct type's fields as
/// `dx.typeAnnotations` states them.
fn dxil_constant_buffers(code: &[u8]) -> Option<Vec<ConstantBuffer>> {
    let m = dxil(code)?;
    let ranges = dxil_reader::reflection::ranges(&m).ok()?;
    Some(
        ranges
            .into_iter()
            .filter(|(r, ..)| r.class == 2)
            .map(|(r, name, ty)| {
                let mut variables: Vec<Variable> = ty
                    .map(|t| members(&m, t))
                    .unwrap_or_default()
                    .into_iter()
                    .map(|member| Variable {
                        name: member.name,
                        offset: member.offset,
                        size: 0,
                        // DXIL does not say which variables the program reads.
                        used: true,
                        ty: member.ty,
                    })
                    .collect();
                variables.sort_by_key(|v| v.offset);
                // Each variable runs to the next one, the last to the buffer's end.
                let ends: Vec<u32> = variables
                    .iter()
                    .skip(1)
                    .map(|v| v.offset)
                    .chain([r.extra])
                    .collect();
                for (v, end) in variables.iter_mut().zip(ends) {
                    v.size = end.saturating_sub(v.offset);
                }
                ConstantBuffer {
                    name,
                    slot: Some(r.lower),
                    space: r.space,
                    size: r.extra,
                    variables,
                }
            })
            .collect(),
    )
}

/// The fields of a DXIL struct type, each with its offset and its type in the reflection's terms.
fn members(m: &dxil_reader::module::Module, t: dxil_reader::module::TypeId) -> Vec<Member> {
    use dxil_reader::module::Type as T;
    let Some(T::Struct { elements, .. }) = m.types.get(t) else {
        return Vec::new();
    };
    let fields = dxil_reader::reflection::fields(m, t).unwrap_or_default();
    fields
        .iter()
        .zip(elements)
        .map(|(f, &e)| Member {
            name: f.name.clone(),
            offset: f.offset,
            ty: dxil_type(m, e, f),
        })
        .collect()
}

/// A field's type as `D3D_SHADER_VARIABLE_CLASS` and `_TYPE` would state it, from its LLVM type and annotation.
fn dxil_type(
    m: &dxil_reader::module::Module,
    t: dxil_reader::module::TypeId,
    f: &dxil_reader::reflection::FieldAnnotation,
) -> Type {
    use dxil_reader::module::Type as T;
    // DxilComponentType: 1 i1, 2 i16, 3 u16, 4 i32, 5 u32, 6 i64, 7 u64, 8 f16, 9 f32, 10 f64.
    let base = |t: Option<&T>| -> u16 {
        match (f.component, t) {
            (Some(1), _) | (None, Some(T::Integer(1))) => 1,
            (Some(2 | 4 | 6), _) => 2,
            (Some(3 | 5 | 7), _) | (None, Some(T::Integer(_))) => 19,
            (Some(10), _) | (None, Some(T::Double)) => 39,
            _ => 3,
        }
    };
    // LLVM lowers a matrix to its lines (a struct holding an array of vectors); only the annotation says it is one.
    if let Some((rows, columns, orientation)) = f.matrix {
        let per = if orientation == 1 { rows } else { columns }.max(1);
        let mut lines = 1u64;
        let mut at = t;
        loop {
            match m.types.get(at) {
                Some(T::Struct { elements, .. }) if elements.len() == 1 => at = elements[0],
                Some(T::Array(n, inner)) => {
                    lines *= n;
                    at = *inner;
                }
                _ => break,
            }
        }
        let scalar = match m.types.get(at) {
            Some(T::Vector(_, s)) => m.types.get(*s),
            other => other,
        };
        let count = lines / u64::from(per);
        return Type {
            class: if orientation == 1 { 2 } else { 3 },
            base: base(scalar),
            rows: rows as u16,
            columns: columns as u16,
            elements: if count > 1 { count as u16 } else { 0 },
            members: Vec::new(),
        };
    }
    match m.types.get(t) {
        Some(T::Array(n, inner)) => Type {
            elements: *n as u16,
            ..dxil_type(m, *inner, f)
        },
        Some(T::Vector(n, s)) => Type {
            class: 1,
            base: base(m.types.get(*s)),
            rows: 1,
            columns: *n as u16,
            elements: 0,
            members: Vec::new(),
        },
        Some(T::Struct { .. }) => Type {
            class: 5,
            base: 0,
            rows: 1,
            columns: 1,
            elements: 0,
            members: members(m, t),
        },
        other => Type {
            class: 0,
            base: base(other),
            rows: 1,
            columns: 1,
            elements: 0,
            members: Vec::new(),
        },
    }
}

/// The constant buffers a shader declares, from its DXBC bytecode. The reflection also lays out tbuffers, interface
/// pointers and each structured buffer's element as buffers; those are bound to t and u registers, not b, so they are
/// left out.
pub fn constant_buffers(dxbc: &[u8]) -> Option<Vec<ConstantBuffer>> {
    let Some(rdef) = rdef(dxbc) else {
        return dxil_constant_buffers(dxbc);
    };
    Some(
        rdef.buffers
            .iter()
            // D3D_CBUFFER_TYPE: 0 is D3D_CT_CBUFFER.
            .filter(|cb| cb.kind == 0)
            .map(|cb| {
                // Constant buffers are bound by name: the binding of type 0 (cbuffer) with the same name gives the
                // register.
                let bound = rdef.bindings.iter().find(|b| b.kind == 0 && b.name == cb.name);
                ConstantBuffer {
                    name: cb.name.clone(),
                    slot: bound.map(|b| b.register),
                    space: bound.map_or(0, |b| b.space),
                    size: cb.size,
                    variables: cb
                        .variables
                        .iter()
                        .map(|v| Variable {
                            name: v.name.clone(),
                            offset: v.offset,
                            size: v.size,
                            used: v.flags & 2 != 0,
                            ty: Type::from(&v.ty),
                        })
                        .collect(),
                }
            })
            .collect(),
    )
}

const COMPONENTS: [&str; 4] = ["x", "y", "z", "w"];

/// The name of the 4-byte value at `offset` in a variable of type `ty` called `name`, like `uvScale.y`,
/// `bones[3][1].z` or `light.color.x`, and its base type. HLSL packs arrays and structs in 16-byte registers.
fn name_in(name: &str, ty: &Type, offset: u32) -> (String, u16) {
    if ty.elements > 0 {
        let element = Type {
            elements: 0,
            ..ty.clone()
        };
        let stride = element_stride(&element);
        let i = (offset / stride).min(u32::from(ty.elements) - 1);
        return name_in(&format!("{name}[{i}]"), &element, offset - i * stride);
    }
    match ty.class {
        5 => {
            let member = ty.members.iter().rev().find(|m| m.offset <= offset);
            match member {
                Some(m) => name_in(&format!("{name}.{}", m.name), &m.ty, offset - m.offset),
                None => (name.to_string(), ty.base),
            }
        }
        2 | 3 => {
            // Row-major: each row is a register; column-major: each column is.
            let (major, minor) = (offset / 16, (offset % 16) / 4);
            let (r, c) = if ty.class == 2 {
                (major, minor)
            } else {
                (minor, major)
            };
            (format!("{name}[{r}][{c}]"), ty.base)
        }
        1 => (
            format!("{name}.{}", COMPONENTS[((offset / 4) % 4) as usize]),
            ty.base,
        ),
        _ => (name.to_string(), ty.base),
    }
}

/// The bytes one array element of `ty` takes in a constant buffer: every element starts a register.
fn element_stride(ty: &Type) -> u32 {
    size(ty).div_ceil(16) * 16
}

/// The bytes a value of type `ty` covers in a constant buffer, to the end of its last component: a struct's last
/// member or an array's last element may end inside a register, and the next variable may start in the rest of it.
fn size(ty: &Type) -> u32 {
    let one = match ty.class {
        5 => ty
            .members
            .iter()
            .map(|m| m.offset + size(&m.ty))
            .max()
            .unwrap_or(0),
        // Row-major: each row a register; column-major: each column.
        2 => u32::from(ty.rows.max(1) - 1) * 16 + u32::from(ty.columns.max(1)) * 4,
        3 => u32::from(ty.columns.max(1) - 1) * 16 + u32::from(ty.rows.max(1)) * 4,
        _ => u32::from(ty.columns.max(1)) * 4,
    };
    match ty.elements {
        0 => one,
        n => u32::from(n - 1) * one.div_ceil(16) * 16 + one,
    }
}

impl ConstantBuffer {
    /// The buffer as it would be declared if its last variable, an array, ran on over `bytes` bytes, with that
    /// variable's name and its elements as declared and as extended: a game can bind a range larger than a program
    /// declares that holds more elements than the program reads (Sekiro binds 225 light sets of `cbLight`, where a
    /// program declares `FC_DirLight[2]`). None when the last variable is not an array or no further element fits.
    pub fn extended(&self, bytes: u32) -> Option<(ConstantBuffer, String, u16, u16)> {
        let (i, last) = self.variables.iter().enumerate().max_by_key(|(_, v)| v.offset)?;
        if last.ty.elements == 0 {
            return None;
        }
        let one = Type {
            elements: 0,
            ..last.ty.clone()
        };
        let (stride, size) = (element_stride(&one), size(&one));
        let room = bytes.checked_sub(last.offset)?;
        let elements = u16::try_from(room.checked_sub(size)? / stride + 1).unwrap_or(u16::MAX);
        if elements <= last.ty.elements {
            return None;
        }
        let mut cb = self.clone();
        let v = &mut cb.variables[i];
        v.ty.elements = elements;
        v.size = self::size(&v.ty);
        cb.size = cb.size.max(v.offset + v.size);
        Some((cb, last.name.clone(), last.ty.elements, elements))
    }

    /// The name of the 4-byte value at `offset` bytes into the buffer, if a variable covers it.
    pub fn name_at(&self, offset: u32) -> Option<String> {
        self.at(offset).map(|(name, _)| name)
    }

    /// The name of the 4-byte value at `offset` bytes into the buffer and its base type (`D3D_SHADER_VARIABLE_TYPE`:
    /// 1 bool, 2 int, 3 float, 19 uint), if a variable covers it.
    pub fn at(&self, offset: u32) -> Option<(String, u16)> {
        let v = self
            .variables
            .iter()
            .find(|v| (v.offset..v.offset + v.size).contains(&offset))?;
        Some(name_in(
            &format!("{}.{}", self.name, v.name),
            &v.ty,
            offset - v.offset,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vector(columns: u16, elements: u16) -> Type {
        Type {
            class: 1,
            base: 3,
            rows: 1,
            columns,
            elements,
            members: Vec::new(),
        }
    }

    /// `testdata/dxbc-stripped.cso`: FXC's cs_5_0 with `/Qstrip_reflect` (no RDEF) of a program declaring b2, t3, s1,
    /// a structured u0 and a raw u4 (`scratch/locum-dx12/dxbc-stripped/stripped.hlsl`).
    #[test]
    fn a_stripped_program_is_read_from_its_declarations() {
        let code = include_bytes!("testdata/dxbc-stripped.cso");
        assert!(rdef(code).is_none(), "the fixture has no reflection");
        let mut found: Vec<(String, &str, u32, u32)> = bindings(code)
            .expect("its declarations")
            .into_iter()
            .map(|b| (b.name.clone(), b.class(), b.slot, b.count))
            .collect();
        found.sort();
        assert_eq!(
            found,
            [
                ("cb2".to_string(), "cb", 2, 1),
                ("s1".to_string(), "sampler", 1, 1),
                ("t3".to_string(), "srv", 3, 1),
                ("u0".to_string(), "uav", 0, 1),
                ("u4".to_string(), "uav", 4, 1),
            ]
        );
        let mut uavs = uav_registers(code).expect("its UAVs");
        uavs.sort();
        assert_eq!(uavs, [(0, 0, 0), (0, 4, 4)]);
    }

    #[test]
    fn names() {
        let cb = ConstantBuffer {
            name: "cbTexScroll".into(),
            slot: Some(11),
            space: 0,
            size: 64,
            variables: vec![Variable {
                name: "scroll".into(),
                offset: 0,
                size: 64,
                used: true,
                ty: vector(4, 4),
            }],
        };
        assert_eq!(cb.name_at(0).unwrap(), "cbTexScroll.scroll[0].x");
        assert_eq!(cb.name_at(20).unwrap(), "cbTexScroll.scroll[1].y");
        assert_eq!(cb.name_at(64), None);
        let m = Type {
            class: 2,
            base: 3,
            rows: 4,
            columns: 4,
            elements: 0,
            members: Vec::new(),
        };
        assert_eq!(name_in("world", &m, 36).0, "world[2][1]");
        assert_eq!(type_name_check(), "float4[4]");
    }

    #[test]
    fn a_last_array_runs_on_over_a_larger_binding() {
        // struct { float4 v; float f; } lights[2] after a float4 head: 32 bytes an element (20 used), from 16.
        let light = Type {
            class: 5,
            base: 0,
            rows: 1,
            columns: 1,
            elements: 2,
            members: vec![
                Member {
                    name: "v".into(),
                    offset: 0,
                    ty: vector(4, 0),
                },
                Member {
                    name: "f".into(),
                    offset: 16,
                    ty: vector(1, 0),
                },
            ],
        };
        let cb = ConstantBuffer {
            name: "cbLight".into(),
            slot: Some(5),
            space: 0,
            size: 80,
            variables: vec![
                Variable {
                    name: "head".into(),
                    offset: 0,
                    size: 16,
                    used: true,
                    ty: vector(4, 0),
                },
                Variable {
                    name: "lights".into(),
                    offset: 16,
                    size: 52,
                    used: true,
                    ty: light,
                },
            ],
        };
        assert_eq!(cb.name_at(84), None);
        // 16 + 32 * 3 + 20 = 132 bytes hold a fourth element's used part; 131 do not.
        let (wide, name, was, now) = cb.extended(132).unwrap();
        assert_eq!((name.as_str(), was, now), ("lights", 2, 4));
        assert_eq!(wide.name_at(80).unwrap(), "cbLight.lights[2].v.x");
        assert_eq!(wide.name_at(128).unwrap(), "cbLight.lights[3].f.x");
        assert_eq!(cb.extended(131).unwrap().3, 3);
        // Nothing more fits, or the last variable is no array.
        assert!(cb.extended(80).is_none());
        let mut plain = cb.clone();
        plain.variables.pop();
        assert!(plain.extended(4096).is_none());
    }

    /// An array of structs whose last member ends inside a register: Elden Ring's `VC_SpeedTreeWind[2]`, a struct of
    /// 272 bytes whose `u_uInteractionFlag` is at 268, puts element 1 at 272, not at 288.
    #[test]
    fn struct_array_stride() {
        let scalar = Type {
            class: 0,
            base: 19,
            rows: 1,
            columns: 1,
            elements: 0,
            members: Vec::new(),
        };
        let wind = Type {
            class: 5,
            base: 0,
            rows: 1,
            columns: 1,
            elements: 2,
            members: vec![
                Member {
                    name: "dir".into(),
                    offset: 0,
                    ty: vector(4, 0),
                },
                Member {
                    name: "flag".into(),
                    offset: 268,
                    ty: scalar.clone(),
                },
            ],
        };
        assert_eq!(name_in("w", &wind, 268).0, "w[0].flag");
        assert_eq!(name_in("w", &wind, 272).0, "w[1].dir.x");
        assert_eq!(name_in("w", &wind, 540).0, "w[1].flag");
        // A float3 array: each element starts a register, the last ends at its third component.
        assert_eq!(size(&vector(3, 4)), 3 * 16 + 12);
        assert_eq!(name_in("v", &vector(3, 4), 52).0, "v[3].y");
        // A row-major float3x4: three registers of four components.
        let m = Type {
            class: 2,
            base: 3,
            rows: 3,
            columns: 4,
            elements: 0,
            members: Vec::new(),
        };
        assert_eq!(size(&m), 48);
    }

    fn type_name_check() -> String {
        vector(4, 4).name()
    }
    /// A DXBC container holding only `rdef`.
    fn container(rdef: &[u8]) -> Vec<u8> {
        let mut d = b"DXBC".to_vec();
        d.extend([0u8; 16]);
        d.extend(1u32.to_le_bytes());
        d.extend((36 + 8 + rdef.len() as u32).to_le_bytes());
        d.extend(1u32.to_le_bytes());
        d.extend(36u32.to_le_bytes());
        d.extend(b"RDEF");
        d.extend((rdef.len() as u32).to_le_bytes());
        d.extend(rdef);
        d
    }

    /// An SM 5.0 or 5.1 RDEF as fxc writes it: header, tag and record sizes, one binding (b6, in space 2 in 5.1), one
    /// constant buffer `cbCopy` of two float4x4, the second of which the program never reads.
    fn cb_copy(sm51: bool) -> Vec<u8> {
        laid_out(sm51, 0, 0)
    }

    /// The same layout as a buffer of `D3D_CBUFFER_TYPE` `buffer_kind`, bound by a binding of `D3D_SHADER_INPUT_TYPE`
    /// `binding_kind`.
    fn laid_out(sm51: bool, buffer_kind: u32, binding_kind: u32) -> Vec<u8> {
        let mut r = vec![0u8; 400];
        let put = |r: &mut Vec<u8>, at: usize, v: u32| r[at..at + 4].copy_from_slice(&v.to_le_bytes());
        let text = |r: &mut Vec<u8>, at: usize, t: &str| r[at..at + t.len()].copy_from_slice(t.as_bytes());
        // cb count, cb at, binding count, binding at, version 5.0 or 5.1 pixel, flags, creator.
        put(&mut r, 0, 1);
        put(&mut r, 4, 100);
        put(&mut r, 8, 1);
        put(&mut r, 12, 60);
        put(&mut r, 16, if sm51 { 0xffff_0501 } else { 0xffff_0500 });
        let (tag, binding) = if sm51 {
            (0x2544_1313, 40)
        } else {
            (u32::from_le_bytes(*b"RD11"), 32)
        };
        for (i, v) in [tag, 60, 24, binding, 40, 36, 12].into_iter().enumerate() {
            put(&mut r, 28 + i * 4, v);
        }
        // The binding: name, cbuffer, register b6, one register; 5.1's space 2 and id 0 end the record at 100.
        put(&mut r, 60, 300);
        put(&mut r, 64, binding_kind);
        put(&mut r, 80, 6);
        put(&mut r, 84, 1);
        if sm51 {
            put(&mut r, 92, 2);
        }
        // The constant buffer: name, two variables at 124, 128 bytes.
        put(&mut r, 100, 300);
        put(&mut r, 104, 2);
        put(&mut r, 108, 124);
        put(&mut r, 112, 128);
        put(&mut r, 120, buffer_kind);
        for (v, (name, offset, flags)) in [(320, 0, 2), (330, 64, 0)].into_iter().enumerate() {
            let at = 124 + v * 40;
            put(&mut r, at, name);
            put(&mut r, at + 4, offset);
            put(&mut r, at + 8, 64);
            put(&mut r, at + 12, flags);
            put(&mut r, at + 16, 210);
        }
        // float4x4: class 3 (column matrix), base 3, 4 rows, 4 columns.
        for (i, v) in [3u16, 3, 4, 4, 0, 0].into_iter().enumerate() {
            r[210 + i * 2..212 + i * 2].copy_from_slice(&v.to_le_bytes());
        }
        text(&mut r, 300, "cbCopy");
        text(&mut r, 320, "first");
        text(&mut r, 330, "second");
        container(&r)
    }

    #[test]
    fn variables_after_the_first_use_the_variable_record_size() {
        let cbs = constant_buffers(&cb_copy(false)).expect("reflection");
        assert_eq!(cbs[0].slot, Some(6));
        let names: Vec<(&str, bool)> = cbs[0]
            .variables
            .iter()
            .map(|v| (v.name.as_str(), v.used))
            .collect();
        assert_eq!(names, [("first", true), ("second", false)]);
        assert_eq!(cbs[0].name_at(64).unwrap(), "cbCopy.second[0][0]");
        let bound = bindings(&cb_copy(false)).expect("bindings");
        assert_eq!(
            bound,
            [Binding {
                name: "cbCopy".into(),
                kind: 0,
                slot: 6,
                count: 1,
                space: 0
            }]
        );
        assert_eq!(bound[0].class(), "cb");
    }

    #[test]
    fn a_shader_model_5_1_binding_and_its_constant_buffer_are_in_its_register_space() {
        let bound = bindings(&cb_copy(true)).expect("bindings");
        assert_eq!((bound[0].slot, bound[0].count, bound[0].space), (6, 1, 2));
        let cbs = constant_buffers(&cb_copy(true)).expect("reflection");
        assert_eq!((cbs[0].slot, cbs[0].space), (Some(6), 2));
        assert_eq!(cbs[0].name_at(4).unwrap(), "cbCopy.first[1][0]");
    }

    #[test]
    fn a_structured_buffers_element_layout_is_not_a_constant_buffer() {
        // D3D_CT_RESOURCE_BIND_INFO laid out for a D3D_SIT_STRUCTURED binding.
        let element = laid_out(false, 3, 5);
        assert!(constant_buffers(&element).expect("reflection").is_empty());
        assert_eq!(bindings(&element).expect("bindings")[0].class(), "srv");
    }

    #[test]
    fn a_binding_names_the_registers_it_covers() {
        let at = |kind, slot, count| Binding {
            name: "g_Tex".into(),
            kind,
            slot,
            count,
            space: 0,
        };
        assert_eq!(at(2, 4, 1).name_at(4).as_deref(), Some("g_Tex"));
        assert_eq!(at(2, 4, 1).name_at(5), None);
        assert_eq!(at(2, 4, 1).name_at(3), None);
        assert_eq!(at(2, 4, 3).name_at(6).as_deref(), Some("g_Tex[2]"));
        assert_eq!(at(2, 4, 3).name_at(7), None);
        // An array of unbounded size covers every register from its first.
        assert_eq!(at(2, 4, 0).name_at(90).as_deref(), Some("g_Tex[86]"));
        let classes: Vec<&str> = [0, 1, 2, 3, 4, 5, 7, 9, 11, 12]
            .into_iter()
            .map(|k| at(k, 0, 1).class())
            .collect();
        assert_eq!(
            classes,
            [
                "cb", "srv", "srv", "sampler", "uav", "srv", "srv", "uav", "uav", "srv"
            ]
        );
    }
}
