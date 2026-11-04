//! Independent shader evidence. Nothing here narrows what capture records.
use serde_json::{Value, json};
use std::sync::OnceLock;
use windows::Win32::Graphics::{Direct3D::Dxc::*, Direct3D12::*};
use windows::Win32::System::LibraryLoader::{GetModuleFileNameW, GetProcAddress, LoadLibraryW};
use windows::core::{GUID, HRESULT, Interface, PCWSTR, s};

/// The dx and iridia commits linked, from Cargo.lock (build.rs).
const DX_REV: &str = env!("LOCUM_DX_REV");
const IRIDIA_REV: &str = env!("LOCUM_IRIDIA_REV");
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
            .map(|b| String::from_utf8_lossy(&blob_bytes(&b.cast().expect("an error buffer is a blob"))).into_owned())
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
            use iridia::D3dClass::*;
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
    json!({"source":source,"shader_hash":locum_capture::hash::key(code),"dx_rev":DX_REV,"iridia_rev":IRIDIA_REV,
        "scope":"display; resource capture is determined by API state, not these analyses", "microsoft_compiler":answer(version),
        "comparison":if agrees {"agree"} else if checked {"disagreement"} else {"incomplete"},
        "dxil_reader":answer(reader),"dxil_program_ranges":answer(program_ranges),"microsoft":answer(native),"iridia":answer(iridia_ranges),
        "interface":answer(interface),"rdat_subobjects":rdat})
}

/// A DXIL or DXBC program lifted through iridia.
pub fn lift(code: &[u8]) -> Result<iridia::Program, String> {
    if is_dxil(code) {
        iridia_dxil::lift(code)
    } else {
        iridia_dxbc::lift(code)
    }
    .map_err(|e| e.to_string())
}

/// Readable WGSL of a DXIL or DXBC program, lifted through iridia.
pub fn wgsl(code: &[u8]) -> Result<String, String> {
    iridia::emit::wgsl(&lift(code)?).map_err(|e| e.to_string())
}
