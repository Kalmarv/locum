//! Shader reflection from a DXBC container's `RDEF` part, as dx's `dxbc-reader` reads it: each constant buffer's name,
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

/// Every resource a shader binds (constant buffers, textures, buffers, samplers, UAVs), from its DXBC bytecode, in
/// the reflection's order.
pub fn bindings(dxbc: &[u8]) -> Option<Vec<Binding>> {
    Some(rdef(dxbc)?.bindings.iter().map(binding).collect())
}

/// The constant buffers a shader declares, from its DXBC bytecode. The reflection also lays out tbuffers, interface
/// pointers and each structured buffer's element as buffers; those are bound to t and u registers, not b, so they are
/// left out.
pub fn constant_buffers(dxbc: &[u8]) -> Option<Vec<ConstantBuffer>> {
    let rdef = rdef(dxbc)?;
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

/// The bytes one array element of `ty` takes in a constant buffer (every element but the last starts a register).
fn element_stride(ty: &Type) -> u32 {
    let bytes = match ty.class {
        5 => ty
            .members
            .iter()
            .map(|m| m.offset + element_stride(&m.ty) * u32::from(m.ty.elements.max(1)))
            .max()
            .unwrap_or(16),
        2 => u32::from(ty.rows) * 16,
        3 => u32::from(ty.columns) * 16,
        _ => u32::from(ty.columns.max(1)) * 4,
    };
    bytes.div_ceil(16) * 16
}

impl ConstantBuffer {
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
