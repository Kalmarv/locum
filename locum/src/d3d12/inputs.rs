//! Borrowed ABI copies: replace only COM fields, leaving the application's structs
//! and reference counts untouched. ManuallyDrop prevents shallow copies releasing
//! interfaces owned by the caller. The underlying call consumes them synchronously.
#![allow(clippy::missing_safety_doc)]
use core::{
    ffi::c_void,
    mem::{ManuallyDrop, align_of, size_of},
};
use windows::Win32::Graphics::Direct3D12::*;

pub struct Input<T> {
    values: Vec<ManuallyDrop<T>>,
    null: bool,
}
impl<T> Input<T> {
    pub fn ptr(&self) -> *const T {
        if self.null {
            core::ptr::null()
        } else {
            self.values.as_ptr().cast()
        }
    }
}
unsafe fn copy<T>(p: *const T, count: usize) -> Input<T> {
    let values = if p.is_null() {
        Vec::new()
    } else {
        (0..count)
            .map(|i| ManuallyDrop::new(unsafe { core::ptr::read(p.add(i)) }))
            .collect()
    };
    Input {
        values,
        null: p.is_null(),
    }
}
unsafe fn field<T>(p: *mut T) {
    let raw = p.cast::<*mut c_void>();
    unsafe { *raw = super::unwrap(*raw) };
}
pub unsafe fn objects(p: *const *mut c_void, count: u32) -> Input<*mut c_void> {
    let mut out = unsafe { copy(p, count as usize) };
    for v in &mut out.values {
        **v = super::unwrap(**v);
    }
    out
}
pub unsafe fn graphics(
    p: *const D3D12_GRAPHICS_PIPELINE_STATE_DESC,
) -> Input<D3D12_GRAPHICS_PIPELINE_STATE_DESC> {
    let mut out = unsafe { copy(p, 1) };
    for v in &mut out.values {
        unsafe { field(&mut v.pRootSignature) };
    }
    out
}
pub unsafe fn compute(
    p: *const D3D12_COMPUTE_PIPELINE_STATE_DESC,
) -> Input<D3D12_COMPUTE_PIPELINE_STATE_DESC> {
    let mut out = unsafe { copy(p, 1) };
    for v in &mut out.values {
        unsafe { field(&mut v.pRootSignature) };
    }
    out
}
pub unsafe fn texture_copy(p: *const D3D12_TEXTURE_COPY_LOCATION) -> Input<D3D12_TEXTURE_COPY_LOCATION> {
    let mut out = unsafe { copy(p, 1) };
    for v in &mut out.values {
        unsafe { field(&mut v.pResource) };
    }
    out
}
pub unsafe fn barriers(p: *const D3D12_RESOURCE_BARRIER, count: u32) -> Input<D3D12_RESOURCE_BARRIER> {
    let mut out = unsafe { copy(p, count as usize) };
    for v in &mut out.values {
        unsafe {
            match v.Type {
                D3D12_RESOURCE_BARRIER_TYPE_TRANSITION => field(&mut (*v.Anonymous.Transition).pResource),
                D3D12_RESOURCE_BARRIER_TYPE_ALIASING => {
                    field(&mut (*v.Anonymous.Aliasing).pResourceBefore);
                    field(&mut (*v.Anonymous.Aliasing).pResourceAfter);
                }
                D3D12_RESOURCE_BARRIER_TYPE_UAV => field(&mut (*v.Anonymous.UAV).pResource),
                _ => {}
            }
        }
    }
    out
}
unsafe fn ending(e: &mut D3D12_RENDER_PASS_ENDING_ACCESS) {
    if e.Type == D3D12_RENDER_PASS_ENDING_ACCESS_TYPE_RESOLVE {
        unsafe {
            field(&mut (*e.Anonymous.Resolve).pSrcResource);
            field(&mut (*e.Anonymous.Resolve).pDstResource);
        }
    }
}
pub unsafe fn render_targets(
    p: *const D3D12_RENDER_PASS_RENDER_TARGET_DESC,
    n: u32,
) -> Input<D3D12_RENDER_PASS_RENDER_TARGET_DESC> {
    let mut out = unsafe { copy(p, n as usize) };
    for v in &mut out.values {
        unsafe { ending(&mut v.EndingAccess) };
    }
    out
}
pub unsafe fn depth_stencil(
    p: *const D3D12_RENDER_PASS_DEPTH_STENCIL_DESC,
) -> Input<D3D12_RENDER_PASS_DEPTH_STENCIL_DESC> {
    let mut out = unsafe { copy(p, 1) };
    for v in &mut out.values {
        unsafe {
            ending(&mut v.DepthEndingAccess);
            ending(&mut v.StencilEndingAccess);
        }
    }
    out
}
fn align(n: usize, a: usize) -> usize {
    (n + a - 1) & !(a - 1)
}
pub(super) fn payload(t: D3D12_PIPELINE_STATE_SUBOBJECT_TYPE) -> Option<(usize, usize)> {
    macro_rules! ty {
        ($t:ty) => {
            Some((size_of::<$t>(), align_of::<$t>()))
        };
    }
    match t {
        D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_ROOT_SIGNATURE => ty!(*mut c_void),
        D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_VS
        | D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_PS
        | D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_DS
        | D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_HS
        | D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_GS
        | D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_CS
        | D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_AS
        | D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_MS => ty!(D3D12_SHADER_BYTECODE),
        D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_STREAM_OUTPUT => ty!(D3D12_STREAM_OUTPUT_DESC),
        D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_BLEND => ty!(D3D12_BLEND_DESC),
        D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_SAMPLE_MASK | D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_NODE_MASK => {
            ty!(u32)
        }
        D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_RASTERIZER => ty!(D3D12_RASTERIZER_DESC),
        D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_RASTERIZER1 => ty!(D3D12_RASTERIZER_DESC1),
        D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_RASTERIZER2 => ty!(D3D12_RASTERIZER_DESC2),
        D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_DEPTH_STENCIL => ty!(D3D12_DEPTH_STENCIL_DESC),
        D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_DEPTH_STENCIL1 => ty!(D3D12_DEPTH_STENCIL_DESC1),
        D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_DEPTH_STENCIL2 => ty!(D3D12_DEPTH_STENCIL_DESC2),
        D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_INPUT_LAYOUT => ty!(D3D12_INPUT_LAYOUT_DESC),
        D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_IB_STRIP_CUT_VALUE => ty!(D3D12_INDEX_BUFFER_STRIP_CUT_VALUE),
        D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_PRIMITIVE_TOPOLOGY => ty!(D3D12_PRIMITIVE_TOPOLOGY_TYPE),
        D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_RENDER_TARGET_FORMATS => ty!(D3D12_RT_FORMAT_ARRAY),
        D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_DEPTH_STENCIL_FORMAT => {
            ty!(windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT)
        }
        D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_SAMPLE_DESC => {
            ty!(windows::Win32::Graphics::Dxgi::Common::DXGI_SAMPLE_DESC)
        }
        D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_CACHED_PSO => ty!(D3D12_CACHED_PIPELINE_STATE),
        D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_FLAGS => ty!(D3D12_PIPELINE_STATE_FLAGS),
        D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_VIEW_INSTANCING => ty!(D3D12_VIEW_INSTANCING_DESC),
        _ => None,
    }
}
pub struct Stream {
    desc: Input<D3D12_PIPELINE_STATE_STREAM_DESC>,
    _words: Vec<usize>,
}
impl Stream {
    pub fn ptr(&self) -> *const D3D12_PIPELINE_STATE_STREAM_DESC {
        self.desc.ptr()
    }
}
pub unsafe fn stream(p: *const D3D12_PIPELINE_STATE_STREAM_DESC) -> Stream {
    let mut desc = unsafe { copy(p, 1) };
    let mut words = Vec::new();
    if let Some(d) = desc.values.first_mut()
        && !d.pPipelineStateSubobjectStream.is_null()
    {
        words.resize(d.SizeInBytes.div_ceil(size_of::<usize>()), 0);
        let bytes = words.as_mut_ptr().cast::<u8>();
        unsafe {
            core::ptr::copy_nonoverlapping(d.pPipelineStateSubobjectStream.cast::<u8>(), bytes, d.SizeInBytes)
        };
        let mut at = 0;
        while at + size_of::<D3D12_PIPELINE_STATE_SUBOBJECT_TYPE>() <= d.SizeInBytes {
            let t = unsafe { bytes.add(at).cast::<D3D12_PIPELINE_STATE_SUBOBJECT_TYPE>().read() };
            let Some((size, alignment)) = payload(t) else {
                break;
            };
            let offset = at + align(4, alignment);
            if offset + size > d.SizeInBytes {
                break;
            }
            if t == D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_ROOT_SIGNATURE {
                unsafe { field(bytes.add(offset).cast::<*mut c_void>()) };
            }
            at = align(offset + size, size_of::<usize>());
        }
        d.pPipelineStateSubobjectStream = bytes.cast();
    }
    Stream { desc, _words: words }
}

// Subobjects point into these boxes while the vectors can still grow.
#[allow(clippy::vec_box)]
pub struct StateObject {
    desc: Input<D3D12_STATE_OBJECT_DESC>,
    _subobjects: Input<D3D12_STATE_SUBOBJECT>,
    _roots: Vec<Box<ManuallyDrop<D3D12_GLOBAL_ROOT_SIGNATURE>>>,
    _collections: Vec<Box<ManuallyDrop<D3D12_EXISTING_COLLECTION_DESC>>>,
    _associations: Vec<Box<D3D12_SUBOBJECT_TO_EXPORTS_ASSOCIATION>>,
}
impl StateObject {
    pub fn ptr(&self) -> *const D3D12_STATE_OBJECT_DESC {
        self.desc.ptr()
    }
}
pub unsafe fn state_object(p: *const D3D12_STATE_OBJECT_DESC) -> StateObject {
    let mut desc = unsafe { copy(p, 1) };
    let mut subs = unsafe { copy(core::ptr::null::<D3D12_STATE_SUBOBJECT>(), 0) };
    let mut roots = Vec::new();
    let mut collections = Vec::new();
    let mut associations = Vec::new();
    if let Some(d) = desc.values.first_mut() {
        let original = d.pSubobjects;
        subs = unsafe { copy(original, d.NumSubobjects as usize) };
        let new_base = subs.ptr();
        for sub in &mut subs.values {
            if sub.pDesc.is_null() {
                continue;
            }
            match sub.Type {
                D3D12_STATE_SUBOBJECT_TYPE_GLOBAL_ROOT_SIGNATURE
                | D3D12_STATE_SUBOBJECT_TYPE_LOCAL_ROOT_SIGNATURE => {
                    // Both root subobjects consist of precisely one COM pointer.
                    let mut root = Box::new(ManuallyDrop::new(unsafe {
                        sub.pDesc.cast::<D3D12_GLOBAL_ROOT_SIGNATURE>().read()
                    }));
                    unsafe { field(&mut root.pGlobalRootSignature) };
                    sub.pDesc = (&**root as *const D3D12_GLOBAL_ROOT_SIGNATURE).cast();
                    roots.push(root);
                }
                D3D12_STATE_SUBOBJECT_TYPE_EXISTING_COLLECTION => {
                    let mut c = Box::new(ManuallyDrop::new(unsafe {
                        sub.pDesc.cast::<D3D12_EXISTING_COLLECTION_DESC>().read()
                    }));
                    unsafe { field(&mut c.pExistingCollection) };
                    sub.pDesc = (&**c as *const D3D12_EXISTING_COLLECTION_DESC).cast();
                    collections.push(c);
                }
                D3D12_STATE_SUBOBJECT_TYPE_SUBOBJECT_TO_EXPORTS_ASSOCIATION => {
                    let mut a = Box::new(unsafe {
                        sub.pDesc.cast::<D3D12_SUBOBJECT_TO_EXPORTS_ASSOCIATION>().read()
                    });
                    let offset = (a.pSubobjectToAssociate as usize).wrapping_sub(original as usize);
                    if offset.is_multiple_of(size_of::<D3D12_STATE_SUBOBJECT>())
                        && offset / size_of::<D3D12_STATE_SUBOBJECT>() < d.NumSubobjects as usize
                    {
                        a.pSubobjectToAssociate =
                            unsafe { new_base.add(offset / size_of::<D3D12_STATE_SUBOBJECT>()) };
                    }
                    sub.pDesc = (&*a as *const D3D12_SUBOBJECT_TO_EXPORTS_ASSOCIATION).cast();
                    associations.push(a);
                }
                _ => {}
            }
        }
        d.pSubobjects = subs.ptr();
    }
    StateObject {
        desc,
        _subobjects: subs,
        _roots: roots,
        _collections: collections,
        _associations: associations,
    }
}
