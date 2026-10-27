use std::{env, fs, path::PathBuf, process};
use treespace::{Dir, human, mft, scan_path, volume};

/// Folder names come from the disk (or a crafted image): print control characters as '?'
/// so a name cannot inject terminal escape sequences.
fn printable(name: &str) -> String {
    name.chars().map(|c| if c.is_control() { '?' } else { c }).collect()
}

fn print(dir: &Dir, total: u64, depth: usize, max_depth: usize, top: usize) {
    let pct = if total == 0 { 0.0 } else { dir.allocated as f64 * 100.0 / total as f64 };
    let name = match (depth, dir.path.file_name()) {
        (0, _) | (_, None) => dir.path.display().to_string(),
        (_, Some(name)) => name.to_string_lossy().into_owned(),
    };
    println!(
        "{:>10} {:>10} {pct:>5.1}%  {:indent$}{}",
        human(dir.allocated),
        human(dir.size),
        "",
        printable(&name),
        indent = depth * 2
    );
    if depth < max_depth {
        for child in dir.children.iter().take(top) {
            print(child, total, depth + 1, max_depth, top);
        }
    }
}

fn number(flag: &str, value: Option<String>) -> usize {
    value.and_then(|v| v.parse().ok()).unwrap_or_else(|| {
        eprintln!("{flag} needs a non-negative number");
        process::exit(2);
    })
}

fn main() {
    let mut root = PathBuf::from(".");
    let (mut depth, mut top, mut walk, mut dump) = (1, 10, false, None);
    let mut args = env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--depth" => depth = number("--depth", args.next()),
            "--top" => top = number("--top", args.next()),
            "--walk" => walk = true,
            "--dump-mft" => {
                dump = Some(args.next().map(PathBuf::from).unwrap_or_else(|| {
                    eprintln!("--dump-mft needs a file name");
                    process::exit(2);
                }))
            }
            "-h" | "--help" => {
                println!("usage: treespace [PATH] [--depth N (default 1)] [--top N (default 10)] [--walk]");
                println!("       treespace DRIVE --dump-mft FILE");
                println!("  A drive root (C:\\, as admin), an NTFS image or a raw $MFT export is read");
                println!("  through its MFT; anything else, or --walk, uses the directory walk.");
                println!("  --dump-mft saves the drive's raw $MFT to FILE for offline analysis.");
                return;
            }
            _ => root = PathBuf::from(arg),
        }
    }

    if let Some(out) = dump {
        let device = fs::canonicalize(&root).ok().and_then(|p| volume(&p)).unwrap_or_else(|| {
            eprintln!("--dump-mft needs a drive root such as C:\\");
            process::exit(2);
        });
        match mft::dump(&device, &out) {
            Ok(bytes) => println!("wrote {} of $MFT records to {}", human(bytes), out.display()),
            Err(e) => {
                eprintln!("{}: {e}", device.display());
                process::exit(1);
            }
        }
        return;
    }

    let mut scan = scan_path(&root, walk).unwrap_or_else(|e| {
        eprintln!("{}: {e}", root.display());
        process::exit(1);
    });
    if let Some(note) = &scan.note {
        eprintln!("{note}");
    }
    scan.tree.path = root;
    println!("{:>10} {:>10} {:>6}  path", "on disk", "size", "");
    print(&scan.tree, scan.tree.allocated, 0, depth, top);
    println!(
        "\n{} files, {} unreadable entries, scanned in {:.2}s ({})",
        scan.tree.files,
        scan.tree.errors,
        scan.elapsed.as_secs_f64(),
        scan.engine
    );
}
