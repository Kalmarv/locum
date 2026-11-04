//! Generated D3D12 interface dispatch.
use core::ffi::c_void;
use windows::Win32::Graphics::Direct3D12::*;
use windows::core::{GUID, Interface};
pub fn table(iid: &GUID) -> Option<(*const c_void, &'static str)> {
    static D12_DEBUG_BASE: ID3D12Debug6_Vtbl = super::d12_debug_base::vtable::<crate::d3d12::Object>();
    if *iid == ID3D12Debug::IID {
        return Some((&D12_DEBUG_BASE as *const _ as *const c_void, "ID3D12Debug"));
    }
    if *iid == ID3D12Debug3::IID {
        return Some((&D12_DEBUG_BASE as *const _ as *const c_void, "ID3D12Debug3"));
    }
    if *iid == ID3D12Debug4::IID {
        return Some((&D12_DEBUG_BASE as *const _ as *const c_void, "ID3D12Debug4"));
    }
    if *iid == ID3D12Debug5::IID {
        return Some((&D12_DEBUG_BASE as *const _ as *const c_void, "ID3D12Debug5"));
    }
    if *iid == ID3D12Debug6::IID {
        return Some((&D12_DEBUG_BASE as *const _ as *const c_void, "ID3D12Debug6"));
    }
    static D12_DEVICE: ID3D12Device14_Vtbl = super::d12_device::vtable::<crate::d3d12::Object>();
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
    if *iid == ID3D12Device6::IID {
        return Some((&D12_DEVICE as *const _ as *const c_void, "ID3D12Device6"));
    }
    if *iid == ID3D12Device7::IID {
        return Some((&D12_DEVICE as *const _ as *const c_void, "ID3D12Device7"));
    }
    if *iid == ID3D12Device8::IID {
        return Some((&D12_DEVICE as *const _ as *const c_void, "ID3D12Device8"));
    }
    if *iid == ID3D12Device9::IID {
        return Some((&D12_DEVICE as *const _ as *const c_void, "ID3D12Device9"));
    }
    if *iid == ID3D12Device10::IID {
        return Some((&D12_DEVICE as *const _ as *const c_void, "ID3D12Device10"));
    }
    if *iid == ID3D12Device11::IID {
        return Some((&D12_DEVICE as *const _ as *const c_void, "ID3D12Device11"));
    }
    if *iid == ID3D12Device12::IID {
        return Some((&D12_DEVICE as *const _ as *const c_void, "ID3D12Device12"));
    }
    if *iid == ID3D12Device13::IID {
        return Some((&D12_DEVICE as *const _ as *const c_void, "ID3D12Device13"));
    }
    if *iid == ID3D12Device14::IID {
        return Some((&D12_DEVICE as *const _ as *const c_void, "ID3D12Device14"));
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
    static D12_LIST: ID3D12GraphicsCommandList10_Vtbl = super::d12_list::vtable::<crate::d3d12::Object>();
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
    if *iid == ID3D12GraphicsCommandList5::IID {
        return Some((
            &D12_LIST as *const _ as *const c_void,
            "ID3D12GraphicsCommandList5",
        ));
    }
    if *iid == ID3D12GraphicsCommandList6::IID {
        return Some((
            &D12_LIST as *const _ as *const c_void,
            "ID3D12GraphicsCommandList6",
        ));
    }
    if *iid == ID3D12GraphicsCommandList7::IID {
        return Some((
            &D12_LIST as *const _ as *const c_void,
            "ID3D12GraphicsCommandList7",
        ));
    }
    if *iid == ID3D12GraphicsCommandList8::IID {
        return Some((
            &D12_LIST as *const _ as *const c_void,
            "ID3D12GraphicsCommandList8",
        ));
    }
    if *iid == ID3D12GraphicsCommandList9::IID {
        return Some((
            &D12_LIST as *const _ as *const c_void,
            "ID3D12GraphicsCommandList9",
        ));
    }
    if *iid == ID3D12GraphicsCommandList10::IID {
        return Some((
            &D12_LIST as *const _ as *const c_void,
            "ID3D12GraphicsCommandList10",
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
    static D12_PROPERTIES: ID3D12StateObjectProperties1_Vtbl =
        super::d12_properties::vtable::<crate::d3d12::Object>();
    if *iid == ID3D12StateObjectProperties::IID {
        return Some((
            &D12_PROPERTIES as *const _ as *const c_void,
            "ID3D12StateObjectProperties",
        ));
    }
    if *iid == ID3D12StateObjectProperties1::IID {
        return Some((
            &D12_PROPERTIES as *const _ as *const c_void,
            "ID3D12StateObjectProperties1",
        ));
    }
    static D12_DEBUG: ID3D12Debug1_Vtbl = super::d12_debug::vtable::<crate::d3d12::Object>();
    if *iid == ID3D12Debug1::IID {
        return Some((&D12_DEBUG as *const _ as *const c_void, "ID3D12Debug1"));
    }
    static D12_INFO: ID3D12InfoQueue1_Vtbl = super::d12_info::vtable::<crate::d3d12::Object>();
    if *iid == ID3D12InfoQueue::IID {
        return Some((&D12_INFO as *const _ as *const c_void, "ID3D12InfoQueue"));
    }
    if *iid == ID3D12InfoQueue1::IID {
        return Some((&D12_INFO as *const _ as *const c_void, "ID3D12InfoQueue1"));
    }
    static D12_PROTECTED: ID3D12ProtectedResourceSession1_Vtbl =
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
    if *iid == ID3D12ProtectedResourceSession1::IID {
        return Some((
            &D12_PROTECTED as *const _ as *const c_void,
            "ID3D12ProtectedResourceSession1",
        ));
    }
    static D12_META: ID3D12MetaCommand_Vtbl = super::d12_meta::vtable::<crate::d3d12::Object>();
    if *iid == ID3D12MetaCommand::IID {
        return Some((&D12_META as *const _ as *const c_void, "ID3D12MetaCommand"));
    }
    static D12_CACHE: ID3D12ShaderCacheSession_Vtbl = super::d12_cache::vtable::<crate::d3d12::Object>();
    if *iid == ID3D12ShaderCacheSession::IID {
        return Some((
            &D12_CACHE as *const _ as *const c_void,
            "ID3D12ShaderCacheSession",
        ));
    }
    static D12_WORK_GRAPH: ID3D12WorkGraphProperties_Vtbl =
        super::d12_work_graph::vtable::<crate::d3d12::Object>();
    if *iid == ID3D12WorkGraphProperties::IID {
        return Some((
            &D12_WORK_GRAPH as *const _ as *const c_void,
            "ID3D12WorkGraphProperties",
        ));
    }
    static D12_DEBUG2: ID3D12Debug2_Vtbl = super::d12_debug2::vtable::<crate::d3d12::Object>();
    if *iid == ID3D12Debug2::IID {
        return Some((&D12_DEBUG2 as *const _ as *const c_void, "ID3D12Debug2"));
    }
    static D12_DEBUG_DEVICE: ID3D12DebugDevice2_Vtbl =
        super::d12_debug_device::vtable::<crate::d3d12::Object>();
    if *iid == ID3D12DebugDevice::IID {
        return Some((
            &D12_DEBUG_DEVICE as *const _ as *const c_void,
            "ID3D12DebugDevice",
        ));
    }
    if *iid == ID3D12DebugDevice2::IID {
        return Some((
            &D12_DEBUG_DEVICE as *const _ as *const c_void,
            "ID3D12DebugDevice2",
        ));
    }
    static D12_DEBUG_DEVICE1: ID3D12DebugDevice1_Vtbl =
        super::d12_debug_device1::vtable::<crate::d3d12::Object>();
    if *iid == ID3D12DebugDevice1::IID {
        return Some((
            &D12_DEBUG_DEVICE1 as *const _ as *const c_void,
            "ID3D12DebugDevice1",
        ));
    }
    static D12_DEBUG_QUEUE: ID3D12DebugCommandQueue1_Vtbl =
        super::d12_debug_queue::vtable::<crate::d3d12::Object>();
    if *iid == ID3D12DebugCommandQueue::IID {
        return Some((
            &D12_DEBUG_QUEUE as *const _ as *const c_void,
            "ID3D12DebugCommandQueue",
        ));
    }
    if *iid == ID3D12DebugCommandQueue1::IID {
        return Some((
            &D12_DEBUG_QUEUE as *const _ as *const c_void,
            "ID3D12DebugCommandQueue1",
        ));
    }
    static D12_DEBUG_LIST: ID3D12DebugCommandList3_Vtbl =
        super::d12_debug_list::vtable::<crate::d3d12::Object>();
    if *iid == ID3D12DebugCommandList::IID {
        return Some((
            &D12_DEBUG_LIST as *const _ as *const c_void,
            "ID3D12DebugCommandList",
        ));
    }
    if *iid == ID3D12DebugCommandList2::IID {
        return Some((
            &D12_DEBUG_LIST as *const _ as *const c_void,
            "ID3D12DebugCommandList2",
        ));
    }
    if *iid == ID3D12DebugCommandList3::IID {
        return Some((
            &D12_DEBUG_LIST as *const _ as *const c_void,
            "ID3D12DebugCommandList3",
        ));
    }
    static D12_DEBUG_LIST1: ID3D12DebugCommandList1_Vtbl =
        super::d12_debug_list1::vtable::<crate::d3d12::Object>();
    if *iid == ID3D12DebugCommandList1::IID {
        return Some((
            &D12_DEBUG_LIST1 as *const _ as *const c_void,
            "ID3D12DebugCommandList1",
        ));
    }
    static D12_DRED: ID3D12DeviceRemovedExtendedData2_Vtbl =
        super::d12_dred::vtable::<crate::d3d12::Object>();
    if *iid == ID3D12DeviceRemovedExtendedData::IID {
        return Some((
            &D12_DRED as *const _ as *const c_void,
            "ID3D12DeviceRemovedExtendedData",
        ));
    }
    if *iid == ID3D12DeviceRemovedExtendedData1::IID {
        return Some((
            &D12_DRED as *const _ as *const c_void,
            "ID3D12DeviceRemovedExtendedData1",
        ));
    }
    if *iid == ID3D12DeviceRemovedExtendedData2::IID {
        return Some((
            &D12_DRED as *const _ as *const c_void,
            "ID3D12DeviceRemovedExtendedData2",
        ));
    }
    static D12_DRED_SETTINGS: ID3D12DeviceRemovedExtendedDataSettings2_Vtbl =
        super::d12_dred_settings::vtable::<crate::d3d12::Object>();
    if *iid == ID3D12DeviceRemovedExtendedDataSettings::IID {
        return Some((
            &D12_DRED_SETTINGS as *const _ as *const c_void,
            "ID3D12DeviceRemovedExtendedDataSettings",
        ));
    }
    if *iid == ID3D12DeviceRemovedExtendedDataSettings1::IID {
        return Some((
            &D12_DRED_SETTINGS as *const _ as *const c_void,
            "ID3D12DeviceRemovedExtendedDataSettings1",
        ));
    }
    if *iid == ID3D12DeviceRemovedExtendedDataSettings2::IID {
        return Some((
            &D12_DRED_SETTINGS as *const _ as *const c_void,
            "ID3D12DeviceRemovedExtendedDataSettings2",
        ));
    }
    static D12_SDK: ID3D12SDKConfiguration1_Vtbl = super::d12_sdk::vtable::<crate::d3d12::Object>();
    if *iid == ID3D12SDKConfiguration::IID {
        return Some((&D12_SDK as *const _ as *const c_void, "ID3D12SDKConfiguration"));
    }
    if *iid == ID3D12SDKConfiguration1::IID {
        return Some((&D12_SDK as *const _ as *const c_void, "ID3D12SDKConfiguration1"));
    }
    static D12_FACTORY: ID3D12DeviceFactory_Vtbl = super::d12_factory::vtable::<crate::d3d12::Object>();
    if *iid == ID3D12DeviceFactory::IID {
        return Some((&D12_FACTORY as *const _ as *const c_void, "ID3D12DeviceFactory"));
    }
    static D12_CONFIGURATION: ID3D12DeviceConfiguration1_Vtbl =
        super::d12_configuration::vtable::<crate::d3d12::Object>();
    if *iid == ID3D12DeviceConfiguration::IID {
        return Some((
            &D12_CONFIGURATION as *const _ as *const c_void,
            "ID3D12DeviceConfiguration",
        ));
    }
    if *iid == ID3D12DeviceConfiguration1::IID {
        return Some((
            &D12_CONFIGURATION as *const _ as *const c_void,
            "ID3D12DeviceConfiguration1",
        ));
    }
    static D12_TOOLS: ID3D12Tools_Vtbl = super::d12_tools::vtable::<crate::d3d12::Object>();
    if *iid == ID3D12Tools::IID {
        return Some((&D12_TOOLS as *const _ as *const c_void, "ID3D12Tools"));
    }
    static D12_SHARING: ID3D12SharingContract_Vtbl = super::d12_sharing::vtable::<crate::d3d12::Object>();
    if *iid == ID3D12SharingContract::IID {
        return Some((&D12_SHARING as *const _ as *const c_void, "ID3D12SharingContract"));
    }
    static D12_GBV: ID3D12GBVDiagnostics_Vtbl = super::d12_gbv::vtable::<crate::d3d12::Object>();
    if *iid == ID3D12GBVDiagnostics::IID {
        return Some((&D12_GBV as *const _ as *const c_void, "ID3D12GBVDiagnostics"));
    }
    static D12_WRITE_TRACKING: ID3D12ManualWriteTrackingResource_Vtbl =
        super::d12_write_tracking::vtable::<crate::d3d12::Object>();
    if *iid == ID3D12ManualWriteTrackingResource::IID {
        return Some((
            &D12_WRITE_TRACKING as *const _ as *const c_void,
            "ID3D12ManualWriteTrackingResource",
        ));
    }
    static D12_ASSISTANT: ID3D12SwapChainAssistant_Vtbl =
        super::d12_assistant::vtable::<crate::d3d12::Object>();
    if *iid == ID3D12SwapChainAssistant::IID {
        return Some((
            &D12_ASSISTANT as *const _ as *const c_void,
            "ID3D12SwapChainAssistant",
        ));
    }
    static D12_GUEST: ID3D12VirtualizationGuestDevice_Vtbl =
        super::d12_guest::vtable::<crate::d3d12::Object>();
    if *iid == ID3D12VirtualizationGuestDevice::IID {
        return Some((
            &D12_GUEST as *const _ as *const c_void,
            "ID3D12VirtualizationGuestDevice",
        ));
    }
    static D12_LIFETIME_OWNER: ID3D12LifetimeOwner_Vtbl =
        super::d12_lifetime_owner::vtable::<crate::d3d12::Object>();
    if *iid == ID3D12LifetimeOwner::IID {
        return Some((
            &D12_LIFETIME_OWNER as *const _ as *const c_void,
            "ID3D12LifetimeOwner",
        ));
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
