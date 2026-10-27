//! Disk usage scanning shared by the `treespace` CLI and the `treespace-gui` window.

pub mod mft;

use rayon::prelude::*;
use std::{
    cmp::Reverse,
    collections::BinaryHeap,
    fs, io,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

pub struct Dir {
    pub path: PathBuf,
    pub size: u64,
    pub allocated: u64,
    pub files: u64,
    pub errors: u64,
    pub children: Vec<Dir>,
}

impl Dir {
    /// Updates the tree after a deletion under `target`: subtracts the given totals from `target`
    /// and every folder above it, and drops `target` from the tree when `remove` is set (the folder
    /// itself was deleted). Returns false, changing nothing, if `target` is not in this tree.
    pub fn deleted(&mut self, target: &Path, size: u64, allocated: u64, files: u64, remove: bool) -> bool {
        if self.path != target {
            let Some(i) = self.children.iter().position(|c| target.starts_with(&c.path)) else { return false };
            if !self.children[i].deleted(target, size, allocated, files, remove) {
                return false;
            }
            if remove && self.children[i].path == target {
                self.children.remove(i);
            }
            self.children.sort_unstable_by_key(|c| Reverse(c.allocated));
        }
        self.size = self.size.saturating_sub(size);
        self.allocated = self.allocated.saturating_sub(allocated);
        self.files = self.files.saturating_sub(files);
        true
    }
}

/// How a scan read the disk.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Engine {
    Mft,
    Walk,
}

impl std::fmt::Display for Engine {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.write_str(match self {
            Engine::Mft => "MFT",
            Engine::Walk => "directory walk",
        })
    }
}

pub struct Scan {
    pub tree: Dir,
    pub engine: Engine,
    /// Why the fast MFT read was not used, when it was expected to be.
    pub note: Option<String>,
    pub elapsed: Duration,
}

/// Scans `root` through the MFT when it is a drive root (as admin) or an NTFS image/export,
/// otherwise (or with `walk`) by opening every folder.
pub fn scan_path(root: &Path, walk: bool) -> io::Result<Scan> {
    // Absolute path; on Windows a \\?\ path, so CreateFileW also opens paths longer than 260 characters.
    let absolute = fs::canonicalize(root)?;
    let started = Instant::now();
    let attempt = if walk { Err(None) } else { mft_scan(&absolute) };
    let (mut tree, engine, note) = match attempt {
        Ok(tree) => (tree, Engine::Mft, None),
        Err(note) => (walk_dir(&absolute), Engine::Walk, note),
    };
    tree.path = absolute;
    Ok(Scan { tree, engine, note, elapsed: started.elapsed() })
}

/// The MFT read of `path`, or why the directory walk is used instead (None: no MFT applies, walk quietly).
fn mft_scan(path: &Path) -> Result<Dir, Option<String>> {
    let failed = |device: &Path, e: io::Error| Some(format!("MFT read of {} failed ({e}); used the directory walk", device.display()));
    if let Some(device) = volume(path) {
        return mft::scan(&device, path.to_path_buf()).map_err(|e| match e.kind() {
            io::ErrorKind::PermissionDenied => Some("Fast MFT scan needs administrator rights; used the directory walk".to_string()),
            _ => failed(&device, e),
        });
    }
    // As administrator a folder on an NTFS drive is cut out of a whole-volume MFT read: a constant
    // few seconds even on a cold cache, hard links counted once. Without access it walks quietly.
    let (device, root) = drive(path).ok_or(None)?;
    match mft::scan(&device, root) {
        Ok(volume) => subtree(volume, path).ok_or_else(|| Some(format!("{} is not in the MFT; used the directory walk", path.display()))),
        Err(e) if e.kind() == io::ErrorKind::PermissionDenied => Err(None),
        Err(e) => Err(failed(&device, e)),
    }
}

/// The folder at `target` with its subtree, taken out of `tree` (an MFT tree covers a whole volume).
fn subtree(mut tree: Dir, target: &Path) -> Option<Dir> {
    while tree.path != target {
        let i = tree.children.iter().position(|c| target.starts_with(&c.path))?;
        tree = tree.children.swap_remove(i);
    }
    Some(tree)
}

/// Scans `path` by opening every folder, in parallel. Unlike the MFT scan, a hard-linked file
/// is counted once per link.
pub fn walk_dir(path: &Path) -> Dir {
    let mut dir = Dir {
        path: path.to_path_buf(),
        size: 0,
        allocated: 0,
        files: 0,
        errors: 0,
        children: Vec::new(),
    };
    let mut subdirs = Vec::new();
    list(path, &mut dir, &mut subdirs);
    dir.children = subdirs.par_iter().map(|p| walk_dir(p)).collect();
    finish(&mut dir);
    dir
}

/// Adds the children's totals to `dir` and sorts them largest first.
fn finish(dir: &mut Dir) {
    for child in &dir.children {
        dir.size += child.size;
        dir.allocated += child.allocated;
        dir.files += child.files;
        dir.errors += child.errors;
    }
    dir.children.sort_unstable_by_key(|c| Reverse(c.allocated));
}

/// The raw volume to read the MFT from: a drive root on Windows, or an NTFS image/device file anywhere.
pub fn volume(path: &Path) -> Option<PathBuf> {
    if !path.is_dir() {
        return Some(path.to_path_buf());
    }
    drive(path).filter(|_| path.parent().is_none()).map(|(device, _)| device)
}

/// For a path on a lettered drive: its raw volume (`\\.\C:`) and root folder (`\\?\C:\`).
#[cfg(windows)]
fn drive(path: &Path) -> Option<(PathBuf, PathBuf)> {
    use std::path::{Component, Prefix};
    match path.components().next()? {
        Component::Prefix(prefix) => match prefix.kind() {
            Prefix::VerbatimDisk(letter) | Prefix::Disk(letter) => {
                let letter = letter as char;
                Some((PathBuf::from(format!(r"\\.\{letter}:")), PathBuf::from(format!(r"\\?\{letter}:\"))))
            }
            _ => None,
        },
        _ => None,
    }
}

#[cfg(not(windows))]
fn drive(_: &Path) -> Option<(PathBuf, PathBuf)> {
    None
}

/// Adds the files directly in `path` to `dir` and collects its subdirectories.
fn list(path: &Path, dir: &mut Dir, subdirs: &mut Vec<PathBuf>) {
    let (mut size, mut allocated, mut files) = (0, 0, 0);
    let errors = entries(path, false, |entry| match entry.name {
        Some(name) if entry.dir => subdirs.push(path.join(name)),
        _ => {
            size += entry.size;
            allocated += entry.allocated;
            files += 1;
        }
    });
    dir.size += size;
    dir.allocated += allocated;
    dir.files += files;
    dir.errors += errors;
}

pub struct FileInfo {
    pub name: String,
    pub size: u64,
    pub allocated: u64,
}

/// The files directly in `path`, largest on disk first, and how many entries could not be read.
pub fn files(path: &Path) -> (Vec<FileInfo>, u64) {
    let mut out = Vec::new();
    let errors = entries(path, true, |entry| {
        if let (false, Some(name)) = (entry.dir, entry.name) {
            out.push(FileInfo { name: name.to_string_lossy().into_owned(), size: entry.size, allocated: entry.allocated });
        }
    });
    out.sort_unstable_by_key(|f| Reverse(f.allocated));
    (out, errors)
}

pub struct TypeStat {
    /// Lower-case extension, or "(none)".
    pub ext: String,
    pub files: u64,
    pub size: u64,
    pub allocated: u64,
}

pub struct LargeFile {
    pub path: PathBuf,
    pub size: u64,
    pub allocated: u64,
}

/// What the Types and Largest tabs show for a folder and all its subfolders.
pub struct Breakdown {
    /// Space per extension, largest on disk first.
    pub types: Vec<TypeStat>,
    /// The `largest` biggest files on disk, biggest first.
    pub largest: Vec<LargeFile>,
}

/// One parallel walk of `path` and all its subfolders: totals per extension and the `largest`
/// biggest files (kept in a bounded heap, so memory does not grow with the folder).
pub fn breakdown(path: &Path, largest: usize) -> Breakdown {
    let part = walk_part(path, largest);
    let mut types: Vec<TypeStat> = part
        .types
        .into_iter()
        .map(|(ext, (files, size, allocated))| TypeStat { ext, files, size, allocated })
        .collect();
    types.sort_unstable_by(|a, b| b.allocated.cmp(&a.allocated).then_with(|| a.ext.cmp(&b.ext)));
    // Ascending order of Reverse(..) is descending by size on disk.
    let largest = part.largest.into_sorted_vec().into_iter().map(|Reverse((allocated, size, path))| LargeFile { path, size, allocated }).collect();
    Breakdown { types, largest }
}

/// Totals for part of the tree: extension -> (files, size, on disk), and a min-heap of the biggest files.
struct Part {
    types: std::collections::HashMap<String, (u64, u64, u64)>,
    largest: BinaryHeap<Reverse<(u64, u64, PathBuf)>>,
    keep: usize,
}

impl Part {
    fn new(keep: usize) -> Self {
        Part { types: Default::default(), largest: BinaryHeap::with_capacity(keep + 1), keep }
    }

    /// Keeps the file if it is among the biggest so far; its path is only built when kept.
    fn offer(&mut self, allocated: u64, size: u64, path: impl FnOnce() -> PathBuf) {
        let smallest = self.largest.peek().map(|Reverse((on_disk, ..))| *on_disk);
        if self.largest.len() < self.keep || smallest.is_some_and(|min| allocated > min) {
            self.largest.push(Reverse((allocated, size, path())));
            if self.largest.len() > self.keep {
                self.largest.pop();
            }
        }
    }

    fn merge(mut self, other: Part) -> Part {
        for (ext, (files, size, allocated)) in other.types {
            let total = self.types.entry(ext).or_default();
            *total = (total.0 + files, total.1 + size, total.2 + allocated);
        }
        for Reverse((allocated, size, path)) in other.largest {
            self.offer(allocated, size, || path);
        }
        self
    }
}

fn walk_part(path: &Path, keep: usize) -> Part {
    let (mut part, mut subdirs) = (Part::new(keep), Vec::new());
    entries(path, true, |entry| match entry.name {
        Some(name) if entry.dir => subdirs.push(path.join(name)),
        Some(name) => {
            let ext = Path::new(&name).extension().map_or("(none)".to_string(), |e| e.to_string_lossy().to_lowercase());
            let total = part.types.entry(ext).or_default();
            *total = (total.0 + 1, total.1 + entry.size, total.2 + entry.allocated);
            part.offer(entry.allocated, entry.size, || path.join(&name));
        }
        None => {}
    });
    let below = subdirs.par_iter().map(|dir| walk_part(dir, keep)).reduce(|| Part::new(keep), Part::merge);
    part.merge(below)
}

/// One directory entry reported by `entries`.
struct Entry {
    /// Always set for directories; for files only when names were asked for.
    name: Option<std::ffi::OsString>,
    dir: bool,
    size: u64,
    allocated: u64,
}

/// Calls `each` for every file and subdirectory of `path` and returns how many entries could not
/// be read. Symlinks and junctions are skipped, so link loops are never followed. Mount points are skipped
/// too, so a scan stays on one file system (like `du -x`): on Windows they are name-surrogate reparse points
/// like junctions, on Unix subdirectories on another device (scanning / skips /proc, /sys and WSL's /mnt/c).
/// File names are only built with `file_names`: a whole-drive scan does not need them.
#[cfg(windows)]
fn entries(path: &Path, file_names: bool, mut each: impl FnMut(Entry)) -> u64 {
    use std::{cell::RefCell, ffi::OsString, os::windows::{ffi::{OsStrExt, OsStringExt}, io::{AsRawHandle, FromRawHandle, OwnedHandle}}, ptr, slice};
    use windows_sys::Win32::{Foundation::*, Storage::FileSystem::*};

    // 64 KiB per batch; u64 keeps the 8-byte alignment FILE_FULL_DIR_INFO needs.
    thread_local!(static BUF: RefCell<Vec<u64>> = RefCell::new(vec![0; 8192]));
    const NAME_SURROGATE: u32 = 0x2000_0000; // IsReparseTagNameSurrogate: symlinks, junctions

    let wide: Vec<u16> = path.as_os_str().encode_wide().chain([0]).collect();
    // SAFETY: `wide` is a NUL-terminated UTF-16 path that outlives the call; the security attributes and
    // template handle may be null.
    let handle = unsafe {
        CreateFileW(
            wide.as_ptr(),
            FILE_LIST_DIRECTORY,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            ptr::null(),
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS,
            ptr::null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return 1;
    }
    // SAFETY: a valid handle CreateFileW just opened and nothing else owns; dropping it calls CloseHandle.
    let handle = unsafe { OwnedHandle::from_raw_handle(handle) };
    let mut errors = 0;
    BUF.with_borrow_mut(|buf| loop {
        let bytes = (buf.len() * 8) as u32;
        // SAFETY: the handle is open and the buffer is `bytes` long, writable and 8-byte aligned.
        if unsafe { GetFileInformationByHandleEx(handle.as_raw_handle(), FileFullDirectoryInfo, buf.as_mut_ptr().cast(), bytes) } == 0 {
            // SAFETY: no preconditions; reads this thread's last error code.
            if unsafe { GetLastError() } != ERROR_NO_MORE_FILES {
                errors += 1;
            }
            break;
        }
        let mut entry = buf.as_ptr().cast::<u8>();
        loop {
            let info = entry.cast::<FILE_FULL_DIR_INFO>();
            // SAFETY: `info` points at a FILE_FULL_DIR_INFO the OS wrote into the buffer: the first at offset 0,
            // which the u64 buffer keeps 8-byte aligned, the others at the NextEntryOffset the OS gave, trusted to
            // stay aligned and inside the batch. The buffer is not written again until the batch is done.
            let (attrs, next) = unsafe { ((*info).FileAttributes, (*info).NextEntryOffset) };
            // SAFETY: the record holds FileNameLength bytes of UTF-16 name from FileName on (u16 aligned).
            let name = unsafe { slice::from_raw_parts(ptr::addr_of!((*info).FileName).cast::<u16>(), (*info).FileNameLength as usize / 2) };
            // For reparse points EaSize holds the reparse tag (what FindFirstFile exposes as dwReserved0).
            // SAFETY: a field of the same record.
            let link = attrs & FILE_ATTRIBUTE_REPARSE_POINT != 0 && unsafe { (*info).EaSize } & NAME_SURROGATE != 0;
            let dir = attrs & FILE_ATTRIBUTE_DIRECTORY != 0;
            if !link && !(dir && (name == [b'.' as u16] || name == [b'.' as u16; 2])) {
                // AllocationSize is what the file occupies: 0 for cloud-only OneDrive files, less for sparse VHDX.
                // SAFETY: fields of the same record.
                let (size, allocated) = if dir { (0, 0) } else { unsafe { ((*info).EndOfFile as u64, (*info).AllocationSize as u64) } };
                let name = (dir || file_names).then(|| OsString::from_wide(name));
                each(Entry { name, dir, size, allocated });
            }
            if next == 0 {
                break;
            }
            // SAFETY: a nonzero NextEntryOffset is the OS's offset from this record to the next one in the batch.
            entry = unsafe { entry.add(next as usize) };
        }
    });
    errors
}

#[cfg(unix)]
fn entries(path: &Path, file_names: bool, mut each: impl FnMut(Entry)) -> u64 {
    use std::os::unix::fs::MetadataExt;

    let (Ok(list), Ok(here)) = (fs::read_dir(path), fs::metadata(path)) else { return 1 };
    let mut errors = 0;
    for entry in list {
        let Ok(entry) = entry else {
            errors += 1;
            continue;
        };
        let Ok(kind) = entry.file_type() else {
            errors += 1;
            continue;
        };
        if kind.is_dir() {
            // DirEntry::metadata does not follow symlinks; another device means a mount point.
            match entry.metadata() {
                Ok(meta) if meta.dev() != here.dev() => {}
                Ok(_) => each(Entry { name: Some(entry.file_name()), dir: true, size: 0, allocated: 0 }),
                Err(_) => errors += 1,
            }
        } else if !kind.is_symlink() {
            match entry.metadata() {
                Ok(meta) => {
                    let name = file_names.then(|| entry.file_name());
                    each(Entry { name, dir: false, size: meta.len(), allocated: meta.blocks() * 512 });
                }
                Err(_) => errors += 1,
            }
        }
    }
    errors
}

pub fn human(bytes: u64) -> String {
    const UNITS: [&str; 6] = ["B", "KiB", "MiB", "GiB", "TiB", "PiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{env, process};

    #[test]
    fn totals_sort_and_skip_symlinks() {
        let root = env::temp_dir().join(format!("treespace-test-{}", process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("sub/deeper")).unwrap();
        fs::write(root.join("a.bin"), [0u8; 10]).unwrap();
        fs::write(root.join("sub/b.bin"), [0u8; 50]).unwrap();
        fs::write(root.join("sub/deeper/c.bin"), [0u8; 25]).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(root.join("sub"), root.join("loop")).unwrap();

        let tree = walk_dir(&root);
        fs::remove_dir_all(&root).unwrap();

        assert_eq!((tree.size, tree.files, tree.errors), (85, 3, 0));
        assert_eq!(tree.children.len(), 1, "symlinked dir must not be followed");
        assert_eq!(tree.children[0].size, 75);
        assert_eq!(human(0), "0 B");
        assert_eq!(human(1536), "1.5 KiB");
    }

    #[test]
    fn files_lists_only_files_largest_first() {
        let root = env::temp_dir().join(format!("treespace-files-{}", process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("sub")).unwrap();
        fs::write(root.join("small.txt"), [0u8; 10]).unwrap();
        fs::write(root.join("big.bin"), vec![0u8; 50_000]).unwrap();

        let (list, errors) = files(&root);
        fs::remove_dir_all(&root).unwrap();

        let names: Vec<&str> = list.iter().map(|f| f.name.as_str()).collect();
        assert_eq!((names, errors), (vec!["big.bin", "small.txt"], 0));
        assert_eq!((list[0].size, list[1].size), (50_000, 10));
        assert!(list[0].allocated >= list[1].allocated);
    }

    #[test]
    fn types_group_by_lowercase_extension_recursively() {
        let root = env::temp_dir().join(format!("treespace-types-{}", process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("sub")).unwrap();
        fs::write(root.join("a.txt"), [0u8; 10]).unwrap();
        fs::write(root.join("B.TXT"), [0u8; 20]).unwrap();
        fs::write(root.join("sub/c.txt"), [0u8; 1]).unwrap();
        fs::write(root.join("d.bin"), [0u8; 5]).unwrap();
        fs::write(root.join("README"), [0u8; 3]).unwrap();

        let found = breakdown(&root, 2);
        fs::remove_dir_all(&root).unwrap();

        let rows: Vec<(&str, u64, u64)> = found.types.iter().map(|t| (t.ext.as_str(), t.files, t.size)).collect();
        assert_eq!(rows[0], ("txt", 3, 31), "most space first: {rows:?}");
        assert!(rows.contains(&("bin", 1, 5)) && rows.contains(&("(none)", 1, 3)), "{rows:?}");
        assert_eq!(rows.len(), 3);

        // Only the 2 biggest files on disk are kept, biggest first, with paths from any subfolder.
        assert_eq!(found.largest.len(), 2);
        assert!(found.largest[0].allocated >= found.largest[1].allocated);
        let paths: Vec<&Path> = found.largest.iter().map(|f| f.path.as_path()).collect();
        assert!(paths.iter().all(|p| p.starts_with(&root) && p.is_absolute()), "{paths:?}");
    }

    #[test]
    fn largest_keeps_the_biggest_across_subfolders() {
        let root = env::temp_dir().join(format!("treespace-largest-{}", process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("deep/er")).unwrap();
        for (name, kib) in [("a.bin", 64), ("deep/b.bin", 256), ("deep/er/c.bin", 128), ("d.bin", 8)] {
            fs::write(root.join(name), vec![1u8; kib * 1024]).unwrap();
        }

        let found = breakdown(&root, 3);
        fs::remove_dir_all(&root).unwrap();

        let names: Vec<_> = found.largest.iter().map(|f| f.path.file_name().unwrap().to_string_lossy().into_owned()).collect();
        assert_eq!(names, ["b.bin", "c.bin", "a.bin"]);
    }

    #[test]
    fn deleted_updates_every_ancestor() {
        let root = env::temp_dir().join(format!("treespace-deleted-{}", process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("a/b")).unwrap();
        fs::create_dir_all(root.join("c")).unwrap();
        fs::write(root.join("a/one.bin"), [0u8; 100]).unwrap();
        fs::write(root.join("a/b/two.bin"), [0u8; 40]).unwrap();
        fs::write(root.join("c/three.bin"), [0u8; 7]).unwrap();
        let mut tree = walk_dir(&root);
        fs::remove_dir_all(&root).unwrap();
        let find = |tree: &Dir, name: &str| tree.children.iter().find(|c| c.path.ends_with(name)).map(|c| (c.size, c.files));

        // A file in a/b: a/b, a and the root each lose 40 bytes and one file.
        let b = root.join("a/b");
        let b_allocated = tree.children.iter().find(|c| c.path.ends_with("a")).unwrap().children[0].allocated;
        assert!(tree.deleted(&b, 40, b_allocated, 1, false));
        assert_eq!((tree.size, tree.files), (107, 2));
        assert_eq!(find(&tree, "a"), Some((100, 1)));

        // The folder c itself: removed from the tree, root totals drop accordingly.
        let c = root.join("c");
        let (c_size, c_files) = find(&tree, "c").unwrap();
        assert!(tree.deleted(&c, c_size, 0, c_files, true));
        assert_eq!((tree.size, tree.files, tree.children.len()), (100, 1, 1));

        assert!(!tree.deleted(&root.join("elsewhere"), 1, 1, 1, false), "unknown paths change nothing");
        assert_eq!(tree.size, 100);
    }

    #[test]
    fn subtree_takes_out_one_folder() {
        let root = env::temp_dir().join(format!("treespace-subtree-{}", process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("a/b/c")).unwrap();
        fs::create_dir_all(root.join("z")).unwrap();
        fs::write(root.join("a/b/one.bin"), [0u8; 30]).unwrap();
        fs::write(root.join("a/b/c/two.bin"), [0u8; 12]).unwrap();
        fs::write(root.join("z/other.bin"), [0u8; 99]).unwrap();
        let (tree, again) = (walk_dir(&root), walk_dir(&root));
        fs::remove_dir_all(&root).unwrap();

        let b = subtree(tree, &root.join("a/b")).expect("a/b is in the tree");
        assert_eq!((b.path, b.size, b.files, b.children.len()), (root.join("a/b"), 42, 2, 1));
        assert!(subtree(again, &root.join("a/gone")).is_none(), "unknown folder");
    }

    #[test]
    fn scan_path_walks_a_folder_quietly() {
        let root = env::temp_dir().join(format!("treespace-scan-{}", process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("sub")).unwrap();
        fs::write(root.join("sub/a.bin"), [0u8; 10]).unwrap();

        // An administrator on Windows gets the folder cut out of the drive's MFT, so there it is asked to walk.
        let scan = scan_path(&root, cfg!(windows)).unwrap();
        fs::remove_dir_all(&root).unwrap();

        assert_eq!((scan.engine, scan.note, scan.tree.size, scan.tree.files), (Engine::Walk, None, 10, 1));
        assert_eq!(scan.engine.to_string(), "directory walk");
    }

    #[test]
    fn scan_path_notes_a_failed_mft_read() {
        let image = env::temp_dir().join(format!("treespace-not-ntfs-{}.img", process::id()));
        fs::write(&image, [0u8; 4096]).unwrap();
        let device = fs::canonicalize(&image).unwrap();

        let scan = scan_path(&image, false).unwrap();
        fs::remove_file(&image).unwrap();

        let note = scan.note.unwrap_or_default();
        assert_eq!(scan.engine, Engine::Walk);
        assert!(note.starts_with(&format!("MFT read of {} failed (", device.display())), "{note}");
        assert!(note.ends_with("); used the directory walk"), "{note}");
    }
}
