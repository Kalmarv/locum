//! GPU addresses inside ExecuteIndirect's argument records (see [`locum_capture::d3d12::indirect`]). The addresses
//! are the game's; replay's buffers are elsewhere. Before each ExecuteIndirect, replay reads the records as its own
//! GPU holds them at that point (so records a dispatch of the frame wrote are read after it wrote them, as
//! gfxreconstruct patches at ExecuteCommandLists), maps every address to the game's buffer and offset by the
//! addresses the capture recorded at each buffer's creation (`gpu_address` in its description), and runs the
//! signature on a copy holding replay's addresses. The buffers the records name are among the event's reads (the
//! capture read the records after the frame), so the plan makes them, with their contents.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use locum_capture::d3d12::indirect::{Layout, addresses, layout, patch};
use serde_json::Value;
use windows::Win32::Graphics::Direct3D12::*;

use super::D3d12;
use crate::open::Capture;

/// The game's buffers by the GPU address each began at.
#[derive(Default)]
pub struct Addresses {
    starts: BTreeMap<u64, Vec<(i64, u64)>>,
    widest: u64,
}

impl Addresses {
    /// Every buffer of the capture with a recorded address (none in captures from before addresses were kept).
    pub fn of(c: &Capture) -> Addresses {
        let mut out = Addresses::default();
        for (id, d) in c.rows(
            "SELECT id, description FROM d12_objects WHERE kind LIKE 'ID3D12Resource%' \
             AND json_extract(description, '$.gpu_address') IS NOT NULL",
            [],
            |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)),
        ) {
            let Ok(d) = serde_json::from_str::<Value>(&d) else {
                continue;
            };
            let (Some(start), Some(width)) = (d["gpu_address"].as_u64(), d["width"].as_u64()) else {
                continue;
            };
            out.insert(id, start, width);
        }
        out
    }

    fn insert(&mut self, id: i64, start: u64, width: u64) {
        if start != 0 {
            self.starts.entry(start).or_default().push((id, width));
            self.widest = self.widest.max(width);
        }
    }

    pub fn is_empty(&self) -> bool {
        self.starts.is_empty()
    }

    /// The buffers holding the game's `address`, and its offset in each (several where placed buffers share memory).
    pub fn find(&self, address: u64) -> Vec<(i64, u64)> {
        self.starts
            .range(address.saturating_sub(self.widest)..=address)
            .flat_map(|(&start, buffers)| {
                buffers
                    .iter()
                    .filter(move |&&(_, width)| address - start < width)
                    .map(move |&(id, _)| (id, address - start))
            })
            .collect()
    }
}

/// What an ExecuteIndirect runs on.
pub(super) enum Arguments {
    /// The argument buffer as it is: its signature puts no address in a record.
    AsRecorded,
    /// A copy of its records with replay's addresses, from offset 0.
    Patched(ID3D12Resource),
    /// Nothing: replay could not place an address (the warning says why).
    Skip,
}

impl D3d12 {
    /// The arguments ExecuteIndirect event `seq` (`a`: signature, max count, argument buffer and offset, count buffer
    /// and offset) runs on. Reading the records runs what the open list holds, so it is called before the event
    /// records anything of its own.
    pub(super) fn indirect_arguments(
        &mut self,
        c: &Capture,
        seq: i64,
        a: &Value,
    ) -> Result<Arguments, String> {
        let u = |i: usize| a[i].as_u64().unwrap_or(0);
        let (signature, max, buffer, offset, count, count_offset) =
            (u(0) as i64, u(1), u(2) as i64, u(3), u(4) as i64, u(5));
        let Some(desc) = c
            .row(
                "SELECT description FROM d12_objects WHERE id = ?1 AND kind = 'ID3D12CommandSignature'",
                [signature],
                |r| r.get::<_, String>(0),
            )
            .and_then(|d| serde_json::from_str::<Value>(&d).ok())
        else {
            // signature() says so.
            return Ok(Arguments::AsRecorded);
        };
        let layout = match layout(&desc) {
            Ok(l) => l,
            Err(e) => {
                self.warn(format!(
                    "command signature #{signature} has {e}: its ExecuteIndirect is not replayed"
                ));
                return Ok(Arguments::Skip);
            }
        };
        if layout.addresses.is_empty() {
            return Ok(Arguments::AsRecorded);
        }
        let addresses = self.addresses.take().unwrap_or_else(|| Addresses::of(c));
        let result = self.patched(
            seq,
            &layout,
            &addresses,
            (max, buffer, offset, count, count_offset),
        );
        self.addresses = Some(addresses);
        result
    }

    fn patched(
        &mut self,
        seq: i64,
        layout: &Layout,
        addresses_of: &Addresses,
        (max, buffer, offset, count, count_offset): (u64, i64, u64, i64, u64),
    ) -> Result<Arguments, String> {
        if addresses_of.is_empty() {
            self.warn(
                "ExecuteIndirect with GPU addresses in its argument records is not replayed: the capture records no \
                 buffer's address (locum before 2026-11-05)",
            );
            return Ok(Arguments::Skip);
        }
        let records = if count == 0 {
            max
        } else {
            let n = self.read_range(count, count_offset, 4)?;
            max.min(u64::from(u32::from_le_bytes(n[..4].try_into().expect("4 bytes"))))
        };
        // The copy holds as many records as the call may run (the debug layer checks the buffer holds them); only
        // those the count lets it run are patched.
        let width = self.made(buffer)?.desc.Width;
        let bytes = (max * layout.stride as u64).min(width.saturating_sub(offset));
        let records = records.min(bytes / layout.stride.max(1) as u64);
        if records == 0 {
            return Ok(Arguments::AsRecorded);
        }
        let mut records_bytes = self.read_range(buffer, offset, bytes)?;
        // Every address the records hold, placed: the game's buffer and offset, and that buffer as replay made it.
        let mut placed: HashMap<u64, u64> = HashMap::new();
        let mut unplaced = None;
        addresses(&records_bytes, layout, records as usize, |game, _| {
            if placed.contains_key(&game) || unplaced.is_some() {
                return;
            }
            let found = addresses_of.find(game);
            let made: Vec<(i64, u64)> = found
                .iter()
                .copied()
                .filter(|(id, _)| self.resources.contains_key(id))
                .collect();
            let Some(&(first, at)) = made.first() else {
                unplaced = Some(match found.first() {
                    None => format!("address {game:#x} is in no buffer the capture describes"),
                    Some((id, _)) => format!(
                        "address {game:#x} is in #{id}, which replay did not make: the capture's events do not name \
                         it"
                    ),
                });
                return;
            };
            let mine = self.address(first, at);
            let others: BTreeSet<u64> = made.iter().map(|&(id, at)| self.address(id, at)).collect();
            if others.len() > 1 {
                self.warn(format!(
                    "ExecuteIndirect (event {seq}): address {game:#x} is in buffers sharing memory that replay \
                     placed apart; #{first}'s is used"
                ));
            }
            placed.insert(game, mine);
        });
        if let Some(why) = unplaced {
            self.warn(format!("ExecuteIndirect (event {seq}) is not replayed: {why}"));
            return Ok(Arguments::Skip);
        }
        patch(&mut records_bytes, layout, records as usize, |game| {
            placed
                .get(&game)
                .copied()
                .ok_or_else(|| format!("address {game:#x} was not placed"))
        })?;
        let (copy, p) = self.staging(D3D12_HEAP_TYPE_UPLOAD, bytes)?;
        unsafe { core::ptr::copy_nonoverlapping(records_bytes.as_ptr(), p, records_bytes.len()) };
        Ok(Arguments::Patched(copy))
    }

    /// `len` bytes of buffer `id` from `offset`, as replay's GPU holds them now.
    fn read_range(&mut self, id: i64, offset: u64, len: u64) -> Result<Vec<u8>, String> {
        self.first_use_now(id)?;
        let m = self.made(id)?;
        if !m.buffer() {
            return Err(format!("#{id} is not a buffer"));
        }
        if offset + len > m.desc.Width {
            return Err(format!(
                "#{id}: bytes {offset}..{} are past its end",
                offset + len
            ));
        }
        let r = m.resource.clone();
        if m.heap.mapped() {
            let mut p = core::ptr::null_mut();
            let range = D3D12_RANGE {
                Begin: offset as usize,
                End: (offset + len) as usize,
            };
            unsafe {
                r.Map(0, Some(&range), Some(&mut p))
                    .map_err(|e| format!("#{id} Map: {e}"))?;
                let out =
                    core::slice::from_raw_parts(p.cast::<u8>().add(offset as usize), len as usize).to_vec();
                r.Unmap(0, Some(&D3D12_RANGE::default()));
                return Ok(out);
            }
        }
        let (readback, p) = self.staging(D3D12_HEAP_TYPE_READBACK, len)?;
        unsafe { self.list.CopyBufferRegion(&readback, 0, &r, offset, len) };
        self.flush()?;
        Ok(unsafe { core::slice::from_raw_parts(p.cast_const(), len as usize) }.to_vec())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn addresses_map_to_buffers_and_offsets() {
        let mut a = Addresses::default();
        a.insert(1, 0x1000, 0x100);
        a.insert(2, 0x2000, 0x10000);
        // Two placed buffers sharing memory.
        a.insert(3, 0x2000, 0x100);
        assert_eq!(a.find(0x1010), vec![(1, 0x10)]);
        assert_eq!(a.find(0x1100), vec![]);
        let mut shared = a.find(0x2010);
        shared.sort();
        assert_eq!(shared, vec![(2, 0x10), (3, 0x10)]);
        assert_eq!(a.find(0x2f00), vec![(2, 0xf00)]);
        assert_eq!(a.find(0x10), vec![]);
    }
}
