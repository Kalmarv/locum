//! Opt-in listed first-chance AVs and fatal AVs anywhere. No allocation, logging, or callback mutexes.
//! A prestarted CLI waits for the request or process exit, then uses the normal PSS dump path.
use std::os::windows::io::AsRawHandle;
use std::os::windows::process::CommandExt;
use std::process::{Command, Stdio};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use windows::Win32::Foundation::{CloseHandle, EXCEPTION_ACCESS_VIOLATION, HANDLE, WAIT_OBJECT_0};
use windows::Win32::System::Diagnostics::Debug::{
    AddVectoredExceptionHandler, EXCEPTION_CONTINUE_SEARCH, EXCEPTION_POINTERS, SetUnhandledExceptionFilter,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::ProcessStatus::{GetModuleInformation, MODULEINFO};
use windows::Win32::System::Threading::{
    CREATE_NO_WINDOW, CreateEventW, GetCurrentProcess, GetCurrentThreadId, SetEvent, WaitForMultipleObjects,
    WaitForSingleObject,
};
use windows::core::HSTRING;

// Wire layout for the CLI: two native-width words, thread ID then remote EXCEPTION_POINTERS.
// Written once before signalling the event. Remains valid until the snapshot is acknowledged.
static EXCEPTION: [AtomicUsize; 2] = [AtomicUsize::new(0), AtomicUsize::new(0)];
static FIRED: AtomicBool = AtomicBool::new(false);
static PREVIOUS: AtomicUsize = AtomicUsize::new(0);
struct Armed {
    addresses: Vec<usize>,
    request: usize,
    ready: usize,
}
static ARMED: OnceLock<Armed> = OnceLock::new();

pub fn install() {
    let settings = crate::settings::settings();
    if settings.crash_av_rvas.is_empty() {
        return;
    }
    if let Err(error) = prepare(&settings.crash_av_rvas) {
        crate::log::log!("crash dump not armed: {error}");
    }
}

fn prepare(rvas: &[usize]) -> Result<(), String> {
    if ARMED.get().is_some() {
        return Err("crash handler already armed".into());
    }
    let settings = crate::settings::settings();
    let dumper = settings
        .dumper
        .as_ref()
        .ok_or("crash-av-rva requires settings.txt dumper")?;
    let dir = settings
        .dumps
        .clone()
        .or_else(crate::log::folder)
        .ok_or("no dump folder")?;
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let pid = std::process::id();
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|e| e.to_string())?
        .as_millis();
    let name = format!("crash-{pid}-{stamp}");
    let path = dir.join(format!("{name}.dmp"));
    let request_name = format!("Local\\locum-{name}-request");
    let ready_name = format!("Local\\locum-{name}-ready");
    let listening_name = format!("Local\\locum-{name}-listening");
    let module = unsafe { GetModuleHandleW(None) }.map_err(|e| e.to_string())?;
    let mut info = MODULEINFO::default();
    unsafe {
        GetModuleInformation(
            GetCurrentProcess(),
            module,
            &mut info,
            size_of::<MODULEINFO>() as u32,
        )
    }
    .map_err(|e| format!("executable bounds: {e}"))?;
    let base = info.lpBaseOfDll as usize;
    let addresses = rvas
        .iter()
        .map(|&rva| {
            if rva >= info.SizeOfImage as usize {
                return Err(format!("crash RVA {rva:#x} outside executable"));
            }
            base.checked_add(rva).ok_or_else(|| "crash RVA overflow".into())
        })
        .collect::<Result<Vec<_>, String>>()?;
    let request = unsafe { CreateEventW(None, true, false, &HSTRING::from(&request_name)) }
        .map_err(|e| e.to_string())?;
    let ready = match unsafe { CreateEventW(None, true, false, &HSTRING::from(&ready_name)) } {
        Ok(event) => event,
        Err(e) => {
            let _ = unsafe { CloseHandle(request) };
            return Err(e.to_string());
        }
    };
    let listening = match unsafe { CreateEventW(None, true, false, &HSTRING::from(&listening_name)) } {
        Ok(event) => event,
        Err(e) => {
            let _ = unsafe { CloseHandle(request) };
            let _ = unsafe { CloseHandle(ready) };
            return Err(e.to_string());
        }
    };
    // Keep diagnostics outside the fault path. All files and the helper are prepared now.
    let started = (|| {
        let log = std::fs::File::create(dir.join(format!("{name}-helper.log")))?;
        Command::new(dumper)
            .creation_flags(CREATE_NO_WINDOW.0)
            .arg("dump")
            .arg("--pid")
            .arg(pid.to_string())
            .arg("--out")
            .arg(&path)
            .arg("--wait-event")
            .arg(&request_name)
            .arg("--signal")
            .arg(&ready_name)
            .arg("--ready-event")
            .arg(&listening_name)
            .arg("--exception-info")
            .arg((EXCEPTION.as_ptr() as usize).to_string())
            .stdin(Stdio::null())
            .stdout(log.try_clone()?)
            .stderr(log)
            .spawn()
    })();
    let mut child = match started {
        Ok(child) => child,
        Err(e) => {
            let _ = unsafe { CloseHandle(request) };
            let _ = unsafe { CloseHandle(ready) };
            let _ = unsafe { CloseHandle(listening) };
            return Err(e.to_string());
        }
    };
    let waited =
        unsafe { WaitForMultipleObjects(&[listening, HANDLE(child.as_raw_handle())], false, 30_000) };
    let _ = unsafe { CloseHandle(listening) };
    if waited != WAIT_OBJECT_0 {
        let _ = child.kill();
        let _ = child.wait();
        let _ = unsafe { CloseHandle(request) };
        let _ = unsafe { CloseHandle(ready) };
        return Err(format!(
            "crash helper did not become ready ({waited:?}); see helper log"
        ));
    }
    // Events live until process exit: the installed callback must always be able to use them.
    ARMED
        .set(Armed {
            addresses,
            request: request.0 as usize,
            ready: ready.0 as usize,
        })
        .map_err(|_| "crash handler already armed")?;
    if unsafe { AddVectoredExceptionHandler(1, Some(vectored)) }.is_null() {
        let _ = child.kill();
        let _ = child.wait();
        let _ = unsafe { CloseHandle(request) };
        let _ = unsafe { CloseHandle(ready) };
        return Err("AddVectoredExceptionHandler failed; no crash callbacks installed".into());
    }
    let previous = unsafe { SetUnhandledExceptionFilter(Some(filter)) };
    PREVIOUS.store(previous.map_or(0, |f| f as usize), Ordering::Release);
    let listed: Vec<String> = rvas.iter().map(|rva| format!("{rva:#x}")).collect();
    crate::log::log!(
        "crash dump armed: first-chance AV RVAs [{}], fatal AV fallback at any address, {}, helper pid {}",
        listed.join(", "),
        path.display(),
        child.id()
    );
    Ok(())
}

#[cfg(all(test, target_arch = "x86_64"))]
mod tests {
    use super::*;
    use windows::Win32::System::Diagnostics::Debug::{
        AddVectoredExceptionHandler, EXCEPTION_CONTINUE_EXECUTION,
    };
    use windows::Win32::System::Threading::TerminateProcess;

    #[unsafe(naked)]
    unsafe extern "C" fn fault() {
        // A real write AV at offset 3; the handled control skips the three-byte MOV.
        core::arch::naked_asm!("xor rax, rax", "mov byte ptr [rax], 1", "ret");
    }
    unsafe extern "system" fn prior(_: *const EXCEPTION_POINTERS) -> i32 {
        // Make chaining observable without invoking WER/UI in this synthetic test process.
        let _ = unsafe { TerminateProcess(GetCurrentProcess(), 91) };
        EXCEPTION_CONTINUE_SEARCH
    }
    unsafe extern "system" fn replacement(_: *const EXCEPTION_POINTERS) -> i32 {
        let _ = unsafe { TerminateProcess(GetCurrentProcess(), 92) };
        EXCEPTION_CONTINUE_SEARCH
    }
    unsafe extern "system" fn handled(info: *mut EXCEPTION_POINTERS) -> i32 {
        let info = unsafe { &*info };
        if unsafe { (*info.ExceptionRecord).ExceptionAddress } as usize == fault as *const () as usize + 3 {
            unsafe { (*info.ContextRecord).Rip += 3 };
            EXCEPTION_CONTINUE_EXECUTION
        } else {
            EXCEPTION_CONTINUE_SEARCH
        }
    }

    /// Run in a child process only, with LOCUM_DIR/settings.txt naming the newly built CLI.
    #[test]
    #[ignore = "CPU subprocess fixture controlled by LOCUM_CRASH_TEST_MODE; may terminate process"]
    fn probe() {
        let mode = std::env::var("LOCUM_CRASH_TEST_MODE").expect("subprocess fixture only");
        let base = unsafe { GetModuleHandleW(None) }.unwrap().0 as usize;
        unsafe { SetUnhandledExceptionFilter(Some(prior)) };
        // A separately allocated marker proves the dump includes ordinary heap contents.
        let marker = Box::new([0x7bd1_349a_2fc8_e605u64; 512]);
        std::hint::black_box(&marker);
        if mode == "off" {
            install();
            assert!(ARMED.get().is_none());
            unsafe { fault() };
            panic!("fatal fault returned");
        }
        let unlisted = matches!(mode.as_str(), "unmatched" | "handled-unlisted");
        let rva = fault as *const () as usize + 3 - base + usize::from(unlisted);
        // Exercise settings parsing, mixed prefixes and lookup beyond the first list entry.
        use std::io::Write;
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(crate::log::folder().unwrap().join("settings.txt"))
            .unwrap();
        writeln!(file, "crash-av-rva 0x{:x}, 0X{rva:X}", rva + 1).unwrap();
        drop(file);
        install();
        assert!(ARMED.get().is_some());
        if mode == "normal" {
            return;
        }
        if mode.starts_with("handled") {
            // Run after locum's first-chance observer, then recover by skipping the fault instruction.
            assert!(!unsafe { AddVectoredExceptionHandler(0, Some(handled)) }.is_null());
        }
        if mode == "replaced" {
            unsafe { SetUnhandledExceptionFilter(Some(replacement)) };
        }
        if mode == "external" {
            // RaiseException's fault site is in Windows, outside this test executable.
            unsafe {
                windows::Win32::System::Diagnostics::Debug::RaiseException(
                    EXCEPTION_ACCESS_VIOLATION.0 as u32,
                    0,
                    None,
                )
            };
            panic!("fatal external exception returned");
        }
        unsafe { fault() };
        assert!(mode.starts_with("handled"));
        assert_eq!(FIRED.load(Ordering::Acquire), !unlisted);
        unsafe { fault() }; // One-shot observation: a later handled AV must not launch/write another dump.
    }
}

unsafe fn observe(info: *const EXCEPTION_POINTERS, fatal: bool) {
    if let Some(armed) = ARMED.get()
        && let Some(info_ref) = unsafe { info.as_ref() }
        && let Some(record) = unsafe { info_ref.ExceptionRecord.as_ref() }
        && record.ExceptionCode == EXCEPTION_ACCESS_VIOLATION
        && (fatal || armed.addresses.contains(&(record.ExceptionAddress as usize)))
        && !FIRED.swap(true, Ordering::AcqRel)
    {
        EXCEPTION[0].store(unsafe { GetCurrentThreadId() } as usize, Ordering::Relaxed);
        EXCEPTION[1].store(info as usize, Ordering::Release);
        if unsafe { SetEvent(HANDLE(armed.request as *mut _)) }.is_ok() {
            let _ = unsafe { WaitForSingleObject(HANDLE(armed.ready as *mut _), 120_000) };
        }
    }
}

unsafe extern "system" fn vectored(info: *mut EXCEPTION_POINTERS) -> i32 {
    unsafe { observe(info, false) };
    EXCEPTION_CONTINUE_SEARCH
}

unsafe extern "system" fn filter(info: *const EXCEPTION_POINTERS) -> i32 {
    unsafe { observe(info, true) };
    let previous = PREVIOUS.load(Ordering::Acquire);
    if previous != 0 {
        let previous: unsafe extern "system" fn(*const EXCEPTION_POINTERS) -> i32 =
            unsafe { core::mem::transmute(previous) };
        unsafe { previous(info) }
    } else {
        EXCEPTION_CONTINUE_SEARCH
    }
}
