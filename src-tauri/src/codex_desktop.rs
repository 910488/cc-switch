use serde::Serialize;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CodexDesktopRefreshResult {
    pub stopped_processes: usize,
    pub respawned: bool,
}

/// Match only the Codex executable bundled in the signed Microsoft Store
/// Desktop package. Other copies (Codex CLI, VS Code, Antigravity, etc.) must
/// never be stopped by model-catalog refresh.
fn is_official_desktop_codex_path(path: &str) -> bool {
    let normalized = path.replace('/', "\\").to_ascii_lowercase();
    normalized.ends_with("\\app\\resources\\codex.exe")
        && normalized.contains("\\windowsapps\\openai.codex_")
}

#[cfg(target_os = "windows")]
mod windows {
    use super::{is_official_desktop_codex_path, CodexDesktopRefreshResult};
    use std::collections::HashSet;
    use std::mem::{size_of, zeroed};
    use std::thread;
    use std::time::{Duration, Instant};
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE};

    const TH32CS_SNAPPROCESS: u32 = 0x0000_0002;
    const PROCESS_TERMINATE: u32 = 0x0000_0001;
    const PROCESS_QUERY_LIMITED_INFORMATION: u32 = 0x0000_1000;

    #[allow(non_snake_case)]
    #[repr(C)]
    struct ProcessEntry32W {
        dwSize: u32,
        cntUsage: u32,
        th32ProcessID: u32,
        th32DefaultHeapID: usize,
        th32ModuleID: u32,
        cntThreads: u32,
        th32ParentProcessID: u32,
        pcPriClassBase: i32,
        dwFlags: u32,
        szExeFile: [u16; 260],
    }

    #[link(name = "kernel32")]
    extern "system" {
        fn CreateToolhelp32Snapshot(flags: u32, process_id: u32) -> HANDLE;
        fn Process32FirstW(snapshot: HANDLE, entry: *mut ProcessEntry32W) -> i32;
        fn Process32NextW(snapshot: HANDLE, entry: *mut ProcessEntry32W) -> i32;
        fn OpenProcess(desired_access: u32, inherit_handle: i32, process_id: u32) -> HANDLE;
        fn QueryFullProcessImageNameW(
            process: HANDLE,
            flags: u32,
            name: *mut u16,
            size: *mut u32,
        ) -> i32;
        fn TerminateProcess(process: HANDLE, exit_code: u32) -> i32;
    }

    struct OwnedHandle(HANDLE);

    impl Drop for OwnedHandle {
        fn drop(&mut self) {
            if !self.0.is_null() && self.0 != INVALID_HANDLE_VALUE {
                // SAFETY: this wrapper owns the handle and closes it exactly once.
                unsafe { CloseHandle(self.0) };
            }
        }
    }

    fn process_image_path(pid: u32) -> Option<String> {
        // SAFETY: OpenProcess is called with a PID obtained from ToolHelp and no
        // handle inheritance. A null handle is handled below.
        let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
        if handle.is_null() {
            return None;
        }
        let handle = OwnedHandle(handle);
        let mut buffer = vec![0_u16; 32_768];
        let mut length = buffer.len() as u32;
        // SAFETY: buffer is writable for `length` UTF-16 code units and the
        // process handle remains alive for the duration of the call.
        let ok =
            unsafe { QueryFullProcessImageNameW(handle.0, 0, buffer.as_mut_ptr(), &mut length) };
        (ok != 0).then(|| String::from_utf16_lossy(&buffer[..length as usize]))
    }

    fn official_desktop_processes() -> Result<Vec<(u32, String)>, String> {
        // SAFETY: no process-specific ID is required for TH32CS_SNAPPROCESS.
        let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
        if snapshot == INVALID_HANDLE_VALUE {
            return Err(format!(
                "Unable to inspect Codex Desktop processes: {}",
                std::io::Error::last_os_error()
            ));
        }
        let snapshot = OwnedHandle(snapshot);
        // SAFETY: PROCESSENTRY32W is a plain Windows FFI structure whose size is
        // initialized before Process32FirstW/NextW use it.
        let mut entry: ProcessEntry32W = unsafe { zeroed() };
        entry.dwSize = size_of::<ProcessEntry32W>() as u32;
        let mut result = Vec::new();
        // SAFETY: `entry` has the required size and remains writable.
        let mut has_entry = unsafe { Process32FirstW(snapshot.0, &mut entry) } != 0;
        while has_entry {
            let pid = entry.th32ProcessID;
            if pid != 0 {
                if let Some(path) = process_image_path(pid) {
                    if is_official_desktop_codex_path(&path) {
                        result.push((pid, path));
                    }
                }
            }
            // SAFETY: same valid snapshot and initialized entry as above.
            has_entry = unsafe { Process32NextW(snapshot.0, &mut entry) } != 0;
        }
        Ok(result)
    }

    fn terminate(pid: u32) -> Result<(), String> {
        // SAFETY: PID came from the immediately preceding process enumeration.
        let handle = unsafe {
            OpenProcess(
                PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_TERMINATE,
                0,
                pid,
            )
        };
        if handle.is_null() {
            return Err(format!(
                "Unable to open Codex Desktop model service (PID {pid}): {}",
                std::io::Error::last_os_error()
            ));
        }
        let handle = OwnedHandle(handle);
        // SAFETY: the handle was opened with PROCESS_TERMINATE.
        if unsafe { TerminateProcess(handle.0, 0) } == 0 {
            return Err(format!(
                "Unable to refresh Codex Desktop model service (PID {pid}): {}",
                std::io::Error::last_os_error()
            ));
        }
        Ok(())
    }

    pub(super) fn refresh() -> Result<CodexDesktopRefreshResult, String> {
        let processes = official_desktop_processes()?;
        if processes.is_empty() {
            return Ok(CodexDesktopRefreshResult {
                stopped_processes: 0,
                respawned: false,
            });
        }

        let old_pids: HashSet<u32> = processes.iter().map(|(pid, _)| *pid).collect();
        for (pid, _) in &processes {
            terminate(*pid)?;
        }

        // The Desktop host supervises app-server and starts a replacement. Wait
        // briefly so the UI can query the new model catalog before we report
        // success. We never relaunch the Desktop window itself.
        let deadline = Instant::now() + Duration::from_secs(8);
        let mut respawned = false;
        while Instant::now() < deadline {
            thread::sleep(Duration::from_millis(100));
            if official_desktop_processes()?
                .iter()
                .any(|(pid, _)| !old_pids.contains(pid))
            {
                respawned = true;
                break;
            }
        }

        Ok(CodexDesktopRefreshResult {
            stopped_processes: processes.len(),
            respawned,
        })
    }
}

pub fn refresh_model_service() -> Result<CodexDesktopRefreshResult, String> {
    #[cfg(target_os = "windows")]
    {
        windows::refresh()
    }
    #[cfg(not(target_os = "windows"))]
    {
        Ok(CodexDesktopRefreshResult {
            stopped_processes: 0,
            respawned: false,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::is_official_desktop_codex_path;

    #[test]
    fn matches_only_microsoft_store_codex_desktop_binary() {
        assert!(is_official_desktop_codex_path(
            r"C:\Program Files\WindowsApps\OpenAI.Codex_26.715.7063.0_x64__2p2nqsd0c76g0\app\resources\codex.exe"
        ));
        assert!(is_official_desktop_codex_path(
            r"\\?\C:/Program Files/WindowsApps/OpenAI.Codex_1.0_x64__publisher/app/resources/codex.exe"
        ));
        assert!(!is_official_desktop_codex_path(
            r"C:\Users\me\AppData\Roaming\npm\codex.exe"
        ));
        assert!(!is_official_desktop_codex_path(
            r"C:\Program Files\Antigravity\resources\codex.exe"
        ));
        assert!(!is_official_desktop_codex_path(
            r"C:\Program Files\WindowsApps\OpenAI.Codex_1.0_x64__publisher\ChatGPT.exe"
        ));
    }
}
