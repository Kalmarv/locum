//! Shader reflection from a DXBC container's `RDEF` chunk: each constant buffer's name, register and variables, so a
//! constant can be shown as `cbTexScroll.uvScale.x` instead of `b11 c0.x`.

/// A DXBC container's chunk with the four-character code `code`.
pub fn chunk<'a>(dxbc: &'a [u8], code: &[u8; 4]) -> Option<&'a [u8]> {
    if dxbc.get(..4)? != b"DXBC" {
        return None;
    }
    let count = u32_at(dxbc, 28)? as usize;
    for i in 0..count {
        let offset = u32_at(dxbc, 32 + i * 4)? as usize;
        if dxbc.get(offset..offset + 4)? == code {
            let size = u32_at(dxbc, offset + 4)? as usize;
            return dxbc.get(offset + 8..offset + 8 + size);
        }
    }
    None
}

fn u32_at(b: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(b.get(at..at + 4)?.try_into().ok()?))
}

fn u16_at(b: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_le_bytes(b.get(at..at + 2)?.try_into().ok()?))
}

fn str_at(b: &[u8], at: usize) -> Option<String> {
    let rest = b.get(at..)?;
    let end = rest.iter().position(|&c| c == 0)?;
    Some(String::from_utf8_lossy(&rest[..end]).into_owned())
}

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

fn read_type(rdef: &[u8], at: usize, depth: u32) -> Option<Type> {
    let mut t = Type {
        class: u16_at(rdef, at)?,
        base: u16_at(rdef, at + 2)?,
        rows: u16_at(rdef, at + 4)?,
        columns: u16_at(rdef, at + 6)?,
        elements: u16_at(rdef, at + 8)?,
        members: Vec::new(),
    };
    let members = u16_at(rdef, at + 10)? as usize;
    let members_at = u32_at(rdef, at + 12)? as usize;
    if depth < 8 {
        for i in 0..members {
            let m = members_at + i * 12;
            let name = str_at(rdef, u32_at(rdef, m)? as usize)?;
            let ty = read_type(rdef, u32_at(rdef, m + 4)? as usize, depth + 1)?;
            t.members.push(Member {
                name,
                offset: u32_at(rdef, m + 8)?,
                ty,
            });
        }
    }
    Some(t)
}

/// The constant buffers a shader declares, from its DXBC bytecode.
pub fn constant_buffers(dxbc: &[u8]) -> Option<Vec<ConstantBuffer>> {
    let rdef = chunk(dxbc, b"RDEF")?;
    let cb_count = u32_at(rdef, 0)? as usize;
    let cb_at = u32_at(rdef, 4)? as usize;
    let bind_count = u32_at(rdef, 8)? as usize;
    let bind_at = u32_at(rdef, 12)? as usize;
    let (minor, major) = (rdef.get(16).copied()?, rdef.get(17).copied()?);
    // Shader model 5 adds an "RD11" header giving the variable record size (40 bytes, 24 before); 5.1 bindings
    // carry a register space and are 40 bytes instead of 32.
    let rd11 = major >= 5 && rdef.get(28..32) == Some(b"RD11");
    let var_size = if rd11 {
        u32_at(rdef, 40).unwrap_or(40) as usize
    } else {
        24
    };
    let bind_size = if major > 5 || (major == 5 && minor >= 1) {
        40
    } else {
        32
    };
    // Constant buffers are bound by name: the binding of type 0 (cbuffer) with the same name gives the register.
    let mut registers = Vec::new();
    for i in 0..bind_count {
        let b = bind_at + i * bind_size;
        let name = str_at(rdef, u32_at(rdef, b)? as usize)?;
        let kind = u32_at(rdef, b + 4)?;
        let slot = u32_at(rdef, b + 20)?;
        let space = if bind_size >= 40 { u32_at(rdef, b + 32)? } else { 0 };
        if kind == 0 {
            registers.push((name, slot, space));
        }
    }
    let mut out = Vec::with_capacity(cb_count);
    for i in 0..cb_count {
        let c = cb_at + i * 24;
        let name = str_at(rdef, u32_at(rdef, c)? as usize)?;
        let var_count = u32_at(rdef, c + 4)? as usize;
        let var_at = u32_at(rdef, c + 8)? as usize;
        let size = u32_at(rdef, c + 12)?;
        let mut variables = Vec::with_capacity(var_count);
        for v in 0..var_count {
            let at = var_at + v * var_size;
            variables.push(Variable {
                name: str_at(rdef, u32_at(rdef, at)? as usize)?,
                offset: u32_at(rdef, at + 4)?,
                size: u32_at(rdef, at + 8)?,
                used: u32_at(rdef, at + 12)? & 2 != 0,
                ty: read_type(rdef, u32_at(rdef, at + 16)? as usize, 0)?,
            });
        }
        let (slot, space) = registers
            .iter()
            .find(|(n, _, _)| *n == name)
            .map_or((None, 0), |&(_, s, sp)| (Some(s), sp));
        out.push(ConstantBuffer {
            name,
            slot,
            space,
            size,
            variables,
        });
    }
    Some(out)
}

const COMPONENTS: [&str; 4] = ["x", "y", "z", "w"];

/// The name of the 4-byte value at `offset` in a variable of type `ty` called `name`, like `uvScale.y`,
/// `bones[3][1].z` or `light.color.x`. HLSL packs arrays and structs in 16-byte registers.
fn name_in(name: &str, ty: &Type, offset: u32) -> String {
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
                None => name.to_string(),
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
            format!("{name}[{r}][{c}]")
        }
        1 => format!("{name}.{}", COMPONENTS[((offset / 4) % 4) as usize]),
        _ => name.to_string(),
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
        assert_eq!(name_in("world", &m, 36), "world[2][1]");
        assert_eq!(type_name_check(), "float4[4]");
    }

    fn type_name_check() -> String {
        vector(4, 4).name()
    }
}
