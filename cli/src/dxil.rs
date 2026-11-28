//! Independent shader evidence. Nothing here narrows what capture records.
use serde_json::{Value, json};
use std::sync::OnceLock;
use windows::Win32::Graphics::{Direct3D::Dxc::*, Direct3D12::*};
use windows::Win32::System::LibraryLoader::{GetModuleFileNameW, GetProcAddress, LoadLibraryW};
use windows::core::{GUID, HRESULT, Interface, PCWSTR, s};

/// The dx and iridia builds linked (`dxil-reader 0.5.0`, or a git build's commit too), from Cargo.lock (build.rs).
const DX: &str = env!("LOCUM_DX");
const IRIDIA: &str = env!("LOCUM_IRIDIA");
type Create = unsafe extern "system" fn(*const GUID, *const GUID, *mut *mut core::ffi::c_void) -> HRESULT;
static COMPILER_IDENTITY: OnceLock<Value> = OnceLock::new();

fn create<T: Interface>(class: &GUID) -> Result<T, String> {
    static CREATE: OnceLock<Result<Create, String>> = OnceLock::new();
    let create = CREATE
        .get_or_init(|| {
            use std::os::windows::ffi::OsStrExt;
            let load = |path: &std::ffi::OsStr| {
                let wide: Vec<_> = path.encode_wide().chain(Some(0)).collect();
                // Keep the module loaded for the process lifetime: COM vtables live in it.
                unsafe { LoadLibraryW(PCWSTR(wide.as_ptr())) }.map_err(|e| e.to_string())
            };
            let module = match std::env::var_os("LOCUM_DXCOMPILER") {
                Some(path) => load(&path).map_err(|e| format!("load LOCUM_DXCOMPILER {}: {e}", path.display()))?,
                None => load("dxcompiler.dll".as_ref())
                    .or_else(|e| match sdk_dxcompiler() {
                        Some(path) => load(path.as_os_str()).map_err(|e| format!("load {}: {e}", path.display())),
                        None => Err(e),
                    })
                    .map_err(|e| {
                        format!(
                            "no dxcompiler.dll: not on the DLL search path, and no Windows SDK has one ({e}); \
                             set LOCUM_DXCOMPILER to its path"
                        )
                    })?,
            };
            let address = unsafe { GetProcAddress(module, s!("DxcCreateInstance")) }
                .ok_or("dxcompiler.dll has no DxcCreateInstance")?;
            let mut name = vec![0u16; 32768];
            let len = unsafe { GetModuleFileNameW(Some(module), &mut name) } as usize;
            let name = String::from_utf16_lossy(&name[..len]);
            let hash = std::fs::read(&name)
                .ok()
                .map(|bytes| locum_capture::hash::key(&bytes));
            let _ = COMPILER_IDENTITY.set(json!({"path":name,"file_hash":hash}));
            Ok(unsafe { std::mem::transmute::<unsafe extern "system" fn() -> isize, Create>(address) })
        })
        .as_ref()
        .map_err(Clone::clone)?;
    let mut result = std::ptr::null_mut();
    unsafe { create(class, &T::IID, &mut result).ok() }.map_err(|e| e.to_string())?;
    if result.is_null() {
        return Err("DxcCreateInstance returned null".into());
    }
    Ok(unsafe { T::from_raw(result) })
}

/// The newest Windows SDK's x64 `dxcompiler.dll` (`Windows Kits\10\bin\<version>\x64`), which the DLL search does
/// not find by itself.
fn sdk_dxcompiler() -> Option<std::path::PathBuf> {
    let root = std::path::PathBuf::from(std::env::var_os("ProgramFiles(x86)")?).join(r"Windows Kits\10\bin");
    std::fs::read_dir(&root)
        .ok()?
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let version: Vec<u32> = entry
                .file_name()
                .to_str()?
                .split('.')
                .map(str::parse)
                .collect::<Result<_, _>>()
                .ok()?;
            let dll = entry.path().join(r"x64\dxcompiler.dll");
            dll.is_file().then_some((version, dll))
        })
        .max()
        .map(|(_, dll)| dll)
}

fn buffer(code: &[u8]) -> DxcBuffer {
    DxcBuffer {
        Ptr: code.as_ptr().cast(),
        Size: code.len(),
        Encoding: 0,
    }
}

fn load_dxil_create() -> Result<Create, String> {
    static CREATE: OnceLock<Result<Create, String>> = OnceLock::new();
    let create = CREATE
        .get_or_init(|| {
            use std::os::windows::ffi::OsStrExt;
            let load = |path: &std::ffi::OsStr| {
                let wide: Vec<_> = path.encode_wide().chain(Some(0)).collect();
                unsafe { LoadLibraryW(PCWSTR(wide.as_ptr())) }.map_err(|e| e.to_string())
            };
            let sibling = std::env::var_os("LOCUM_DXCOMPILER")
                .map(std::path::PathBuf::from)
                .or_else(sdk_dxcompiler)
                .map(|p| p.with_file_name("dxil.dll"))
                .filter(|p| p.is_file());
            let module = match sibling {
                Some(path) => load(path.as_os_str()).map_err(|e| format!("load {}: {e}", path.display()))?,
                None => load("dxil.dll".as_ref()).map_err(|e| format!("no dxil.dll: {e}"))?,
            };
            let address = unsafe { GetProcAddress(module, s!("DxcCreateInstance")) }
                .ok_or("dxil.dll has no DxcCreateInstance")?;
            Ok(unsafe { std::mem::transmute::<unsafe extern "system" fn() -> isize, Create>(address) })
        })
        .as_ref()
        .map_err(Clone::clone)?;
    Ok(*create)
}

fn create_validator() -> Result<IDxcValidator, String> {
    let create = load_dxil_create()?;
    let mut result = std::ptr::null_mut();
    unsafe { create(&CLSID_DxcValidator, &IDxcValidator::IID, &mut result).ok() }
        .map_err(|e| format!("DxcValidator: {e}"))?;
    if result.is_null() {
        return Err("DxcValidator returned null".into());
    }
    Ok(unsafe { IDxcValidator::from_raw(result) })
}

/// Queries the loaded `IDxcValidator`'s `(major, minor)` version and clamps `!dx.valver` in `ir` when the
/// original shader was validated by a newer `dxil.dll` (e.g. 1.9 on a machine with SDK 1.7).
fn clamp_valver(ir: &str, validator: &IDxcValidator) -> String {
    let (mut maj, mut min) = (0u32, 0u32);
    if let Ok(info) = validator.cast::<IDxcVersionInfo>()
        && unsafe { info.GetVersion(&mut maj, &mut min) }.is_ok()
        && maj > 0
    {
        // Find `!dx.valver = !{!<id>}`
        let node = ir.lines().find_map(|line| {
            let rest = line.trim().strip_prefix("!dx.valver = !{")?;
            let id = rest.strip_suffix('}')?.trim();
            id.starts_with('!').then(|| id.to_string())
        });
        if let Some(node) = node {
            let prefix = format!("{node} = !{{i32 ");
            let mut changed = false;
            let mut out = String::with_capacity(ir.len());
            for line in ir.lines() {
                if let Some(rest) = line.strip_prefix(&prefix)
                    && let Some(inner) = rest.strip_suffix('}')
                    && let Some((a, b)) = inner.split_once(", i32 ")
                    && let (Ok(smaj), Ok(smin)) = (a.trim().parse::<u32>(), b.trim().parse::<u32>())
                    && (smaj, smin) > (maj, min)
                {
                    out.push_str(&format!("{node} = !{{i32 {maj}, i32 {min}}}\n"));
                    changed = true;
                    continue;
                }
                out.push_str(line);
                out.push('\n');
            }
            if changed {
                return out;
            }
        }
    }
    ir.to_string()
}

/// Assembles textual DXIL LLVM IR back into a signed DXIL container using `IDxcAssembler` + `IDxcValidator`.
pub fn assemble_and_validate(ir: &str) -> Result<Vec<u8>, String> {
    let utils: IDxcUtils = create(&CLSID_DxcUtils)?;
    let assembler: IDxcAssembler = create(&CLSID_DxcAssembler)?;
    let validator = create_validator()?;
    let ir = clamp_valver(ir, &validator);
    let src_blob: IDxcBlobEncoding =
        unsafe { utils.CreateBlobFromPinned(ir.as_ptr().cast(), ir.len() as u32, DXC_CP_UTF8) }
            .map_err(|e| format!("CreateBlobFromPinned: {e}"))?;
    let asm_res: IDxcOperationResult = unsafe { assembler.AssembleToContainer(&src_blob) }
        .map_err(|e| format!("AssembleToContainer: {e}"))?;
    let status = unsafe { asm_res.GetStatus() }.map_err(|e| format!("Assemble status: {e}"))?;
    if status.is_err() {
        let msg = unsafe { asm_res.GetErrorBuffer() }
            .ok()
            .map(|b| {
                let bytes = unsafe {
                    std::slice::from_raw_parts(b.GetBufferPointer().cast::<u8>(), b.GetBufferSize())
                };
                String::from_utf8_lossy(bytes).trim_end_matches('\0').to_string()
            })
            .unwrap_or_default();
        return Err(format!("DXIL assemble failed ({status:?}): {msg}"));
    }
    let container = unsafe { asm_res.GetResult() }.map_err(|e| format!("Assemble GetResult: {e}"))?;
    let val_res: IDxcOperationResult =
        unsafe { validator.Validate(&container, DxcValidatorFlags_InPlaceEdit) }
            .map_err(|e| format!("Validate: {e}"))?;
    let vstatus = unsafe { val_res.GetStatus() }.map_err(|e| format!("Validate status: {e}"))?;
    if vstatus.is_err() {
        let msg = unsafe { val_res.GetErrorBuffer() }
            .ok()
            .map(|b| {
                let bytes = unsafe {
                    std::slice::from_raw_parts(b.GetBufferPointer().cast::<u8>(), b.GetBufferSize())
                };
                String::from_utf8_lossy(bytes).trim_end_matches('\0').to_string()
            })
            .unwrap_or_default();
        return Err(format!("DXIL validate failed ({vstatus:?}): {msg}"));
    }
    let signed = unsafe { val_res.GetResult() }.ok().unwrap_or(container);
    let bytes =
        unsafe { std::slice::from_raw_parts(signed.GetBufferPointer().cast::<u8>(), signed.GetBufferSize()) };
    Ok(bytes.to_vec())
}

/// Instruments a SM 6.6+ DXIL shader so every dynamic `dx.op.createHandleFromHeap(..., i1 false, ...)`
/// (CBV/SRV/UAV `ResourceDescriptorHeap` access) atomically sets a bit in a 2-region bitmask `RWByteAddressBuffer`
/// bound at `ResourceDescriptorHeap[log_slot]`:
/// - bytes `0..half_bytes`: slots read (SRV/CBV/unknown)
/// - bytes `half_bytes..2*half_bytes`: slots written/accessed as UAV (`ResourceProperties` bit `0x1000`)
///
/// Here `half_bytes = ((max_slots / 32) + 2) * 4` (extra sentinel word at index `max_slots` avoids branches).
///
/// Returns `Ok(None)` if the shader has no `ResourceDescriptorHeap` `createHandleFromHeap` calls.
pub fn instrument_bindless(code: &[u8], log_slot: u32, max_slots: u32) -> Result<Option<Vec<u8>>, String> {
    if !is_dxil(code) {
        return Ok(None);
    }
    let ir = disassemble(code)?;
    if !ir.contains("@dx.op.createHandleFromHeap(") {
        return Ok(None);
    }
    // Pre-scan `annotateHandle` lines to classify which `createHandleFromHeap` handles are UAVs (ResourceProperties
    // kind & 0x1000 != 0, or kind == 10..=11 / TypedUAV / RawBuffer UAV / StructuredBuffer UAV: bit 12 = 4096 is
    // `IsUAV` in DXIL `ResourceProperties.Basic.ResourceKind` flags).
    let mut uav_handles = std::collections::HashSet::<String>::new();
    for line in ir.lines() {
        if let Some(rest) = line.split_once("@dx.op.annotateHandle(i32 216, %dx.types.Handle ") {
            let after_handle = rest.1;
            if let Some((handle_name, tail)) = after_handle.split_once(',') {
                let handle_name = handle_name.trim();
                if let Some((_, props_tail)) = tail.split_once("ResourceProperties { i32 ")
                    && let Some((num_str, _)) = props_tail.split_once(',')
                    && let Ok(props) = num_str.trim().parse::<u32>()
                    && (props & 0x1000) != 0
                {
                    uav_handles.insert(handle_name.to_string());
                }
            }
        }
    }

    let half_bytes: u32 = ((max_slots / 32) + 2) * 4;
    let has_atomic_decl = ir.contains("declare i32 @dx.op.atomicBinOp.i32(");
    let has_annotate_decl = ir.contains("declare %dx.types.Handle @dx.op.annotateHandle(");
    let mut out = String::with_capacity(ir.len() + 8192);
    let mut injected = 0usize;
    let mut decl_inserted = false;

    for line in ir.lines() {
        // Match: `<indent>%<res> = call %dx.types.Handle @dx.op.createHandleFromHeap(i32 218, i32 <idx>, i1 false, ...)`
        if let Some((lhs, rhs)) = line.split_once(" = call %dx.types.Handle @dx.op.createHandleFromHeap(")
            && let Some((args_str, _)) = rhs.split_once(')')
        {
            let parts: Vec<&str> = args_str.split(',').map(str::trim).collect();
            if parts.len() == 4 && parts[2] == "i1 false" {
                let res_name = lhs.trim();
                let idx_expr = parts[1].strip_prefix("i32 ").unwrap_or(parts[1]).trim();
                let is_uav = uav_handles.contains(res_name);
                let region_off = if is_uav { half_bytes } else { 0 };
                let k = injected;
                injected += 1;
                // All inserted SSA values are named (`%__locum_*`) so existing unnamed `%N` numbering is untouched.
                use std::fmt::Write;
                let _ = writeln!(
                    out,
                    "  %__locum_ok_{k} = icmp ult i32 {idx_expr}, {max_slots}\n\
                     \x20 %__locum_idx_{k} = select i1 %__locum_ok_{k}, i32 {idx_expr}, i32 {max_slots}\n\
                     \x20 %__locum_w_{k} = lshr i32 %__locum_idx_{k}, 5\n\
                     \x20 %__locum_b_{k} = shl i32 %__locum_w_{k}, 2\n\
                     \x20 %__locum_off_{k} = add i32 %__locum_b_{k}, {region_off}\n\
                     \x20 %__locum_sh_{k} = and i32 %__locum_idx_{k}, 31\n\
                     \x20 %__locum_bit_{k} = shl i32 1, %__locum_sh_{k}\n\
                     \x20 %__locum_h_{k} = call %dx.types.Handle @dx.op.createHandleFromHeap(i32 218, i32 {log_slot}, i1 false, i1 false)\n\
                     \x20 %__locum_a_{k} = call %dx.types.Handle @dx.op.annotateHandle(i32 216, %dx.types.Handle %__locum_h_{k}, %dx.types.ResourceProperties {{ i32 4107, i32 0 }})\n\
                     \x20 %__locum_old_{k} = call i32 @dx.op.atomicBinOp.i32(i32 78, %dx.types.Handle %__locum_a_{k}, i32 2, i32 %__locum_off_{k}, i32 undef, i32 undef, i32 %__locum_bit_{k})"
                );
            }
        }
        if !decl_inserted && line.starts_with("declare ") && line.contains("@dx.op.createHandleFromHeap(") {
            out.push_str(line);
            out.push('\n');
            if !has_atomic_decl {
                out.push_str(
                    "declare i32 @dx.op.atomicBinOp.i32(i32, %dx.types.Handle, i32, i32, i32, i32, i32) nounwind\n",
                );
            }
            if !has_annotate_decl {
                out.push_str(
                    "declare %dx.types.Handle @dx.op.annotateHandle(i32, %dx.types.Handle, %dx.types.ResourceProperties) nounwind readnone\n",
                );
            }
            decl_inserted = true;
            continue;
        }
        out.push_str(line);
        out.push('\n');
    }

    if injected == 0 {
        return Ok(None);
    }
    let patched = assemble_and_validate(&out)?;
    Ok(Some(patched))
}

/// Byte size required for the 2-region bindless trace bitmask buffer for a heap of `max_slots` descriptors.
pub fn bindless_log_bytes(max_slots: u32) -> u64 {
    let half_bytes: u64 = ((u64::from(max_slots) / 32) + 2) * 4;
    half_bytes * 2
}

/// Decodes a bindless trace bitmask buffer read back from the GPU into sorted `(slot, is_write)` pairs.
pub fn decode_bindless_log(bytes: &[u8], max_slots: u32) -> Vec<(u32, bool)> {
    let half_bytes = (((max_slots / 32) + 2) * 4) as usize;
    let mut out = Vec::new();
    for (is_write, region) in [
        (false, bytes.get(..half_bytes).unwrap_or(&[])),
        (true, bytes.get(half_bytes..half_bytes * 2).unwrap_or(&[])),
    ] {
        for (word_idx, &chunk) in region.as_chunks::<4>().0.iter().enumerate() {
            let mut w = u32::from_le_bytes(chunk);
            while w != 0 {
                let bit = w.trailing_zeros();
                let slot = (word_idx as u32) * 32 + bit;
                if slot < max_slots {
                    out.push((slot, is_write));
                }
                w &= w - 1;
            }
        }
    }
    out.sort_unstable();
    out.dedup();
    out
}

pub fn is_dxil(code: &[u8]) -> bool {
    dxcontainer::chunks(code).is_ok_and(|chunks| chunks.iter().any(|c| &c.kind == b"DXIL"))
}

pub fn disassemble(code: &[u8]) -> Result<String, String> {
    let compiler: IDxcCompiler3 = create(&CLSID_DxcCompiler)?;
    let result: IDxcResult = unsafe { compiler.Disassemble(&buffer(code)) }.map_err(|e| e.to_string())?;
    unsafe { result.GetStatus() }
        .and_then(|h| h.ok())
        .map_err(|e| e.to_string())?;
    let blob = unsafe { result.GetResult() }.map_err(|e| e.to_string())?;
    let bytes =
        unsafe { std::slice::from_raw_parts(blob.GetBufferPointer().cast::<u8>(), blob.GetBufferSize()) };
    Ok(String::from_utf8_lossy(bytes).trim_end_matches('\0').into())
}

/// Compiles HLSL `source`'s `main` for `profile` (`ps_6_0`) to DXIL, signed when the compiler finds `dxil.dll`.
pub fn compile(source: &str, profile: &str) -> Result<Vec<u8>, String> {
    let compiler: IDxcCompiler3 = create(&CLSID_DxcCompiler)?;
    let wide = |s: &str| s.encode_utf16().chain(Some(0)).collect::<Vec<u16>>();
    let words = [wide("-E"), wide("main"), wide("-T"), wide(profile)];
    let args: Vec<PCWSTR> = words.iter().map(|w| PCWSTR(w.as_ptr())).collect();
    let result: IDxcResult = unsafe { compiler.Compile(&buffer(source.as_bytes()), Some(&args), None) }
        .map_err(|e| e.to_string())?;
    let blob_bytes = |b: &IDxcBlob| unsafe {
        std::slice::from_raw_parts(b.GetBufferPointer().cast::<u8>(), b.GetBufferSize()).to_vec()
    };
    let status = unsafe { result.GetStatus() }.map_err(|e| e.to_string())?;
    if status.is_err() {
        let errors = unsafe { result.GetErrorBuffer() }
            .map(|b| {
                String::from_utf8_lossy(&blob_bytes(&b.cast().expect("an error buffer is a blob")))
                    .into_owned()
            })
            .unwrap_or_default();
        return Err(format!("{}: {}", status.message(), errors.trim_end_matches('\0')));
    }
    let code = unsafe { result.GetResult() }.map_err(|e| e.to_string())?;
    Ok(blob_bytes(&code))
}

fn native_ranges(code: &[u8]) -> Result<Vec<Value>, String> {
    let utils: IDxcUtils = create(&CLSID_DxcUtils)?;
    let mut pointer = std::ptr::null_mut();
    let shader = unsafe { utils.CreateReflection(&buffer(code), &ID3D12ShaderReflection::IID, &mut pointer) };
    if let Err(shader_error) = shader {
        pointer = std::ptr::null_mut();
        unsafe { utils.CreateReflection(&buffer(code), &ID3D12LibraryReflection::IID, &mut pointer) }
            .map_err(|e| format!("shader reflection: {shader_error}; library reflection: {e}"))?;
        if pointer.is_null() {
            return Err("library reflection returned null".into());
        }
        let library = unsafe { ID3D12LibraryReflection::from_raw(pointer) };
        let desc = unsafe { library.GetDesc() }.map_err(|e| e.to_string())?;
        let mut ranges = Vec::new();
        for index in 0..desc.FunctionCount {
            let function =
                unsafe { library.GetFunctionByIndex(i32::try_from(index).map_err(|e| e.to_string())?) }
                    .ok_or("library function absent")?;
            let mut desc = D3D12_FUNCTION_DESC::default();
            unsafe { function.GetDesc(&mut desc) }.map_err(|e| e.to_string())?;
            for index in 0..desc.BoundResources {
                let mut r = D3D12_SHADER_INPUT_BIND_DESC::default();
                unsafe { function.GetResourceBindingDesc(index, &mut r) }.map_err(|e| e.to_string())?;
                ranges.push(native_range(r)?);
            }
        }
        ranges.sort_by_key(Value::to_string);
        ranges.dedup();
        return Ok(ranges);
    }
    if pointer.is_null() {
        return Err("CreateReflection returned null".into());
    }
    let reflection = unsafe { ID3D12ShaderReflection::from_raw(pointer) };
    let mut desc = D3D12_SHADER_DESC::default();
    unsafe { reflection.GetDesc(&mut desc) }.map_err(|e| e.to_string())?;
    let mut ranges = Vec::new();
    for index in 0..desc.BoundResources {
        let mut r = D3D12_SHADER_INPUT_BIND_DESC::default();
        unsafe { reflection.GetResourceBindingDesc(index, &mut r) }.map_err(|e| e.to_string())?;
        ranges.push(native_range(r)?);
    }
    ranges.sort_by_key(Value::to_string);
    Ok(ranges)
}
fn native_range(r: D3D12_SHADER_INPUT_BIND_DESC) -> Result<Value, String> {
    let class = match r.Type.0 {
        0 => "cb",
        1 | 2 | 5 | 7 | 12 => "srv",
        3 => "sampler",
        4 | 6 | 8 | 9 | 10 | 11 | 13 => "uav",
        n => {
            return Err(format!(
                "Microsoft resource type {n} has no register class mapping"
            ));
        }
    };
    let name = if r.Name.is_null() {
        String::new()
    } else {
        unsafe { r.Name.to_string() }.map_err(|e| e.to_string())?
    };
    Ok(
        json!({"class":class,"register":r.BindPoint,"space":r.Space,"count":if r.BindCount==0 {u32::MAX} else {r.BindCount},"name":name}),
    )
}

fn reader_ranges(code: &[u8], reflection: bool) -> Result<Vec<Value>, String> {
    let parts = dxil_reader::parts(code).map_err(|e| e.to_string())?;
    let kind = if reflection && parts.iter().any(|p| &p.kind == b"STAT") {
        b"STAT"
    } else {
        b"DXIL"
    };
    let module = dxil_reader::module(&parts, kind)
        .map_err(|e| e.to_string())?
        .ok_or("DXIL module absent")?;
    let mut ranges = dxil_reader::reflection::ranges(&module)
        .map_err(|e| e.to_string())?
        .into_iter()
        .map(|(r, name, _)| {
            json!({"class":match r.class {0=>"srv",1=>"uav",2=>"cb",3=>"sampler",_=>"unknown"},
            "register":r.lower,"space":r.space,"count":r.count,"name":name})
        })
        .collect::<Vec<_>>();
    ranges.sort_by_key(Value::to_string);
    Ok(ranges)
}

fn answer<T: serde::Serialize>(result: Result<T, String>) -> Value {
    match result {
        Ok(value) => json!({"ok":value}),
        Err(e) => json!({"error":e}),
    }
}

/// Keep both answers even when one fails. Disagreements are evidence, never a fallback.
pub fn evidence(code: &[u8], source: &str) -> Value {
    let reader = reader_ranges(code, true);
    let program_ranges = reader_ranges(code, false);
    let native = native_ranges(code);
    let version = (|| -> Result<Value, String> {
        let compiler: IDxcCompiler3 = create(&CLSID_DxcCompiler)?;
        let info: IDxcVersionInfo = compiler.cast().map_err(|e| e.to_string())?;
        let (mut major, mut minor) = (0, 0);
        unsafe { info.GetVersion(&mut major, &mut minor) }.map_err(|e| e.to_string())?;
        Ok(json!({"major":major,"minor":minor,"module":COMPILER_IDENTITY.get()}))
    })();
    let interface = iridia_dxil::interface(code).map_err(|e| e.to_string());
    let iridia_ranges = interface.as_ref().map(|i| {
        let mut ranges = i.resources.iter().filter_map(|r|r.provenance.d3d.map(|b| {
            use iridia_ir::interface::D3dClass::*;
            json!({"class":match b.class {ConstantBuffer=>"cb", ShaderResource=>"srv", Sampler=>"sampler", UnorderedAccess=>"uav"},
                "register":b.register,"space":b.space,"count":r.count,"name":r.provenance.name})
        })).collect::<Vec<_>>();
        ranges.sort_by_key(Value::to_string);
        ranges
    }).map_err(Clone::clone);
    let checked = reader.is_ok() && native.is_ok() && iridia_ranges.is_ok();
    let agrees = checked && reader == native && iridia_ranges == native;
    let rdat = dxcontainer::chunks(code)
        .ok()
        .and_then(|parts| parts.into_iter().find(|p| &p.kind == b"RDAT"))
        .map(|part| answer(iridia_dxil::subobjects(part.data).map_err(|e| e.to_string())));
    json!({"source":source,"shader_hash":locum_capture::hash::key(code),"dx":DX,"iridia":IRIDIA,
        "scope":"display; resource capture is determined by API state, not these analyses", "microsoft_compiler":answer(version),
        "comparison":if agrees {"agree"} else if checked {"disagreement"} else {"incomplete"},
        "dxil_reader":answer(reader),"dxil_program_ranges":answer(program_ranges),"microsoft":answer(native),"iridia":answer(iridia_ranges),
        "interface":answer(interface),"rdat_subobjects":rdat})
}

/// A DXIL or DXBC program on iridia's IR, in the form its writers and analyses take (`iridia_passes::prepare`).
pub fn lift(code: &[u8]) -> Result<iridia_ir::Program, String> {
    let mut program = if is_dxil(code) {
        iridia_dxil::ir::lift(code)
    } else {
        iridia_dxbc::ir::lift(code)
    }
    .map_err(|e| e.to_string())?;
    iridia_passes::prepare(&mut program)?;
    Ok(program)
}

/// Readable WGSL of a DXIL or DXBC program from iridia's IR, in naga's dialect: resources keep their D3D registers.
pub fn wgsl(code: &[u8]) -> Result<String, String> {
    iridia_naga::wgsl(&lift(code)?, &iridia_naga::Profile::NAGA).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn instrument_bindless_round_trips_sm66_shader() {
        let src = r#"
            cbuffer Params : register(b0) { uint srvIdx; uint uavIdx; };
            [numthreads(1,1,1)]
            void main() {
                ByteAddressBuffer s = ResourceDescriptorHeap[srvIdx];
                RWByteAddressBuffer u = ResourceDescriptorHeap[uavIdx];
                u.Store(0, s.Load(0) + 1);
            }
        "#;
        let compiled = compile(src, "cs_6_6").expect("compile cs_6_6");
        let patched = instrument_bindless(&compiled, 1024, 1024)
            .expect("instrument_bindless")
            .expect("shader uses ResourceDescriptorHeap");
        assert!(is_dxil(&patched));
        let dis = disassemble(&patched).expect("disassemble patched");
        assert!(dis.contains("@dx.op.atomicBinOp.i32"), "{dis}");
        // Verify bitmask decoder round-trip.
        let mut buf = vec![0u8; bindless_log_bytes(1024) as usize];
        let half = (((1024u32 / 32) + 2) * 4) as usize;
        buf[0] = 1 << 5; // read slot 5
        buf[half + 4] = 1 << 3; // write slot 35
        assert_eq!(decode_bindless_log(&buf, 1024), vec![(5, false), (35, true)]);

        // Also test against the real P1LEVEL 67-sampleLevel CS 4fa9c207 if present on this machine.
        let p1 = std::path::Path::new(
            r"C:\Users\kalmarv\modding\.targets\xcl-sky-captures\P1LEVEL\captures\depth-plane\capture.db",
        );
        if p1.is_file() {
            let cap = crate::open::open(&p1.to_string_lossy(), None).expect("open P1LEVEL");
            let key: String = cap
                .row(
                    "SELECT blob FROM shaders WHERE hash LIKE '4fa9c207%' LIMIT 1",
                    [],
                    |r| r.get(0),
                )
                .expect("4fa9c207 in P1LEVEL");
            let code = cap.blob(&key).expect("4fa9c207 blob");
            let patched = instrument_bindless(&code, 1_000_000, 1_000_000)
                .expect("instrument P1LEVEL 4fa9c207")
                .expect("4fa9c207 uses ResourceDescriptorHeap");
            assert!(is_dxil(&patched));
            for prefix in ["5b12b481", "28ac858f"] {
                let k: String = cap
                    .row(
                        &format!("SELECT blob FROM shaders WHERE hash LIKE '{prefix}%' LIMIT 1"),
                        [],
                        |r| r.get(0),
                    )
                    .expect(prefix);
                let orig = cap.blob(&k).expect(prefix);
                let patched = instrument_bindless(&orig, 999_999, 999_999)
                    .expect(prefix)
                    .expect(prefix);
                assert!(is_dxil(&patched));
            }
        }
    }
}
