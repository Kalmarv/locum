//! Generated D3D12 interface dispatch.
use core::ffi::c_void;
use windows::Win32::Graphics::Direct3D12::*;
use windows::core::{GUID, Interface};
pub fn table(iid: &GUID) -> Option<(*const c_void, &'static str)> {
    static D12_DEBUG_BASE: ID3D12Debug_Vtbl = super::d12_debug_base::vtable::<crate::d3d12::Object>();
    if *iid == ID3D12Debug::IID {
        return Some((&D12_DEBUG_BASE as *const _ as *const c_void, "ID3D12Debug"));
    }
    static D12_DEVICE: ID3D12Device5_Vtbl = super::d12_device::vtable::<crate::d3d12::Object>();
    if *iid == ID3D12Object::IID {
        return Some((&D12_DEVICE as *const _ as *const c_void, "ID3D12Object"));
    }
    if *iid == ID3D12Device::IID {
        return Some((&D12_DEVICE as *const _ as *const c_void, "ID3D12Device"));
    }
    if *iid == ID3D12Device1::IID {
        return Some((&D12_DEVICE as *const _ as *const c_void, "ID3D12Device1"));
    }
    if *iid == ID3D12Device2::IID {
        return Some((&D12_DEVICE as *const _ as *const c_void, "ID3D12Device2"));
    }
    if *iid == ID3D12Device3::IID {
        return Some((&D12_DEVICE as *const _ as *const c_void, "ID3D12Device3"));
    }
    if *iid == ID3D12Device4::IID {
        return Some((&D12_DEVICE as *const _ as *const c_void, "ID3D12Device4"));
    }
    if *iid == ID3D12Device5::IID {
        return Some((&D12_DEVICE as *const _ as *const c_void, "ID3D12Device5"));
    }
    static D12_QUEUE: ID3D12CommandQueue_Vtbl = super::d12_queue::vtable::<crate::d3d12::Object>();
    if *iid == ID3D12DeviceChild::IID {
        return Some((&D12_QUEUE as *const _ as *const c_void, "ID3D12DeviceChild"));
    }
    if *iid == ID3D12Pageable::IID {
        return Some((&D12_QUEUE as *const _ as *const c_void, "ID3D12Pageable"));
    }
    if *iid == ID3D12CommandQueue::IID {
        return Some((&D12_QUEUE as *const _ as *const c_void, "ID3D12CommandQueue"));
    }
    static D12_LIST: ID3D12GraphicsCommandList4_Vtbl = super::d12_list::vtable::<crate::d3d12::Object>();
    if *iid == ID3D12CommandList::IID {
        return Some((&D12_LIST as *const _ as *const c_void, "ID3D12CommandList"));
    }
    if *iid == ID3D12GraphicsCommandList::IID {
        return Some((
            &D12_LIST as *const _ as *const c_void,
            "ID3D12GraphicsCommandList",
        ));
    }
    if *iid == ID3D12GraphicsCommandList1::IID {
        return Some((
            &D12_LIST as *const _ as *const c_void,
            "ID3D12GraphicsCommandList1",
        ));
    }
    if *iid == ID3D12GraphicsCommandList2::IID {
        return Some((
            &D12_LIST as *const _ as *const c_void,
            "ID3D12GraphicsCommandList2",
        ));
    }
    if *iid == ID3D12GraphicsCommandList3::IID {
        return Some((
            &D12_LIST as *const _ as *const c_void,
            "ID3D12GraphicsCommandList3",
        ));
    }
    if *iid == ID3D12GraphicsCommandList4::IID {
        return Some((
            &D12_LIST as *const _ as *const c_void,
            "ID3D12GraphicsCommandList4",
        ));
    }
    static D12_ALLOCATOR: ID3D12CommandAllocator_Vtbl =
        super::d12_allocator::vtable::<crate::d3d12::Object>();
    if *iid == ID3D12CommandAllocator::IID {
        return Some((
            &D12_ALLOCATOR as *const _ as *const c_void,
            "ID3D12CommandAllocator",
        ));
    }
    static D12_RESOURCE: ID3D12Resource2_Vtbl = super::d12_resource::vtable::<crate::d3d12::Object>();
    if *iid == ID3D12Resource::IID {
        return Some((&D12_RESOURCE as *const _ as *const c_void, "ID3D12Resource"));
    }
    if *iid == ID3D12Resource1::IID {
        return Some((&D12_RESOURCE as *const _ as *const c_void, "ID3D12Resource1"));
    }
    if *iid == ID3D12Resource2::IID {
        return Some((&D12_RESOURCE as *const _ as *const c_void, "ID3D12Resource2"));
    }
    static D12_HEAP: ID3D12Heap1_Vtbl = super::d12_heap::vtable::<crate::d3d12::Object>();
    if *iid == ID3D12Heap::IID {
        return Some((&D12_HEAP as *const _ as *const c_void, "ID3D12Heap"));
    }
    if *iid == ID3D12Heap1::IID {
        return Some((&D12_HEAP as *const _ as *const c_void, "ID3D12Heap1"));
    }
    static D12_FENCE: ID3D12Fence1_Vtbl = super::d12_fence::vtable::<crate::d3d12::Object>();
    if *iid == ID3D12Fence::IID {
        return Some((&D12_FENCE as *const _ as *const c_void, "ID3D12Fence"));
    }
    if *iid == ID3D12Fence1::IID {
        return Some((&D12_FENCE as *const _ as *const c_void, "ID3D12Fence1"));
    }
    static D12_DESCRIPTORS: ID3D12DescriptorHeap_Vtbl =
        super::d12_descriptors::vtable::<crate::d3d12::Object>();
    if *iid == ID3D12DescriptorHeap::IID {
        return Some((
            &D12_DESCRIPTORS as *const _ as *const c_void,
            "ID3D12DescriptorHeap",
        ));
    }
    static D12_ROOT: ID3D12RootSignature_Vtbl = super::d12_root::vtable::<crate::d3d12::Object>();
    if *iid == ID3D12RootSignature::IID {
        return Some((&D12_ROOT as *const _ as *const c_void, "ID3D12RootSignature"));
    }
    static D12_PSO: ID3D12PipelineState_Vtbl = super::d12_pso::vtable::<crate::d3d12::Object>();
    if *iid == ID3D12PipelineState::IID {
        return Some((&D12_PSO as *const _ as *const c_void, "ID3D12PipelineState"));
    }
    static D12_SIGNATURE: ID3D12CommandSignature_Vtbl =
        super::d12_signature::vtable::<crate::d3d12::Object>();
    if *iid == ID3D12CommandSignature::IID {
        return Some((
            &D12_SIGNATURE as *const _ as *const c_void,
            "ID3D12CommandSignature",
        ));
    }
    static D12_QUERY: ID3D12QueryHeap_Vtbl = super::d12_query::vtable::<crate::d3d12::Object>();
    if *iid == ID3D12QueryHeap::IID {
        return Some((&D12_QUERY as *const _ as *const c_void, "ID3D12QueryHeap"));
    }
    static D12_LIBRARY: ID3D12PipelineLibrary1_Vtbl = super::d12_library::vtable::<crate::d3d12::Object>();
    if *iid == ID3D12PipelineLibrary::IID {
        return Some((&D12_LIBRARY as *const _ as *const c_void, "ID3D12PipelineLibrary"));
    }
    if *iid == ID3D12PipelineLibrary1::IID {
        return Some((
            &D12_LIBRARY as *const _ as *const c_void,
            "ID3D12PipelineLibrary1",
        ));
    }
    static D12_STATE: ID3D12StateObject_Vtbl = super::d12_state::vtable::<crate::d3d12::Object>();
    if *iid == ID3D12StateObject::IID {
        return Some((&D12_STATE as *const _ as *const c_void, "ID3D12StateObject"));
    }
    static D12_PROPERTIES: ID3D12StateObjectProperties_Vtbl =
        super::d12_properties::vtable::<crate::d3d12::Object>();
    if *iid == ID3D12StateObjectProperties::IID {
        return Some((
            &D12_PROPERTIES as *const _ as *const c_void,
            "ID3D12StateObjectProperties",
        ));
    }
    static D12_DEBUG: ID3D12Debug1_Vtbl = super::d12_debug::vtable::<crate::d3d12::Object>();
    if *iid == ID3D12Debug1::IID {
        return Some((&D12_DEBUG as *const _ as *const c_void, "ID3D12Debug1"));
    }
    static D12_INFO: ID3D12InfoQueue_Vtbl = super::d12_info::vtable::<crate::d3d12::Object>();
    if *iid == ID3D12InfoQueue::IID {
        return Some((&D12_INFO as *const _ as *const c_void, "ID3D12InfoQueue"));
    }
    static D12_PROTECTED: ID3D12ProtectedResourceSession_Vtbl =
        super::d12_protected::vtable::<crate::d3d12::Object>();
    if *iid == ID3D12ProtectedSession::IID {
        return Some((
            &D12_PROTECTED as *const _ as *const c_void,
            "ID3D12ProtectedSession",
        ));
    }
    if *iid == ID3D12ProtectedResourceSession::IID {
        return Some((
            &D12_PROTECTED as *const _ as *const c_void,
            "ID3D12ProtectedResourceSession",
        ));
    }
    static D12_META: ID3D12MetaCommand_Vtbl = super::d12_meta::vtable::<crate::d3d12::Object>();
    if *iid == ID3D12MetaCommand::IID {
        return Some((&D12_META as *const _ as *const c_void, "ID3D12MetaCommand"));
    }
    static D12_LIFETIME: ID3D12LifetimeTracker_Vtbl = super::d12_lifetime::vtable::<crate::d3d12::Object>();
    if *iid == ID3D12LifetimeTracker::IID {
        return Some((
            &D12_LIFETIME as *const _ as *const c_void,
            "ID3D12LifetimeTracker",
        ));
    }
    None
}
