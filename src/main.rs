use clap::Parser;
use std::io;
use std::net::TcpListener;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

#[derive(Parser)]
#[command(
    name = "deucalion-bridge",
    about = "Injects deucalion.dll into FFXIV and forwards its named pipe over TCP \
             so a native Linux Teamcraft can connect without Wine"
)]
struct Args {
    /// Windows path to deucalion.dll (e.g. C:\deucalion\deucalion.dll)
    #[arg(long)]
    dll_path: String,

    /// TCP port to listen on
    #[arg(long, default_value_t = 31594)]
    port: u16,
}

fn main() {
    let args = Args::parse();
    if let Err(e) = run(&args) {
        eprintln!("[bridge] fatal: {e:#}");
        std::process::exit(1);
    }
}

// ── Platform dispatch ─────────────────────────────────────────────────────────

#[cfg(not(windows))]
fn run(_: &Args) -> anyhow::Result<()> {
    anyhow::bail!("deucalion-bridge is a Windows-only binary; run it under Wine")
}

#[cfg(windows)]
fn run(args: &Args) -> anyhow::Result<()> {
    use win::{find_pid, inject_dll, Pipe};

    // 1. Wait for the game process to appear.
    eprintln!("[bridge] Waiting for ffxiv_dx11.exe …");
    let mut game_wait_ticks = 0u32; // each tick = 500 ms
    let pid = loop {
        match find_pid("ffxiv_dx11.exe")? {
            Some(pid) => break pid,
            None => {
                thread::sleep(Duration::from_millis(500));
                game_wait_ticks += 1;
                if game_wait_ticks % 20 == 0 {
                    let secs = game_wait_ticks / 2;
                    eprintln!("[bridge] Still waiting for ffxiv_dx11.exe ({secs} s) — start the game to enable packet capture");
                }
            }
        }
    };
    eprintln!("[bridge] PID {pid}");

    // 2. Inject deucalion.dll via remote thread + LoadLibraryW.
    inject_dll(pid, &args.dll_path)
        .map_err(|e| anyhow::anyhow!("injection failed: {e}"))?;
    eprintln!("[bridge] deucalion.dll injected");

    // 3. Poll for the named pipe that deucalion creates after loading.
    //    If the pipe never appears within 30 s the DLL likely failed to
    //    initialise — most commonly because this deucalion build does not
    //    yet support the current FFXIV patch.
    let pipe_path = format!("\\\\.\\pipe\\deucalion-{pid}");
    eprintln!("[bridge] Waiting for {pipe_path} …");
    let pipe_deadline = std::time::Instant::now() + Duration::from_secs(30);
    let pipe = loop {
        match Pipe::open(&pipe_path) {
            Ok(p) => break p,
            Err(_) => {
                if std::time::Instant::now() >= pipe_deadline {
                    eprintln!(
                        "[bridge] Timed out waiting for {pipe_path} after 30 s."
                    );
                    eprintln!(
                        "[bridge] Deucalion did not create its pipe. \
                         Possible causes: the DLL does not support this FFXIV version yet, \
                         or it failed to load for another reason. \
                         Check https://github.com/ff14wed/deucalion/releases for updates."
                    );
                    std::process::exit(2);
                }
                thread::sleep(Duration::from_millis(200));
            }
        }
    };
    eprintln!("[bridge] Pipe open");

    // 4. Accept exactly one TCP client; the Teamcraft Electron app connects here.
    let bind_addr = format!("127.0.0.1:{}", args.port);
    let listener = TcpListener::bind(&bind_addr)
        .map_err(|e| anyhow::anyhow!("bind {bind_addr}: {e}"))?;
    eprintln!("[bridge] Listening on {bind_addr}");
    let (tcp, peer) = listener.accept()?;
    eprintln!("[bridge] Client {peer}");
    drop(listener); // stop accepting further connections

    // 5. Forward bytes in both directions until either side closes.
    let pipe = Arc::new(pipe);

    let pipe_rx = Arc::clone(&pipe);
    let mut tcp_tx = tcp.try_clone()?;
    let t1 = thread::spawn(move || -> io::Result<()> {
        let mut buf = vec![0u8; 8192];
        loop {
            let n = pipe_rx.read(&mut buf)?;
            if n == 0 {
                break;
            }
            use std::io::Write;
            tcp_tx.write_all(&buf[..n])?;
        }
        eprintln!("[bridge] pipe→tcp finished");
        // Send TCP FIN so the client (Node.js) closes its end too, which
        // will unblock the tcp→pipe thread and let the bridge exit cleanly.
        let _ = tcp_tx.shutdown(std::net::Shutdown::Write);
        Ok(())
    });

    let pipe_tx = Arc::clone(&pipe);
    let mut tcp_rx = tcp;
    let t2 = thread::spawn(move || -> io::Result<()> {
        let mut buf = vec![0u8; 8192];
        loop {
            use std::io::Read;
            let n = tcp_rx.read(&mut buf)?;
            if n == 0 {
                break;
            }
            pipe_tx.write(&buf[..n])?;
        }
        eprintln!("[bridge] tcp→pipe finished");
        Ok(())
    });

    let _ = t1.join();
    let _ = t2.join();

    eprintln!("[bridge] done");
    Ok(())
}

// ── Win32 wrappers ────────────────────────────────────────────────────────────

#[cfg(windows)]
mod win {
    use anyhow::Context;
    use std::io;
    use windows::Win32::Foundation::*;
    use windows::Win32::Security::Authorization::*;
    use windows::Win32::Security::*;
    use windows::Win32::Storage::FileSystem::*;
    use windows::Win32::System::Diagnostics::Debug::*;
    use windows::Win32::System::Diagnostics::ToolHelp::*;
    use windows::Win32::System::LibraryLoader::*;
    use windows::Win32::System::Memory::*;
    use windows::Win32::System::Threading::*;
    use windows::core::{w, PCSTR, PCWSTR};

    // ── Process enumeration ───────────────────────────────────────────────────

    pub fn find_pid(name: &str) -> anyhow::Result<Option<u32>> {
        let name_wide: Vec<u16> = name.encode_utf16().collect();
        unsafe {
            let snap = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0)
                .context("CreateToolhelp32Snapshot")?;

            let mut entry = PROCESSENTRY32W {
                dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
                ..Default::default()
            };
            let mut found = None;

            if Process32FirstW(snap, &mut entry).is_ok() {
                loop {
                    let exe = &entry.szExeFile;
                    let len = exe.iter().position(|&c| c == 0).unwrap_or(exe.len());
                    if exe[..len] == name_wide[..] {
                        found = Some(entry.th32ProcessID);
                        break;
                    }
                    if Process32NextW(snap, &mut entry).is_err() {
                        break;
                    }
                }
            }
            let _ = CloseHandle(snap);
            Ok(found)
        }
    }

    // ── DLL injection ─────────────────────────────────────────────────────────

    /// Copies our own DACL onto the target process so OpenProcess(PROCESS_ALL_ACCESS)
    /// succeeds — mirrors the "winterACLShit" technique from dll-inject/functions.cc.
    fn fix_acl(pid: u32) {
        unsafe {
            // WRITE_DAC and READ_CONTROL are generic object rights (FILE_ACCESS_RIGHTS),
            // so we combine them as raw u32 values into a PROCESS_ACCESS_RIGHTS.
            let limited_access = PROCESS_ACCESS_RIGHTS(
                WRITE_DAC.0 | READ_CONTROL.0 | PROCESS_QUERY_LIMITED_INFORMATION.0,
            );
            let Ok(target) = OpenProcess(limited_access, false, pid) else {
                return;
            };

            let mut dacl: *mut ACL = std::ptr::null_mut();
            let mut sd = PSECURITY_DESCRIPTOR::default();

            if GetSecurityInfo(
                GetCurrentProcess(),
                SE_KERNEL_OBJECT,
                DACL_SECURITY_INFORMATION,
                None,
                None,
                Some(&mut dacl),
                None,
                Some(&mut sd),
            )
            .is_ok()
            {
                let _ = SetSecurityInfo(
                    target,
                    SE_KERNEL_OBJECT,
                    DACL_SECURITY_INFORMATION | UNPROTECTED_DACL_SECURITY_INFORMATION,
                    None,
                    None,
                    Some(dacl as *const ACL),
                    None,
                );
            }
            let _ = CloseHandle(target);
        }
    }

    pub fn inject_dll(pid: u32, dll_path: &str) -> anyhow::Result<()> {
        // Best-effort ACL fixup; failure is non-fatal (may already have access).
        fix_acl(pid);

        // Encode the DLL path as a null-terminated UTF-16 string for WriteProcessMemory.
        let path_wide: Vec<u16> = dll_path.encode_utf16().chain(std::iter::once(0)).collect();
        let path_bytes = path_wide.len() * std::mem::size_of::<u16>();

        unsafe {
            let process =
                OpenProcess(PROCESS_ALL_ACCESS, false, pid).context("OpenProcess")?;

            // Resolve LoadLibraryW inside kernel32 — this address is the same in all
            // processes on the same Windows session, so we can call it remotely.
            let kernel32 = GetModuleHandleW(w!("kernel32.dll"))
                .context("GetModuleHandleW(kernel32.dll)")?;
            let load_lib = GetProcAddress(kernel32, PCSTR(b"LoadLibraryW\0".as_ptr()))
                .context("GetProcAddress(LoadLibraryW)")?;

            // Allocate memory in FFXIV's address space for the path string.
            let remote_mem = VirtualAllocEx(
                process,
                None,
                path_bytes,
                MEM_RESERVE | MEM_COMMIT,
                PAGE_READWRITE,
            );
            anyhow::ensure!(!remote_mem.is_null(), "VirtualAllocEx failed");

            WriteProcessMemory(
                process,
                remote_mem,
                path_wide.as_ptr() as *const _,
                path_bytes,
                None,
            )
            .context("WriteProcessMemory")?;

            // Kick off LoadLibraryW(path) in a remote thread inside FFXIV.
            // SAFETY: load_lib is a valid function pointer of the correct calling convention.
            let load_lib_fn: unsafe extern "system" fn(*mut std::ffi::c_void) -> u32 =
                std::mem::transmute(load_lib);
            let thread = CreateRemoteThread(
                process,
                None,
                0,
                Some(load_lib_fn),
                Some(remote_mem),
                0,
                None,
            )
            .context("CreateRemoteThread")?;

            let _ = CloseHandle(thread);
            let _ = CloseHandle(process);
        }
        Ok(())
    }

    // ── Named pipe wrapper ────────────────────────────────────────────────────

    /// A Win32 named pipe handle that is safe to share across threads for
    /// simultaneous reads and writes (Windows named pipes are full-duplex).
    pub struct Pipe(HANDLE);

    // SAFETY: Windows HANDLE is valid to send across threads; we use it only for
    // concurrent pipe I/O (one thread reading, one writing) which is well-defined.
    unsafe impl Send for Pipe {}
    unsafe impl Sync for Pipe {}

    impl Drop for Pipe {
        fn drop(&mut self) {
            unsafe {
                let _ = CloseHandle(self.0);
            }
        }
    }

    impl Pipe {
        pub fn open(path: &str) -> anyhow::Result<Self> {
            let path_wide: Vec<u16> = path.encode_utf16().chain(std::iter::once(0)).collect();
            unsafe {
                let h = CreateFileW(
                    PCWSTR(path_wide.as_ptr()),
                    (GENERIC_READ | GENERIC_WRITE).0,
                    FILE_SHARE_NONE,
                    None,
                    OPEN_EXISTING,
                    FILE_ATTRIBUTE_NORMAL,
                    None,
                )
                .context("CreateFileW")?;
                Ok(Pipe(h))
            }
        }

        /// Blocking read. Returns 0 on pipe closure (EOF).
        pub fn read(&self, buf: &mut [u8]) -> io::Result<usize> {
            let mut n = 0u32;
            unsafe {
                match ReadFile(self.0, Some(buf), Some(&mut n), None) {
                    Ok(()) => Ok(n as usize),
                    Err(e)
                        if e.code() == ERROR_BROKEN_PIPE.to_hresult()
                            || e.code() == ERROR_PIPE_NOT_CONNECTED.to_hresult() =>
                    {
                        Ok(0) // treat broken/disconnected pipe as EOF
                    }
                    Err(e) => Err(io::Error::new(io::ErrorKind::Other, e.message())),
                }
            }
        }

        /// Blocking write.
        pub fn write(&self, buf: &[u8]) -> io::Result<()> {
            let mut written = 0u32;
            unsafe {
                WriteFile(self.0, Some(buf), Some(&mut written), None)
                    .map_err(|e| io::Error::new(io::ErrorKind::Other, e.message()))
            }
        }
    }
}
