//! Everything that talks to Windows directly: the Explorer menu entry, elevation and the Recycle Bin.

use std::path::Path;

/// "Scan with treespace" in Explorer's right-click menu for folders, folder backgrounds and drives,
/// registered for the current user only (no administrator rights). Windows 11 lists it under
/// "Show more options": the top-level menu needs an IExplorerCommand DLL in a signed sparse package.
#[cfg(windows)]
pub mod explorer_menu {
    use std::{ffi::OsStr, os::windows::ffi::OsStrExt, ptr};
    use windows_sys::Win32::{
        Foundation::{ERROR_FILE_NOT_FOUND, ERROR_SUCCESS},
        System::Registry::{HKEY_CURRENT_USER, REG_SZ, RRF_RT_REG_SZ, RegDeleteTreeW, RegGetValueW, RegSetKeyValueW},
    };

    const KEYS: [&str; 3] = [
        r"Software\Classes\Directory\shell\treespace",
        r"Software\Classes\Directory\Background\shell\treespace",
        r"Software\Classes\Drive\shell\treespace",
    ];

    fn wide(s: &str) -> Vec<u16> {
        OsStr::new(s).encode_wide().chain([0]).collect()
    }

    /// The command Explorer runs: this exe, wherever it is now, with the clicked folder.
    fn command() -> Option<String> {
        Some(format!("\"{}\" \"%V\"", std::env::current_exe().ok()?.display()))
    }

    fn set(key: &str, value: &str) -> bool {
        let data = wide(value);
        let size = (data.len() * 2) as u32;
        // SAFETY: the key and the data are NUL-terminated UTF-16 buffers that outlive the call (the key's
        // temporary lives to the end of the statement), `size` is exactly the data's length in bytes, and a
        // null value name is documented to mean the key's default value.
        unsafe { RegSetKeyValueW(HKEY_CURRENT_USER, wide(key).as_ptr(), ptr::null(), REG_SZ, data.as_ptr().cast(), size) == ERROR_SUCCESS }
    }

    fn get(key: &str) -> Option<String> {
        let mut buf = vec![0u16; 1024];
        let mut bytes = (buf.len() * 2) as u32;
        // SAFETY: the key is a NUL-terminated UTF-16 buffer alive for the call, `bytes` holds the size of
        // `buf` in bytes so Windows writes at most that much, and the value name and type outputs may be null.
        let status = unsafe {
            RegGetValueW(HKEY_CURRENT_USER, wide(key).as_ptr(), ptr::null(), RRF_RT_REG_SZ, ptr::null_mut(), buf.as_mut_ptr().cast(), &mut bytes)
        };
        (status == ERROR_SUCCESS).then(|| String::from_utf16_lossy(&buf[..(bytes as usize / 2).saturating_sub(1)]))
    }

    /// True when every entry exists and launches this exe.
    pub fn installed() -> bool {
        let Some(expected) = command() else { return false };
        KEYS.iter().all(|key| get(&format!(r"{key}\command")).as_deref() == Some(expected.as_str()))
    }

    pub fn install() -> bool {
        let Some(command) = command() else { return false };
        KEYS.iter().all(|key| set(key, "Scan with treespace") && set(&format!(r"{key}\command"), &command))
    }

    pub fn uninstall() -> bool {
        KEYS.iter().all(|key| {
            // SAFETY: the key is a NUL-terminated UTF-16 buffer that lives to the end of the statement.
            let status = unsafe { RegDeleteTreeW(HKEY_CURRENT_USER, wide(key).as_ptr()) };
            status == ERROR_SUCCESS || status == ERROR_FILE_NOT_FOUND
        })
    }
}

/// Starts this program again elevated (UAC prompt) scanning `path`; true if Windows started it.
#[cfg(windows)]
pub fn relaunch_as_admin(path: &str) -> bool {
    use std::{ffi::OsStr, os::windows::ffi::OsStrExt, ptr};
    use windows_sys::Win32::UI::{Shell::ShellExecuteW, WindowsAndMessaging::SW_SHOWNORMAL};

    let wide = |s: &OsStr| s.encode_wide().chain([0]).collect::<Vec<u16>>();
    let Ok(exe) = std::env::current_exe() else { return false };
    // A trailing backslash would escape the closing quote: "C:\" must be written "C:\\".
    let arg = if path.ends_with('\\') { format!("\"{path}\\\"") } else { format!("\"{path}\"") };
    let (verb, exe, arg) = (wide(OsStr::new("runas")), wide(exe.as_os_str()), wide(OsStr::new(&arg)));
    // SAFETY: verb, exe and arg are NUL-terminated UTF-16 buffers alive for the call; the owner window and
    // the working directory are optional and may be null.
    let result = unsafe { ShellExecuteW(ptr::null_mut(), verb.as_ptr(), exe.as_ptr(), arg.as_ptr(), ptr::null(), SW_SHOWNORMAL) };
    result as isize > 32 // ShellExecuteW reports success as a value above 32
}

/// Moves `path` to the Recycle Bin through the Windows shell, which shows its own progress and,
/// thanks to FOF_WANTNUKEWARNING, asks before anything would be deleted for good (an item too large
/// for the bin, or a drive without one).
#[cfg(windows)]
pub fn recycle(path: &Path) -> Result<(), String> {
    use crate::display;
    use std::{ffi::OsStr, os::windows::ffi::OsStrExt, ptr};
    use windows_sys::Win32::{
        System::Com::{COINIT_APARTMENTTHREADED, CoInitializeEx},
        UI::Shell::{FO_DELETE, FOF_ALLOWUNDO, FOF_NOCONFIRMATION, FOF_WANTNUKEWARNING, SHFILEOPSTRUCTW, SHFileOperationW},
    };

    // See ADMIN_DELETE. Running the move from an unelevated helper would allow it.
    if elevated() {
        return Err(ADMIN_DELETE.to_string());
    }
    // The shell takes plain paths, which Windows normalizes (e.g. drops trailing dots), so the plain
    // form of `x.` names `x`: only go ahead when it still resolves to exactly this item.
    let plain = display(path);
    // A name that is not valid Unicode shows (and may have been stored) with U+FFFD, which could be
    // the real name of a different item: never act on it.
    if plain.contains('\u{FFFD}') {
        return Err(format!("{plain} has a name that is not valid Unicode; delete it from Explorer"));
    }
    if std::fs::canonicalize(&plain).ok().as_deref() != Some(path) {
        return Err(format!("{plain} cannot be handed to the Recycle Bin safely: Windows would read its name differently"));
    }
    let mut from: Vec<u16> = OsStr::new(&plain).encode_wide().collect();
    from.extend([0, 0]); // a list of paths, ended by an empty one
    let mut op = SHFILEOPSTRUCTW {
        hwnd: ptr::null_mut(),
        wFunc: FO_DELETE,
        pFrom: from.as_ptr(),
        pTo: ptr::null(),
        fFlags: (FOF_ALLOWUNDO | FOF_NOCONFIRMATION | FOF_WANTNUKEWARNING) as u16,
        fAnyOperationsAborted: 0,
        hNameMappings: ptr::null_mut(),
        lpszProgressTitle: ptr::null(),
    };
    // SAFETY: the reserved argument must be null and is; this runs on the deletion's own thread, so the
    // apartment it picks cannot clash with one chosen by other code on that thread.
    unsafe { CoInitializeEx(ptr::null(), COINIT_APARTMENTTHREADED as u32) };
    // SAFETY: `op` is fully initialized and only borrowed for the call; `pFrom` points into `from`, a
    // double-NUL-terminated list alive until after it; FO_DELETE ignores the null `pTo`, and the null name
    // mappings and progress title are only used with flags not set here.
    let code = unsafe { SHFileOperationW(&mut op) };
    if code != 0 {
        return Err(format!("Windows could not move {plain} to the Recycle Bin (code {code:#x})"));
    }
    if op.fAnyOperationsAborted != 0 || path.exists() {
        return Err(format!("{plain} was not fully moved (cancelled, or items in use); rescan to see what is left"));
    }
    Ok(())
}

#[cfg(not(windows))]
pub fn recycle(_: &Path) -> Result<(), String> {
    Err("Moving to the trash is only implemented on Windows".to_string())
}

/// Elevated, another program could swap a folder on the path for a junction between the safety
/// check and the shell's move, steering the deletion into files the user could not delete.
pub const ADMIN_DELETE: &str = "Disabled while treespace runs as administrator: a folder swapped during the move \
     could make it delete protected files. Open treespace normally to delete.";

#[cfg(windows)]
pub fn elevated() -> bool {
    // SAFETY: IsUserAnAdmin takes no arguments and only reads the calling process's token.
    unsafe { windows_sys::Win32::UI::Shell::IsUserAnAdmin() != 0 }
}

#[cfg(not(windows))]
pub fn elevated() -> bool {
    false
}
