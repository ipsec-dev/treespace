# treespace

Fast disk space analyzer for Windows: see what takes the space, drill in, clean up.

- **NTFS MFT scan** (as administrator): reads the volume's file table in one pass, about 3 s for
  1.4 million files, totals that match Windows' used space, hard links counted once.
- **Directory walk** otherwise: parallel, size on disk (cloud-only OneDrive files count as 0),
  never follows symlinks or junctions.
- **Window**: folder tree sorted by size, treemap, and per folder the **Files**, **Types**
  (space per extension) and **Largest** (100 biggest files below it) tabs.
- **Clean up**: right-click a folder or file to move it to the Recycle Bin (confirmed first).
- **Explorer**: "Scan with treespace" on folders and drives (Windows 11: under "Show more options").

## Download

Get `treespace-windows-x86_64.zip` from the [Releases](https://github.com/ipsec-dev/treespace/releases)
page. It holds two programs, no installer needed:

- `treespace-gui.exe`: the window.
- `treespace.exe`: the command line version.

Put them in a folder of your choice, for example `%LOCALAPPDATA%\Programs\treespace`.

## Use

Start `treespace-gui.exe`, pick a drive or type a folder, then **Scan**.

- **Restart as administrator** (status bar) switches to the fast MFT scan. Moving to the Recycle
  Bin is disabled while elevated, for safety.
- Tick **Explorer menu** to add "Scan with treespace" to Explorer's right-click menu (your account
  only; untick to remove). It launches the exe from where it is when you tick it.

Command line:

```
treespace [PATH] [--depth N] [--top N] [--walk]
treespace C:\ --depth 2 --top 5
treespace C:\ --dump-mft mft.bin      # save the raw $MFT for offline analysis (as administrator)
```

## Build

```
cargo build --release                     # native: Windows or Linux
cargo xwin build --release --target x86_64-pc-windows-msvc   # Windows exes from Linux/WSL
cargo test
```

`vendor/winit` is winit 0.30.13 with one fix for windows moved between screens with different
scaling; it can go once eframe ships a winit release with an equivalent fix.
