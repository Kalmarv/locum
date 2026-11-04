use super::watch::{Shadow, Write};
use super::*;
use crate::{d3d11::registry, vtables::context::real};
use locum_capture::{layout, layout::Sub};

pub(super) enum Upload {
    Update {
        resource: Object,
        sub: u32,
        region: Option<D3D11_BOX>,
        bytes: Vec<u8>,
        layout: Sub,
        flags: u32,
    },
    Tiles {
        resource: Object,
        at: D3D11_TILED_RESOURCE_COORDINATE,
        region: D3D11_TILE_REGION_SIZE,
        bytes: Vec<u8>,
        flags: u32,
    },
    Map {
        resource: Object,
        sub: u32,
        kind: D3D11_MAP,
        flags: u32,
        writes: Vec<Write>,
        layout: Sub,
    },
}
pub(super) struct Mapped {
    resource: Object,
    sub: u32,
    kind: D3D11_MAP,
    flags: u32,
    real: D3D11_MAPPED_SUBRESOURCE,
    shadow: Shadow,
    layout: Sub,
}

#[derive(Clone, Copy)]
struct Pitches {
    row: usize,
    depth: usize,
}
impl Pitches {
    fn new(layout: &Sub, mapped: &D3D11_MAPPED_SUBRESOURCE) -> Option<Self> {
        if mapped.pData.is_null() || layout.row_bytes == 0 || layout.rows == 0 || layout.depth == 0 {
            return None;
        }
        // The unused pitches can be zero for buffers and one-dimensional resources.
        let row = if layout.rows == 1 {
            layout.row_bytes as usize
        } else {
            mapped.RowPitch as usize
        };
        let slice = row.checked_mul(layout.rows as usize)?;
        let depth = if layout.depth == 1 {
            slice
        } else {
            mapped.DepthPitch as usize
        };
        if row < layout.row_bytes as usize || depth < slice {
            return None;
        }
        depth.checked_mul(layout.depth as usize)?;
        Some(Self { row, depth })
    }
}

fn canonical(layout: &Sub, pitches: Pitches, physical: &[Write]) -> Vec<Write> {
    let mut out = Vec::new();
    for write in physical {
        let mut at = write.offset;
        let end = at + write.bytes.len();
        while at < end {
            let z = at / pitches.depth;
            let within = at % pitches.depth;
            let y = within / pitches.row;
            let x = within % pitches.row;
            if y >= layout.rows as usize {
                at = end.min((z + 1) * pitches.depth);
            } else if x >= layout.row_bytes as usize {
                at = end.min(z * pitches.depth + (y + 1) * pitches.row);
            } else {
                let n = (end - at).min(layout.row_bytes as usize - x);
                let offset = (z * layout.rows as usize + y) * layout.row_bytes as usize + x;
                Write::push(
                    &mut out,
                    offset,
                    &write.bytes[at - write.offset..at - write.offset + n],
                );
                at += n;
            }
        }
    }
    out
}

/// Copies canonical byte ranges into a live map. Ranges may cross rows or depth slices.
unsafe fn copy_writes(layout: &Sub, pitches: Pitches, writes: &[Write], dst: *mut u8) {
    for write in writes {
        let mut used = 0;
        while used < write.bytes.len() {
            let at = write.offset + used;
            let row = at / layout.row_bytes as usize;
            let x = at % layout.row_bytes as usize;
            let z = row / layout.rows as usize;
            let y = row % layout.rows as usize;
            let n = (write.bytes.len() - used).min(layout.row_bytes as usize - x);
            unsafe {
                core::ptr::copy_nonoverlapping(
                    write.bytes.as_ptr().add(used),
                    dst.add(z * pitches.depth + y * pitches.row + x),
                    n,
                )
            };
            used += n;
        }
    }
}

unsafe fn dimensions(
    raw: *mut c_void,
    sub: u32,
    region: Option<&D3D11_BOX>,
) -> Option<(registry::Dims, Sub)> {
    let resource = unsafe { ID3D11Resource::from_raw_borrowed(&raw) }?;
    let dims = registry::dims_of(resource)?;
    let mut layout = *dims.subs()?.get(sub as usize)?;
    if let Some(b) = region {
        layout.width = b.right.saturating_sub(b.left);
        if dims.kind == "buffer" {
            layout.row_bytes = layout.width;
        } else {
            let f = locum_capture::format::layout(dims.format)?;
            layout.height = b.bottom.saturating_sub(b.top);
            layout.depth = b.back.saturating_sub(b.front);
            layout.row_bytes = f.row_bytes(layout.width);
            layout.rows = f.rows(layout.height);
        }
        layout.size = u64::from(layout.row_bytes) * u64::from(layout.rows) * u64::from(layout.depth);
    }
    Some((dims, layout))
}
impl Recorder {
    #[allow(clippy::too_many_arguments)]
    pub unsafe fn update(
        &self,
        resource: *mut c_void,
        sub: u32,
        region: *const D3D11_BOX,
        data: *const c_void,
        row: u32,
        depth: u32,
        flags: u32,
    ) {
        let region = unsafe { region.as_ref() }.copied();
        let Some((dims, layout)) = (unsafe { dimensions(resource, sub, region.as_ref()) }) else {
            self.gap("upload layout is unknown");
            return;
        };
        if data.is_null() {
            self.gap("upload data is null");
            return;
        }
        let (row, depth) = if matches!(dims.kind, "buffer" | "texture1d") {
            (layout.row_bytes, layout.slice_bytes() as u32)
        } else {
            (row, depth)
        };
        let mut bytes = Vec::with_capacity(layout.size as usize);
        unsafe { layout::pack(&layout, data.cast(), row as usize, depth as usize, &mut bytes) };
        let upload = Upload::Update {
            resource: unsafe { Object::copy(resource) },
            sub,
            region,
            bytes,
            layout,
            flags,
        };
        self.current
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .commands
            .push(Recorded::Upload(Box::new(upload)));
    }
    pub unsafe fn tiles(
        &self,
        resource: *mut c_void,
        at: *const D3D11_TILED_RESOURCE_COORDINATE,
        region: *const D3D11_TILE_REGION_SIZE,
        data: *const c_void,
        flags: u32,
    ) {
        let (Some(at), Some(size), Some(bytes)) =
            (unsafe { at.as_ref() }, unsafe { region.as_ref() }, unsafe {
                crate::d3d11::capture::tiled::data(data, region)
            })
        else {
            self.gap("tile upload arguments are incomplete");
            return;
        };
        let upload = Upload::Tiles {
            resource: unsafe { Object::copy(resource) },
            at: *at,
            region: *size,
            bytes,
            flags,
        };
        self.current
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .commands
            .push(Recorded::Upload(Box::new(upload)));
    }
    pub unsafe fn mapped(
        &self,
        resource: *mut c_void,
        sub: u32,
        kind: D3D11_MAP,
        flags: u32,
        out: *mut D3D11_MAPPED_SUBRESOURCE,
    ) {
        let Some(m) = (unsafe { out.as_mut() }) else {
            self.gap("deferred map has no output");
            return;
        };
        let Some((dims, layout)) = (unsafe { dimensions(resource, sub, None) }) else {
            self.gap("mapped layout is unknown");
            return;
        };
        if !matches!(kind, D3D11_MAP_WRITE_DISCARD | D3D11_MAP_WRITE_NO_OVERWRITE) || m.pData.is_null() {
            self.gap("deferred map type cannot be reconstructed");
            return;
        }
        if kind == D3D11_MAP_WRITE_NO_OVERWRITE && dims.kind != "buffer" {
            self.gap("NO_OVERWRITE on a non-buffer resource");
            return;
        }
        let Some(pitches) = Pitches::new(&layout, m) else {
            self.gap("mapped pitches cannot be reconstructed");
            return;
        };
        let size = pitches.depth * layout.depth as usize;
        let mut s = self.current.lock().unwrap_or_else(|e| e.into_inner());
        let key = (resource as usize, sub);
        if s.maps.contains_key(&key) {
            s.gaps.push("resource mapped twice without Unmap");
            return;
        }
        let mut shadow = match s.shadows.remove(&key) {
            Some(shadow) if shadow.size == size => shadow,
            _ if kind == D3D11_MAP_WRITE_NO_OVERWRITE => {
                s.gaps
                    .push("NO_OVERWRITE without a retained DISCARD in this list");
                return;
            }
            _ => match Shadow::new(size) {
                Ok(shadow) => shadow,
                Err(reason) => {
                    s.gaps.push(reason);
                    return;
                }
            },
        };
        if kind == D3D11_MAP_WRITE_DISCARD {
            shadow.discard();
        }
        let mapped = Mapped {
            resource: unsafe { Object::copy(resource) },
            sub,
            kind,
            flags,
            real: *m,
            shadow,
            layout,
        };
        m.pData = mapped.shadow.pointer().cast();
        s.maps.insert(key, mapped);
    }
    pub unsafe fn unmap(&self, resource: *mut c_void, sub: u32) {
        let mut s = self.current.lock().unwrap_or_else(|e| e.into_inner());
        let Some(mut m) = s.maps.remove(&(resource as usize, sub)) else {
            s.gaps.push("unmap has no retained map");
            return;
        };
        let (physical, failed) = unsafe { m.shadow.changes() };
        if failed {
            s.gaps
                .push("GetWriteWatch failed; original command list retained");
        }
        // Strip pitch padding without scanning any untouched rows. The same byte runs are
        // copied to the real map and retained for reconstruction; never read driver memory.
        let pitches = Pitches::new(&m.layout, &m.real).expect("validated at Map");
        let writes = canonical(&m.layout, pitches, &physical);
        unsafe { copy_writes(&m.layout, pitches, &writes, m.real.pData.cast()) };
        s.shadows.insert((resource as usize, sub), m.shadow);
        s.commands.push(Recorded::Upload(Box::new(Upload::Map {
            resource: m.resource,
            sub: m.sub,
            kind: m.kind,
            flags: m.flags,
            writes,
            layout: m.layout,
        })));
    }
}
impl Upload {
    pub unsafe fn replay(&self, context: *mut c_void) -> windows::core::Result<()> {
        match self {
            Self::Update {
                resource,
                sub,
                region,
                bytes,
                layout,
                flags,
            } => unsafe {
                real::UpdateSubresource1(
                    context,
                    resource.raw(),
                    *sub,
                    region.as_ref().map_or(core::ptr::null(), |b| b),
                    bytes.as_ptr().cast(),
                    layout.row_bytes,
                    layout.slice_bytes() as u32,
                    *flags,
                )
            },
            Self::Tiles {
                resource,
                at,
                region,
                bytes,
                flags,
            } => unsafe {
                real::UpdateTiles(context, resource.raw(), at, region, bytes.as_ptr().cast(), *flags)
            },
            Self::Map {
                resource,
                sub,
                kind,
                flags,
                writes,
                layout,
            } => {
                let mut m = D3D11_MAPPED_SUBRESOURCE::default();
                unsafe { real::Map(context, resource.raw(), *sub, *kind, *flags, &mut m) }.ok()?;
                let Some(pitches) = Pitches::new(layout, &m) else {
                    unsafe { real::Unmap(context, resource.raw(), *sub) };
                    return Err(windows::core::Error::from_hresult(
                        windows::Win32::Foundation::E_INVALIDARG,
                    ));
                };
                unsafe {
                    copy_writes(layout, pitches, writes, m.pData.cast());
                    real::Unmap(context, resource.raw(), *sub)
                };
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dirty_spans_cross_padding_and_replay_with_different_pitches() {
        let layout = Sub {
            row_bytes: 3,
            rows: 2,
            depth: 2,
            size: 12,
            ..Default::default()
        };
        let physical = vec![Write {
            offset: 1,
            bytes: (1..14).collect(),
        }];
        let writes = canonical(&layout, Pitches { row: 4, depth: 10 }, &physical);
        assert_eq!(writes.len(), 1);
        assert_eq!(writes[0].offset, 1);
        assert_eq!(writes[0].bytes, [1, 2, 4, 5, 6, 10, 11, 12]);
        let mut dst = [0xcc; 32];
        unsafe { copy_writes(&layout, Pitches { row: 5, depth: 16 }, &writes, dst.as_mut_ptr()) };
        assert_eq!(&dst[..8], &[0xcc, 1, 2, 0xcc, 0xcc, 4, 5, 6]);
        assert_eq!(&dst[16..19], &[10, 11, 12]);
        assert!(dst[8..16].iter().all(|&b| b == 0xcc));
        assert!(dst[19..].iter().all(|&b| b == 0xcc));
    }
}
