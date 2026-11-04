//! Hidden append/consume counters belong to views, separately from resource bytes.

use windows::Win32::Graphics::Direct3D11::*;

use super::state;
use locum_d3d11::predication::unpredicated;

pub(super) fn track(id: u64, view: &ID3D11UnorderedAccessView) {
    let mut desc = D3D11_UNORDERED_ACCESS_VIEW_DESC::default();
    unsafe { view.GetDesc(&mut desc) };
    if desc.ViewDimension == D3D11_UAV_DIMENSION_BUFFER
        && unsafe { desc.Anonymous.Buffer.Flags }
            & (D3D11_BUFFER_UAV_FLAG_APPEND.0 | D3D11_BUFFER_UAV_FLAG_COUNTER.0) as u32
            != 0
    {
        let mut s = state();
        s.counter_views.insert(id, view.clone());
        s.counter_seen.insert(id, view.clone());
    }
}

/// Called before the first event using a counter runs on the immediate context.
pub(super) fn touch(c: &ID3D11DeviceContext4, ids: &[u64]) {
    let todo: Vec<_> = {
        let mut s = state();
        ids.iter()
            .filter_map(|id| s.counter_views.remove(id).map(|v| (*id, v)))
            .collect()
    };
    for (id, view) in todo {
        let copied = issue(c, &view).map_err(|e| e.to_string());
        state().counter_copies.insert(id, copied);
    }
}

fn issue(c: &ID3D11DeviceContext4, view: &ID3D11UnorderedAccessView) -> windows::core::Result<ID3D11Buffer> {
    let device = unsafe { c.GetDevice() }?;
    let desc = D3D11_BUFFER_DESC {
        ByteWidth: 4,
        Usage: D3D11_USAGE_STAGING,
        CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
        ..Default::default()
    };
    let mut buffer = None;
    unsafe { device.CreateBuffer(&desc, None, Some(&mut buffer)) }?;
    let buffer = buffer.expect("a successful CreateBuffer returns a buffer");
    unsafe { unpredicated(c, || c.CopyStructureCount(&buffer, 0, view)) };
    Ok(buffer)
}

pub(super) fn read(c: &ID3D11DeviceContext4, buffer: &ID3D11Buffer) -> Result<u32, String> {
    let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
    unsafe { c.Map(buffer, 0, D3D11_MAP_READ, 0, Some(&mut mapped)) }.map_err(|e| e.to_string())?;
    let count = unsafe { mapped.pData.cast::<u32>().read_unaligned() };
    unsafe { c.Unmap(buffer, 0) };
    Ok(count)
}
