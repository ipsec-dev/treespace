//! Volume scan from the NTFS Master File Table: one sequential read of every file record
//! instead of opening each folder. Each record is one file, so hard links are counted once,
//! and NTFS's own metadata files ($MFT, $LogFile, ...) are included like on-disk usage.
//!
//! Limitations: the $MFT's own extents must all be listed in record 0 (no $ATTRIBUTE_LIST on $MFT),
//! and a record must not straddle two extents (clusters smaller than a record, i.e. 512-byte
//! clusters). Either case returns an error and the caller falls back to the directory walk.

use crate::{finish, Dir};
use rayon::prelude::*;
use std::{
    fs::File,
    io::{self, Write},
    path::{Path, PathBuf},
};

const ROOT: usize = 5;
const BADCLUS: usize = 8;
const CHUNK: u64 = 4 << 20;
const END: u64 = 0xFFFF_FFFF;
const FILE_NAME: u64 = 0x30;
const DATA: u64 = 0x80;
/// Deepest folder level build() descends to; records below it are left out and counted as errors.
/// A crafted image can chain folders as deep as it likes, and building, dropping and showing the tree
/// all recurse once per level. Real trees stay far shallower: most programs, Explorer included, stop
/// at MAX_PATH (260 characters, about 130 levels), and a 32767-character long path averages 64
/// characters per name at this depth. Building and dropping 512 levels takes under 450 KiB of stack
/// in a debug build (under 200 KiB optimized), less than half the CLI's 1 MiB main thread on Windows.
const MAX_DEPTH: usize = 512;

/// What one record contributes; extension records (`base` set) belong to their base record.
struct Piece {
    base: Option<usize>,
    /// (parent index, name); the text is only decoded where needed: directories and extension records.
    name: Option<(usize, String)>,
    dir: bool,
    size: u64,
    allocated: u64,
}

pub fn scan(device: &Path, root: PathBuf) -> io::Result<Dir> {
    let file = File::open(device)?;
    let (record, extents) = layout(&file)?;
    let mut pieces = Vec::new();
    let mut index = 0;
    read_extents(&file, &extents, |buf| {
        let first = index;
        pieces.par_extend(
            buf.par_chunks_mut(record as usize)
                .enumerate()
                .filter_map(|(i, r)| parse(r).map(|p| (first + i, p))),
        );
        index += buf.len() / record as usize;
        Ok(())
    })?;
    // Folder totals are sums of these sizes; if all of them fit in a u64, every subtotal does too.
    if pieces.iter().try_fold((0u64, 0u64), |(s, a), (_, p)| Some((s.checked_add(p.size)?, a.checked_add(p.allocated)?))).is_none() {
        return Err(invalid("file sizes overflow"));
    }

    let mut nodes: Vec<Option<Piece>> = (0..index).map(|_| None).collect();
    let mut extensions = Vec::new();
    for (i, piece) in pieces {
        match piece.base {
            None => nodes[i] = Some(piece),
            Some(base) => extensions.push((base, piece)),
        }
    }
    for (base, ext) in extensions {
        if let Some(Some(node)) = nodes.get_mut(base) {
            node.size += ext.size;
            node.allocated += ext.allocated;
            if node.name.is_none() {
                node.name = ext.name; // a crowded record can move its $FILE_NAME to an extension
            }
        }
    }
    // $BadClus:$Bad is a volume-sized placeholder mapping bad clusters, not data.
    if let Some(node) = nodes.get_mut(BADCLUS) {
        *node = None;
    }

    let mut kids: Vec<Vec<u32>> = vec![Vec::new(); index];
    for (i, node) in nodes.iter().enumerate() {
        if let Some(Piece { name: Some((parent, _)), .. }) = node
            && i != ROOT
            && let Some(list) = kids.get_mut(*parent)
        {
            list.push(i as u32);
        }
    }
    if !matches!(nodes.get(ROOT), Some(Some(p)) if p.dir) {
        return Err(invalid("no root directory record"));
    }
    let mut reached = 0;
    let mut tree = build(ROOT, root, 0, &nodes, &kids, &mut reached);
    // Records whose parent chain never reaches the root (e.g. a deleted parent) are reported, not guessed.
    let named = nodes.iter().flatten().filter(|p| p.name.is_some()).count() as u64;
    tree.errors = named.saturating_sub(1 + reached);
    Ok(tree)
}

/// Copies the raw $MFT records of `device` to `out`, for offline analysis. Returns the bytes written.
pub fn dump(device: &Path, out: &Path) -> io::Result<u64> {
    let file = File::open(device)?;
    let (_, extents) = layout(&file)?;
    let mut target = io::BufWriter::new(File::create(out)?);
    let mut written = 0;
    read_extents(&file, &extents, |buf| {
        written += buf.len() as u64;
        target.write_all(buf)
    })?;
    target.flush()?;
    Ok(written)
}

/// Record size and byte extents of the MFT in a volume, an NTFS image, or a raw $MFT export.
fn layout(file: &File) -> io::Result<(u64, Vec<(u64, u64)>)> {
    let mut boot = vec![0; 4096]; // a multiple of both 512- and 4096-byte sectors
    read_at(file, &mut boot, 0)?;
    if boot.starts_with(b"FILE") {
        // Raw $MFT export: records back to back, sized by the first record's header.
        let record = le(&boot, 0x1C, 4).unwrap();
        let len = file.metadata()?.len();
        check(record)?;
        return Ok((record, vec![(0, len - len % record)]));
    }
    if &boot[3..11] != b"NTFS    " {
        return Err(invalid("not an NTFS volume or $MFT export"));
    }
    let geometry = || invalid("unsupported NTFS geometry");
    let per_cluster = match boot[0x0D] {
        n if n > 0x80 => 1u64.checked_shl(256 - n as u32), // clusters above 64 KiB store a negative power of two
        n => Some(n as u64),
    };
    let cluster = per_cluster.and_then(|n| le(&boot, 0x0B, 2).unwrap().checked_mul(n)).ok_or_else(geometry)?;
    if cluster == 0 {
        return Err(geometry());
    }
    let record = match boot[0x40] as i8 {
        n if n < 0 => 1u64.checked_shl(n.unsigned_abs().into()),
        n => (n as u64).checked_mul(cluster),
    }
    .ok_or_else(geometry)?;
    check(record)?;

    let mut first = vec![0; record as usize];
    read_at(file, &mut first, le(&boot, 0x30, 8).unwrap().checked_mul(cluster).ok_or_else(geometry)?)?;
    fixup(&mut first).ok_or_else(|| invalid("unreadable $MFT record"))?;
    let data = attrs(&first)
        .find(|a| le(a, 0, 4) == Some(DATA) && a[8] != 0 && a[9] == 0)
        .ok_or_else(|| invalid("no $MFT data"))?;
    let extents = runs(data, cluster).ok_or_else(|| invalid("bad $MFT run list"))?;
    let mapped = extents.iter().try_fold(0u64, |total, &(_, len)| total.checked_add(len));
    if mapped != Some(le(data, 0x28, 8).unwrap_or(0)) || extents.iter().any(|&(_, len)| len % record != 0) {
        return Err(invalid("$MFT layout not supported (attribute list or tiny clusters)"));
    }
    Ok((record, extents))
}

fn check(record: u64) -> io::Result<()> {
    if record < 1024 || !CHUNK.is_multiple_of(record) {
        return Err(invalid("unsupported MFT record size"));
    }
    Ok(())
}

/// Reads `extents` in order, in CHUNK-sized whole-record buffers.
fn read_extents(file: &File, extents: &[(u64, u64)], mut each: impl FnMut(&mut [u8]) -> io::Result<()>) -> io::Result<()> {
    for &(start, len) in extents {
        let mut done = 0;
        while done < len {
            let n = (len - done).min(CHUNK);
            let mut buf = vec![0; n as usize];
            read_at(file, &mut buf, start + done)?;
            each(&mut buf)?;
            done += n;
        }
    }
    Ok(())
}

fn build(i: usize, path: PathBuf, depth: usize, nodes: &[Option<Piece>], kids: &[Vec<u32>], reached: &mut u64) -> Dir {
    let mut dir = Dir { path, size: 0, allocated: 0, files: 0, errors: 0, children: Vec::new() };
    // The records below a folder at MAX_DEPTH stay unreached, so scan() counts them as errors.
    let below = if depth < MAX_DEPTH { kids[i].as_slice() } else { &[] };
    for &k in below {
        let Some(node) = &nodes[k as usize] else { continue };
        *reached += 1;
        if node.dir {
            let name = node.name.as_ref().map_or("", |(_, n)| n.as_str());
            let child = build(k as usize, dir.path.join(component(name)), depth + 1, nodes, kids, reached);
            dir.children.push(child);
        } else {
            dir.size += node.size;
            dir.allocated += node.allocated;
            dir.files += 1;
        }
    }
    finish(&mut dir);
    dir
}

/// A name read from the volume as exactly one path component. Names come from untrusted
/// data (any image or export can be crafted): a separator, drive prefix or `..` would let
/// `join` escape or replace the scanned path, e.g. into a program "Open in Explorer" then runs.
fn component(name: &str) -> String {
    match name {
        "" | "." | ".." => "_".to_string(),
        _ => name.replace(['\\', '/', ':', '\0'], "_"),
    }
}

/// Parses one in-use MFT record, applying its update-sequence fixups in place.
fn parse(rec: &mut [u8]) -> Option<Piece> {
    fixup(rec)?;
    let flags = le(rec, 0x16, 2)?;
    if flags & 1 == 0 {
        return None; // free record
    }
    let dir = flags & 2 != 0;
    let base = match le(rec, 0x20, 6)? as usize {
        0 => None,
        b => Some(b),
    };
    let keep_text = dir || base.is_some();
    let mut piece = Piece { base, name: None, dir, size: 0, allocated: 0 };
    for attr in attrs(rec) {
        let resident = attr[8] == 0;
        match le(attr, 0, 4)? {
            FILE_NAME if resident && piece.name.is_none() => {
                let value = attr.get(le(attr, 0x14, 2)? as usize..)?;
                // Namespace 2 is a DOS 8.3 alias of a long name; the first other name wins for hard links.
                if *value.get(0x41)? != 2 {
                    let chars = *value.get(0x40)? as usize;
                    let units: Vec<u16> = if keep_text {
                        value.get(0x42..0x42 + 2 * chars)?.as_chunks::<2>().0.iter().map(|c| u16::from_le_bytes(*c)).collect()
                    } else {
                        Vec::new()
                    };
                    piece.name = Some((le(value, 0, 6)? as usize, String::from_utf16_lossy(&units)));
                }
            }
            DATA if resident => {
                let n = le(attr, 0x10, 4)?;
                if attr[9] == 0 {
                    piece.size = piece.size.checked_add(n)?;
                }
                piece.allocated = piece.allocated.checked_add(n.next_multiple_of(8))?;
            }
            // Only the first piece of a split attribute (lowest VCN 0) carries the sizes.
            DATA if le(attr, 0x10, 8)? == 0 => {
                if attr[9] == 0 {
                    piece.size = piece.size.checked_add(le(attr, 0x30, 8)?)?;
                }
                // Compressed or sparse streams record their real on-disk size separately.
                let on_disk = if le(attr, 0x0C, 2)? & 0x8001 != 0 { le(attr, 0x40, 8)? } else { le(attr, 0x28, 8)? };
                piece.allocated = piece.allocated.checked_add(on_disk)?;
            }
            _ => {}
        }
    }
    Some(piece)
}

/// Restores the last two bytes of every 512-byte block from the update sequence array.
/// Fails on a missing signature or a torn write.
fn fixup(rec: &mut [u8]) -> Option<()> {
    if !rec.starts_with(b"FILE") {
        return None;
    }
    let usa = le(rec, 4, 2)? as usize;
    let count = le(rec, 6, 2)? as usize;
    let usn = [*rec.get(usa)?, *rec.get(usa + 1)?];
    for i in 1..count {
        let end = i * 512;
        let saved = [*rec.get(usa + 2 * i)?, *rec.get(usa + 2 * i + 1)?];
        let tail = rec.get_mut(end - 2..end)?;
        if tail != usn {
            return None;
        }
        tail.copy_from_slice(&saved);
    }
    Some(())
}

/// The attribute records of a fixed-up MFT record.
fn attrs(rec: &[u8]) -> impl Iterator<Item = &[u8]> {
    let mut at = le(rec, 0x14, 2).map(|v| v as usize);
    std::iter::from_fn(move || {
        let start = at?;
        let len = le(rec, start + 4, 4)? as usize;
        if le(rec, start, 4)? == END || len < 0x18 {
            at = None;
            return None;
        }
        at = start.checked_add(len);
        rec.get(start..at?)
    })
}

/// Decodes a non-resident attribute's run list into (byte offset, byte length) extents.
fn runs(attr: &[u8], cluster: u64) -> Option<Vec<(u64, u64)>> {
    let mut at = le(attr, 0x20, 2)? as usize;
    let (mut lcn, mut out) = (0i64, Vec::new());
    loop {
        let head = *attr.get(at)?;
        if head == 0 {
            return Some(out);
        }
        let (len_bytes, off_bytes) = ((head & 0xF) as usize, (head >> 4) as usize);
        if off_bytes == 0 || off_bytes > 8 || len_bytes > 8 {
            return None; // a sparse run cannot appear in $MFT
        }
        let len = le(attr, at + 1, len_bytes)?;
        let shift = 64 - 8 * off_bytes as u32;
        lcn = lcn.checked_add(((le(attr, at + 1 + len_bytes, off_bytes)? << shift) as i64) >> shift)?; // signed delta
        let start = u64::try_from(lcn).ok()?.checked_mul(cluster)?;
        let bytes = len.checked_mul(cluster)?;
        start.checked_add(bytes)?; // read_extents() reads up to the extent's end
        out.push((start, bytes));
        at += 1 + len_bytes + off_bytes;
    }
}

/// Little-endian unsigned integer of `n` bytes at `at`, or None past the end.
fn le(b: &[u8], at: usize, n: usize) -> Option<u64> {
    Some(b.get(at..at + n)?.iter().rev().fold(0, |v, &x| v << 8 | x as u64))
}

fn invalid(what: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, what.to_string())
}

#[cfg(unix)]
fn read_at(file: &File, buf: &mut [u8], offset: u64) -> io::Result<()> {
    std::os::unix::fs::FileExt::read_exact_at(file, buf, offset)
}

#[cfg(windows)]
fn read_at(file: &File, mut buf: &mut [u8], mut offset: u64) -> io::Result<()> {
    use std::os::windows::fs::FileExt;
    while !buf.is_empty() {
        let n = file.seek_read(buf, offset)?;
        if n == 0 {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        buf = &mut buf[n..];
        offset += n as u64;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{env, fs, process, thread};

    fn put(b: &mut [u8], at: usize, value: u64, n: usize) {
        b[at..at + n].copy_from_slice(&value.to_le_bytes()[..n]);
    }

    /// A 2-block record with one $FILE_NAME per (parent index, name) and a sparse $DATA stream of two
    /// runs (0x9000 bytes, 0x2000 on disk). `flags` 1 is a file, 3 a folder, 0 a free record; a nonzero
    /// `base` makes it an extension record of that one.
    fn record(flags: u64, base: u64, names: &[(usize, &str)]) -> Vec<u8> {
        let mut r = vec![0; 1024];
        r[..4].copy_from_slice(b"FILE");
        put(&mut r, 0x04, 0x30, 2); // update sequence array offset
        put(&mut r, 0x06, 3, 2); // usn + one entry per 512-byte block
        put(&mut r, 0x14, 0x38, 2); // first attribute
        put(&mut r, 0x16, flags, 2);
        put(&mut r, 0x1C, 1024, 4); // record size, read from the first record of a raw export
        put(&mut r, 0x20, base, 6);
        put(&mut r, 0x30, 0x0001, 2); // usn
        put(&mut r, 0x32, 0xBBAA, 2); // real bytes of block 0's tail
        put(&mut r, 0x34, 0xDDCC, 2); // real bytes of block 1's tail
        put(&mut r, 510, 0x0001, 2);
        put(&mut r, 1022, 0x0001, 2);

        let mut a = 0x38;
        for &(parent, name) in names {
            let units: Vec<u16> = name.encode_utf16().collect();
            let len = (0x18 + 0x42 + 2 * units.len()).next_multiple_of(8);
            put(&mut r, a, FILE_NAME, 4); // resident
            put(&mut r, a + 4, len as u64, 4);
            put(&mut r, a + 0x10, 0x42 + 2 * units.len() as u64, 4);
            put(&mut r, a + 0x14, 0x18, 2);
            let v = a + 0x18;
            put(&mut r, v, parent as u64 | 5 << 48, 8); // parent reference: index, sequence 5
            r[v + 0x40] = units.len() as u8;
            r[v + 0x41] = 1; // Win32 namespace
            for (i, &c) in units.iter().enumerate() {
                put(&mut r, v + 0x42 + 2 * i, c as u64, 2);
            }
            a += len;
        }

        let d = a; // $DATA, non-resident, sparse
        put(&mut r, d, DATA, 4);
        put(&mut r, d + 4, 0x50, 4);
        r[d + 8] = 1;
        put(&mut r, d + 0x0C, 0x8000, 2);
        put(&mut r, d + 0x18, 0x17, 8);
        put(&mut r, d + 0x20, 0x48, 2);
        put(&mut r, d + 0x28, 0x18000, 8); // allocated
        put(&mut r, d + 0x30, 0x9000, 8); // logical size
        put(&mut r, d + 0x40, 0x2000, 8); // really on disk
        // Runs: 0x10 clusters at LCN 0x20, then 8 clusters at 0x20 - 0x10 = 0x10.
        r[d + 0x48..d + 0x4F].copy_from_slice(&[0x11, 0x10, 0x20, 0x11, 0x08, 0xF0, 0x00]);
        put(&mut r, d + 0x50, END, 4);
        r
    }

    #[test]
    fn parses_record_and_runs() {
        let mut r = record(1, 0, &[(ROOT, "a.txt")]);
        let piece = parse(&mut r).expect("record parsed");
        assert_eq!(&r[510..512], &[0xAA, 0xBB], "fixup restored");
        assert_eq!((piece.base, piece.dir, piece.size, piece.allocated), (None, false, 0x9000, 0x2000));
        assert_eq!(piece.name.map(|(parent, _)| parent), Some(ROOT));
        assert_eq!(runs(&r[0xA0..0xF0], 4096), Some(vec![(0x20 * 4096, 0x10 * 4096), (0x10 * 4096, 8 * 4096)]));

        let mut ext = record(1, 42, &[(ROOT, "a.txt")]); // an extension of record 42 keeps its name text
        let piece = parse(&mut ext).expect("extension parsed");
        assert_eq!((piece.base, piece.name.map(|(_, n)| n)), (Some(42), Some("a.txt".to_string())));

        let mut torn = record(1, 0, &[(ROOT, "a.txt")]);
        torn[1022] = 0x02;
        assert!(parse(&mut torn).is_none(), "torn write rejected");
    }

    #[test]
    fn crafted_names_stay_one_component() {
        let root = PathBuf::from("/scan");
        for name in [r"C:\Windows\evil.exe", "../up", "a/b", "..", ".", "", "x\0y"] {
            let joined = root.join(component(name));
            assert_eq!(joined.parent(), Some(root.as_path()), "{name:?} escaped as {joined:?}");
        }
        assert_eq!(component("Program Files"), "Program Files");
    }

    #[test]
    fn bad_input_is_an_error_not_a_panic() {
        let path = env::temp_dir().join(format!("treespace-mft-bad-{}", process::id()));
        let layout_of = |bytes: &[u8]| {
            fs::write(&path, bytes).unwrap();
            let file = File::open(&path).unwrap();
            fs::remove_file(&path).unwrap(); // gone even when a check below panics or fails
            layout(&file)
        };
        // A boot sector (512-byte sectors, 8 per cluster, 1 KiB records, $MFT at cluster 1) and the $MFT's record.
        let mut image = vec![0; 8192];
        image[3..11].copy_from_slice(b"NTFS    ");
        put(&mut image, 0x0B, 512, 2);
        image[0x0D] = 8;
        put(&mut image, 0x30, 1, 8);
        image[0x40] = 0xF6; // -10: 2^10-byte records
        // A folder record, so the sweep below also decodes name text; layout() ignores the flags.
        image[4096..5120].copy_from_slice(&record(3, 0, &[(ROOT, "$MFT")]));
        assert_eq!(layout_of(&image).unwrap(), (1024, vec![(0x20 * 4096, 0x10 * 4096), (0x10 * 4096, 8 * 4096)]));
        // Two runs of 2^51 clusters (2^63 bytes each) whose total overflows; the run list is moved over
        // the size fields layout() does not read, for room.
        let mut two_runs = image.clone();
        put(&mut two_runs, 4096 + 0xA0 + 0x20, 0x30, 2);
        two_runs[4096 + 0xD0..4096 + 0xE3].copy_from_slice(&[0x17, 0, 0, 0, 0, 0, 0, 0x08, 0x01, 0x17, 0, 0, 0, 0, 0, 0, 0x08, 0x01, 0]);
        assert!(layout_of(&two_runs).is_err(), "$MFT size overflows");

        // 2^127 and 2^64 sectors per cluster, 2^63 * 512-byte clusters, 2^128- and 2^64-byte records, a $MFT past 2^64 bytes.
        for (at, value) in [(0x0D, 0x81), (0x0D, 0xC0), (0x0D, 0xC1), (0x40, 0x80), (0x40, 0xC0), (0x37, 0xFF)] {
            let mut bad = image.clone();
            bad[at] = value;
            assert!(layout_of(&bad).is_err(), "byte {at:#x} = {value:#x}");
        }
        for len in [0, 100, 4095, 5000] {
            assert!(layout_of(&image[..len]).is_err(), "truncated to {len} bytes");
        }
        let r = record(1, 0, &[(ROOT, "a.txt")]);
        for len in 0..r.len() {
            let _ = parse(&mut r[..len].to_vec());
            let _ = runs(&r[0xA0..(0xA0 + len).min(0xF0)], 4096);
        }
        let mut huge = record(1, 0, &[(ROOT, "a.txt")]); // u64::MAX bytes, then a resident 8-byte $DATA
        put(&mut huge, 0xA0 + 0x30, u64::MAX, 8);
        put(&mut huge, 0xF0, DATA, 4);
        put(&mut huge, 0xF0 + 4, 0x20, 4);
        put(&mut huge, 0xF0 + 0x10, 8, 4);
        put(&mut huge, 0xF0 + 0x14, 0x18, 2);
        put(&mut huge, 0x110, END, 4);
        assert!(parse(&mut huge).is_none(), "file size overflows");

        let run_list = |list: &[u8], cluster| {
            let mut attr = vec![0; 0x40];
            put(&mut attr, 0x20, 0x40, 2);
            attr.extend(list);
            attr.push(0);
            runs(&attr, cluster)
        };
        let far = [0x81, 0x01, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0x7F]; // 1 cluster at LCN 2^63 - 1
        assert_eq!(run_list(&far, 1), Some(vec![(i64::MAX as u64, 1)]));
        assert_eq!(run_list(&far, 4096), None, "byte offset overflows");
        assert_eq!(run_list(&[far, far].concat(), 1), None, "LCN overflows");
        assert_eq!(run_list(&[0x11, 0x01, 0xFF], 4096), None, "LCN -1");
        assert_eq!(run_list(&[0x18, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0x01], 4096), None, "length overflows");
        assert_eq!(run_list(&[0x81, 0x02, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0x7F], 2), None, "extent end overflows");

        // Deterministic xorshift sweep over geometry and record bytes, through layout(), parse() and runs().
        let mut state = 0x2545_F491_4F6C_DD1Du64;
        let mut next = move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        for _ in 0..3000 {
            let mut bad = image.clone();
            for _ in 0..1 + next() % 4 {
                let at = if next() % 2 == 0 { next() as usize % 0x50 } else { 4096 + next() as usize % 0x110 };
                bad[at] = next() as u8;
            }
            let _ = layout_of(&bad);
            let mut r = bad[4096..5120].to_vec();
            let _ = parse(&mut r);
            for attr in attrs(&r) {
                let _ = runs(attr, next() >> (next() % 64));
            }
        }
    }

    #[test]
    fn scans_a_raw_mft_export() {
        let mut export = vec![0; 12 * 1024];
        let mut set = |i: usize, r: Vec<u8>| export[i * 1024..(i + 1) * 1024].copy_from_slice(&r);
        set(0, record(0, 0, &[])); // free; a raw export is detected and sized by its first record
        set(ROOT, record(3, 0, &[(ROOT, ".")]));
        set(6, record(3, 0, &[(ROOT, "sub")]));
        set(7, record(1, 0, &[(6, "inner.bin")]));
        set(9, record(1, 0, &[(ROOT, "linked.bin"), (6, "link.bin")])); // two hard links, counted once
        set(10, record(1, 9, &[])); // extension record: its stream adds to record 9
        set(11, record(1, 0, &[(0, "orphan.bin")])); // its parent (the free record) is gone
        let path = env::temp_dir().join(format!("treespace-mft-scan-{}", process::id()));
        fs::write(&path, &export).unwrap();

        let root = PathBuf::from("/volume");
        let tree = scan(&path, root.clone());
        fs::remove_file(&path).unwrap();
        let tree = tree.unwrap();

        assert_eq!((tree.size, tree.allocated, tree.files, tree.errors), (3 * 0x9000, 3 * 0x2000, 2, 1));
        assert_eq!(tree.children.len(), 1);
        let sub = &tree.children[0];
        assert_eq!((&sub.path, sub.size, sub.files, sub.children.len()), (&root.join("sub"), 0x9000, 1, 0));
    }

    #[test]
    fn file_sizes_that_overflow_a_total_are_an_error() {
        let mut export = vec![0; 8 * 1024];
        let mut set = |i: usize, r: Vec<u8>| export[i * 1024..(i + 1) * 1024].copy_from_slice(&r);
        set(0, record(0, 0, &[]));
        set(ROOT, record(3, 0, &[(ROOT, ".")]));
        let mut big = record(1, 0, &[(ROOT, "a.txt")]); // 2^63 bytes: two of them overflow the root's total
        put(&mut big, 0xA0 + 0x30, 1 << 63, 8);
        set(6, big.clone());
        set(7, big);
        let path = env::temp_dir().join(format!("treespace-mft-overflow-{}", process::id()));
        fs::write(&path, &export).unwrap();

        let tree = scan(&path, PathBuf::from("/volume"));
        fs::remove_file(&path).unwrap();
        assert_eq!(tree.err().map(|e| e.to_string()).as_deref(), Some("file sizes overflow"));
    }

    #[test]
    fn depth_cap_stops_a_crafted_folder_chain() {
        // 100k folders each inside the previous one, built on the 1 MiB stack the CLI scans on under Windows.
        let n = 100_000;
        let folder = |i: usize| Piece { base: None, name: Some((i - 1, "d".to_string())), dir: true, size: 0, allocated: 0 };
        let nodes: Vec<Option<Piece>> = (0..ROOT + n).map(|i| (i >= ROOT).then(|| folder(i))).collect();
        let kids: Vec<Vec<u32>> = (0..ROOT + n).map(|i| if i >= ROOT && i + 1 < ROOT + n { vec![i as u32 + 1] } else { Vec::new() }).collect();
        let (depth, reached) = thread::Builder::new()
            .stack_size(1 << 20)
            .spawn(move || {
                let mut reached = 0;
                let tree = build(ROOT, PathBuf::from("/"), 0, &nodes, &kids, &mut reached);
                let (mut depth, mut dir) = (0, &tree);
                while let Some(child) = dir.children.first() {
                    (depth, dir) = (depth + 1, child);
                }
                (depth, reached) // the tree is dropped here, on the same stack
            })
            .unwrap()
            .join()
            .unwrap();
        assert_eq!(depth, MAX_DEPTH);
        // scan() reports every named record but the root that build() did not reach as an error.
        let errors = (n as u64).saturating_sub(1 + reached);
        assert_eq!(errors, (n - 1 - MAX_DEPTH) as u64);
    }
}
