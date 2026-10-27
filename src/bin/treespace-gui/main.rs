#![windows_subsystem = "windows"] // no console window behind the GUI

use eframe::egui::{self, Align, Align2, Color32, FontId, Layout, Rect, Sense, Stroke, StrokeKind, ecolor::Hsva, pos2, vec2};
use egui_extras::{Column, Table, TableBuilder};
use platform::{ADMIN_DELETE, elevated, recycle};
use std::{
    cmp::Reverse,
    collections::HashSet,
    io,
    path::{Path, PathBuf},
    process::Command,
    sync::mpsc,
    thread,
};
use treespace::{Breakdown, Dir, FileInfo, LargeFile, Scan, TypeStat, breakdown, files, human, scan_path};

mod platform;

fn main() -> eframe::Result {
    let first = std::env::args().nth(1);
    #[cfg(windows)]
    match first.as_deref() {
        Some("--install-menu") => std::process::exit(if platform::explorer_menu::install() { 0 } else { 1 }),
        Some("--uninstall-menu") => std::process::exit(if platform::explorer_menu::uninstall() { 0 } else { 1 }),
        _ => {}
    }
    // A folder given on the command line ("Scan with treespace" in Explorer) is scanned right away.
    let start = first.map(explorer_arg);
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default().with_title("treespace").with_inner_size([1280.0, 780.0]),
        ..Default::default()
    };
    eframe::run_native(
        "treespace",
        options,
        Box::new(move |cc| {
            let mut app = App {
                path: start.clone().unwrap_or_else(|| drives().into_iter().next().unwrap_or_default()),
                drives: drives(),
                #[cfg(windows)]
                menu_installed: platform::explorer_menu::installed(),
                ..Default::default()
            };
            if start.is_some() {
                app.start(&cc.egui_ctx);
            }
            Ok(Box::new(app))
        }),
    )
}

#[derive(Default, PartialEq)]
enum Tab {
    #[default]
    Treemap,
    Files,
    Types,
    Largest,
}

#[derive(Default)]
struct App {
    path: String,
    drives: Vec<String>,
    walk: bool,
    scan: Option<Scan>,
    pending: Option<Job<io::Result<Scan>>>,
    error: Option<String>,
    expanded: HashSet<PathBuf>,
    selected: Option<PathBuf>,
    scroll_to_selected: bool,
    /// Confirmation of an action whose effect is outside this window (the Explorer menu).
    notice: Option<String>,
    tab: Tab,
    /// Files of the folder last shown in the Files tab, and one being read.
    files_cache: Option<(PathBuf, Listing)>,
    files_pending: Option<(PathBuf, Job<Listing>)>,
    /// Types and Largest data of the folder last shown in those tabs, and one being computed.
    breakdown: Option<(PathBuf, Breakdown)>,
    breakdown_pending: Option<(PathBuf, Job<Breakdown>)>,
    /// A deletion waiting for the user's confirmation, and one in progress.
    confirm: Option<Deletion>,
    deleting: Option<(Deletion, Job<Result<(), String>>)>,
    #[cfg(windows)]
    menu_installed: bool,
}

/// A folder's files, and how many entries were unreadable (what `files` returns).
type Listing = (Vec<FileInfo>, u64);

/// Work running on its own thread; the window repaints once it is done.
struct Job<T>(mpsc::Receiver<T>);

enum Poll<T> {
    Running,
    Done(T),
    /// The thread ended without a result (it panicked).
    Died,
}

impl<T: Send + 'static> Job<T> {
    fn spawn(ctx: &egui::Context, work: impl FnOnce() -> T + Send + 'static) -> Self {
        let (tx, rx) = mpsc::channel();
        let ctx = ctx.clone();
        thread::spawn(move || {
            let _ = tx.send(work());
            ctx.request_repaint();
        });
        Job(rx)
    }

    fn poll(&self) -> Poll<T> {
        match self.0.try_recv() {
            Ok(result) => Poll::Done(result),
            Err(mpsc::TryRecvError::Empty) => Poll::Running,
            Err(mpsc::TryRecvError::Disconnected) => Poll::Died,
        }
    }
}

/// Explorer passes the folder as "%V". For a drive root that is `"C:\"`, whose final backslash
/// escapes the closing quote, so the argument arrives as `C:"`: turn that back into `C:\`.
fn explorer_arg(arg: String) -> String {
    match arg.strip_suffix('"') {
        Some(path) => format!("{path}\\"),
        None => arg,
    }
}

/// Existing drive roots on Windows, `/` elsewhere.
fn drives() -> Vec<String> {
    if cfg!(windows) {
        (b'A'..=b'Z').map(|d| format!("{}:\\", d as char)).filter(|p| Path::new(p).exists()).collect()
    } else {
        vec!["/".to_string()]
    }
}

/// A path without the \\?\ prefix used internally for long paths.
fn display(path: &Path) -> String {
    let s = path.display().to_string();
    if let Some(rest) = s.strip_prefix(r"\\?\UNC\") {
        format!(r"\\{rest}")
    } else {
        s.strip_prefix(r"\\?\").map(str::to_string).unwrap_or(s)
    }
}

fn name(dir: &Dir, depth: usize) -> String {
    match dir.path.file_name() {
        Some(name) if depth > 0 => name.to_string_lossy().into_owned(),
        _ => display(&dir.path),
    }
}

/// The directories from the root down to `target`, root first.
fn chain<'a>(root: &'a Dir, target: &Path) -> Vec<&'a Dir> {
    let mut out = vec![root];
    let mut dir = root;
    while dir.path != target {
        match dir.children.iter().find(|c| target.starts_with(&c.path)) {
            Some(child) => {
                out.push(child);
                dir = child;
            }
            None => break,
        }
    }
    out
}

/// Rows currently shown in the tree: (depth, directory, parent's on-disk size).
fn visible<'a>(dir: &'a Dir, depth: usize, parent: u64, open: &HashSet<PathBuf>, out: &mut Vec<(usize, &'a Dir, u64)>) {
    out.push((depth, dir, parent));
    if open.contains(&dir.path) {
        for child in &dir.children {
            visible(child, depth + 1, dir.allocated, open, out);
        }
    }
}

/// The argument to give the file manager for `path`, if it names an existing folder.
/// The check runs on that exact string: without the \\?\ prefix Windows normalizes paths
/// (e.g. drops trailing dots), so a folder `evil.exe.` would otherwise pass as a folder
/// and reach explorer.exe as `evil.exe`, a file it would run. A leading '/' would be a switch.
fn openable(path: &Path) -> Option<String> {
    let arg = display(path);
    let target = Path::new(&arg);
    (target.is_absolute() && target.is_dir()).then_some(arg)
}

/// Something the user asked to move to the Recycle Bin, and how to update the tree afterwards.
#[derive(Clone)]
struct Deletion {
    path: PathBuf,
    /// The tree node to update: the folder itself, or the folder holding the file.
    node: PathBuf,
    folder: bool,
    size: u64,
    allocated: u64,
    files: u64,
}

impl Deletion {
    fn folder(dir: &Dir) -> Self {
        Deletion { path: dir.path.clone(), node: dir.path.clone(), folder: true, size: dir.size, allocated: dir.allocated, files: dir.files }
    }

    fn large(file: &LargeFile) -> Self {
        let folder = file.path.parent().unwrap_or(&file.path).to_path_buf();
        Deletion { path: file.path.clone(), node: folder, folder: false, size: file.size, allocated: file.allocated, files: 1 }
    }

    fn file(folder: &Path, file: &FileInfo) -> Self {
        // `file.name` comes from the folder listing, so it is a single component.
        Deletion { path: folder.join(&file.name), node: folder.to_path_buf(), folder: false, size: file.size, allocated: file.allocated, files: 1 }
    }
}

/// The "Move to Recycle Bin…" context-menu entry, greyed out with the reason when elevated.
fn recycle_menu_item(ui: &mut egui::Ui) -> bool {
    ui.add_enabled(!elevated(), egui::Button::new("Move to Recycle Bin…")).on_disabled_hover_text(ADMIN_DELETE).clicked()
}

/// Opens an existing folder; anything else (a path inside a scanned image, a file) is ignored.
fn open_in_file_manager(path: &Path) {
    if let Some(arg) = openable(path) {
        let program = if cfg!(windows) { "explorer" } else { "xdg-open" };
        let _ = Command::new(program).arg(arg).spawn();
    }
}

fn right_aligned(ui: &mut egui::Ui, text: String) {
    ui.with_layout(Layout::right_to_left(Align::Center), |ui| ui.label(text));
}

fn bar(ui: &mut egui::Ui, part: u64, whole: u64) {
    let fraction = if whole == 0 { 0.0 } else { part as f32 / whole as f32 };
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), 14.0), Sense::hover());
    let painter = ui.painter();
    painter.rect_filled(rect, 2.0, ui.visuals().extreme_bg_color);
    painter.rect_filled(Rect::from_min_size(rect.min, vec2(rect.width() * fraction, rect.height())), 2.0, Color32::from_rgb(70, 130, 200));
    painter.text(rect.center(), Align2::CENTER_CENTER, format!("{:.1} %", fraction * 100.0), FontId::proportional(11.0), ui.visuals().strong_text_color());
}

impl App {
    fn start(&mut self, ctx: &egui::Context) {
        let (path, walk) = (PathBuf::from(self.path.trim()), self.walk);
        self.pending = Some(Job::spawn(ctx, move || scan_path(&path, walk)));
        self.error = None;
        self.notice = None;
        self.files_cache = None; // files may have changed since the last look
        self.files_pending = None;
        self.breakdown = None;
        self.breakdown_pending = None;
    }

    /// Asks before moving anything to the Recycle Bin; Cancel has the focus, Escape cancels.
    fn confirm_deletion(&mut self, ctx: &egui::Context) {
        let Some(deletion) = self.confirm.clone() else { return };
        let mut decision = None;
        let modal = egui::Modal::new(egui::Id::unique("confirm-delete")).show(ctx, |ui| {
            ui.heading(if deletion.folder { "Move this folder to the Recycle Bin?" } else { "Move this file to the Recycle Bin?" });
            ui.add_space(6.0);
            ui.label(egui::RichText::new(display(&deletion.path)).strong());
            let contents = if deletion.folder { format!(" · {} files", deletion.files) } else { String::new() };
            ui.label(format!("{} on disk{contents}", human(deletion.allocated)));
            ui.add_space(10.0);
            ui.horizontal(|ui| {
                if ui.button("Move to Recycle Bin").clicked() {
                    decision = Some(true);
                }
                let cancel = ui.button("Cancel");
                if ui.memory(|m| m.focused()).is_none() {
                    cancel.request_focus();
                }
                if cancel.clicked() {
                    decision = Some(false);
                }
            });
        });
        if modal.should_close() {
            decision.get_or_insert(false);
        }
        match decision {
            Some(true) => {
                self.confirm = None;
                let path = deletion.path.clone();
                self.deleting = Some((deletion, Job::spawn(ctx, move || recycle(&path))));
            }
            Some(false) => self.confirm = None,
            None => {}
        }
    }

    /// Applies a finished deletion: the tree loses the item's sizes without a rescan.
    fn finish_deletion(&mut self) {
        let finished = self.deleting.as_ref().and_then(|(deletion, job)| match job.poll() {
            Poll::Done(result) => Some((deletion.clone(), result)),
            Poll::Died => Some((deletion.clone(), Err("The deletion stopped unexpectedly".to_string()))),
            Poll::Running => None,
        });
        let Some((d, result)) = finished else { return };
        self.deleting = None;
        match result {
            Ok(()) => {
                if let Some(scan) = &mut self.scan {
                    scan.tree.deleted(&d.node, d.size, d.allocated, d.files, d.folder);
                }
                if d.folder && self.selected.as_ref().is_some_and(|s| s.starts_with(&d.path)) {
                    self.selected = d.path.parent().map(Path::to_path_buf);
                }
                self.files_cache = None;
                self.files_pending = None;
                self.breakdown = None;
                self.breakdown_pending = None;
                self.notice = Some(format!("Moved {} ({}) to the Recycle Bin", display(&d.path), human(d.allocated)));
            }
            Err(e) => self.error = Some(e),
        }
    }

    /// Selects `path` and expands every folder above it so its row is visible.
    fn select(&mut self, path: PathBuf) {
        if let Some(scan) = &self.scan {
            let chain = chain(&scan.tree, &path);
            for dir in &chain[..chain.len() - 1] {
                self.expanded.insert(dir.path.clone());
            }
        }
        self.selected = Some(path);
        self.scroll_to_selected = true;
    }

    fn toolbar(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            let busy = self.pending.is_some();
            let mut go = false;
            for drive in self.drives.clone() {
                if ui.add_enabled(!busy, egui::Button::new(&drive)).clicked() {
                    self.path = drive;
                    go = true;
                }
            }
            ui.separator();
            let edit = ui.add(egui::TextEdit::singleline(&mut self.path).hint_text("Folder to scan").desired_width(380.0));
            go |= edit.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
            go |= ui.add_enabled(!busy, egui::Button::new("Scan")).clicked();
            ui.checkbox(&mut self.walk, "Directory walk")
                .on_hover_text("Open every folder instead of reading the NTFS file table (which needs administrator rights)");
            #[cfg(windows)]
            {
                let mut wanted = self.menu_installed;
                let toggle = ui.checkbox(&mut wanted, "Explorer menu").on_hover_text(
                    "Adds \"Scan with treespace\" to the right-click menu of folders and drives, for your account \
                     (on Windows 11 under \"Show more options\"). It launches this exe from where it is now.",
                );
                if toggle.changed() {
                    let ok = if wanted { platform::explorer_menu::install() } else { platform::explorer_menu::uninstall() };
                    self.menu_installed = platform::explorer_menu::installed();
                    if !ok {
                        self.error = Some("Could not update the Explorer menu entry in the registry".to_string());
                    } else if self.menu_installed {
                        self.notice = Some(
                            "Added \"Scan with treespace\" to Explorer: right-click a folder or drive, \
                             then \"Show more options\" on Windows 11"
                                .to_string(),
                        );
                    } else {
                        self.notice = Some("Removed \"Scan with treespace\" from the Explorer menu".to_string());
                    }
                }
            }
            if go && !busy {
                self.start(ui.ctx());
            }
            if busy {
                ui.spinner();
                ui.label("Scanning…");
            }
        });
    }

    fn status(&self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            if let Some(notice) = &self.notice {
                ui.colored_label(Color32::from_rgb(70, 170, 90), notice);
                ui.separator();
            }
            if let Some((deletion, _)) = &self.deleting {
                ui.spinner();
                ui.label(format!("Moving {} to the Recycle Bin…", display(&deletion.path)));
                ui.separator();
            }
            if let Some(error) = &self.error {
                ui.colored_label(ui.visuals().error_fg_color, error);
            } else if let Some(scan) = &self.scan {
                let tree = &scan.tree;
                ui.label(format!(
                    "{} on disk · {} logical · {} files · {} unreadable · {:.2} s ({})",
                    human(tree.allocated),
                    human(tree.size),
                    tree.files,
                    tree.errors,
                    scan.elapsed.as_secs_f64(),
                    scan.engine
                ));
                if let Some(note) = &scan.note {
                    ui.separator();
                    ui.colored_label(ui.visuals().warn_fg_color, note);
                    #[cfg(windows)]
                    if ui
                        .button("Restart as administrator")
                        .on_hover_text("Reopens treespace elevated (Windows asks first) to read the NTFS file table directly")
                        .clicked()
                        && platform::relaunch_as_admin(self.path.trim())
                    {
                        ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
                    }
                }
            } else {
                ui.label("Pick a drive or type a folder, then Scan.");
            }
        });
    }

    fn tree(&mut self, ui: &mut egui::Ui) {
        let Some(scan) = &self.scan else {
            ui.centered_and_justified(|ui| ui.label("No scan yet."));
            return;
        };
        let mut rows = Vec::new();
        visible(&scan.tree, 0, scan.tree.allocated, &self.expanded, &mut rows);
        let (mut toggle, mut select, mut delete) = (None, None, None);
        let deleting = self.deleting.is_some();
        let mut table = TableBuilder::new(ui).sense(Sense::click());
        if self.scroll_to_selected {
            if let Some(index) = rows.iter().position(|(_, dir, _)| Some(&dir.path) == self.selected.as_ref()) {
                table = table.scroll_to_row(index, Some(Align::Center));
            }
            self.scroll_to_selected = false;
        }
        let columns = [
            (Column::remainder().at_least(220.0).clip(true), "Name"),
            (Column::initial(90.0).at_least(70.0), "On disk"),
            (Column::initial(90.0).at_least(70.0), "Size"),
            (Column::initial(120.0).at_least(80.0), "% of parent"),
            (Column::initial(80.0).at_least(60.0), "Files"),
        ];
        with_columns(table, &columns)
            .body(|body| {
                body.rows(20.0, rows.len(), |mut row| {
                    let (depth, dir, parent) = rows[row.index()];
                    row.set_selected(self.selected.as_ref() == Some(&dir.path));
                    row.col(|ui| {
                        ui.add_space(depth as f32 * 16.0);
                        if dir.children.is_empty() {
                            ui.add_space(18.0);
                        } else {
                            // Painted like egui's collapsing headers: the default fonts have no triangle glyphs.
                            let (_, arrow) = ui.allocate_exact_size(vec2(18.0, 18.0), Sense::click());
                            let openness = if self.expanded.contains(&dir.path) { 1.0 } else { 0.0 };
                            egui::collapsing_header::paint_default_icon(ui, openness, &arrow);
                            if arrow.clicked() {
                                toggle = Some(dir.path.clone());
                            }
                        }
                        ui.label(name(dir, depth));
                    });
                    row.col(|ui| right_aligned(ui, human(dir.allocated)));
                    row.col(|ui| right_aligned(ui, human(dir.size)));
                    row.col(|ui| bar(ui, dir.allocated, parent));
                    row.col(|ui| right_aligned(ui, dir.files.to_string()));
                    let response = row.response();
                    if response.clicked() {
                        select = Some(dir.path.clone());
                    }
                    if response.double_clicked() && !dir.children.is_empty() {
                        toggle = Some(dir.path.clone());
                    }
                    // Not the scan root: the whole scanned folder is never one click away from the bin.
                    if depth > 0 && !deleting {
                        response.context_menu(|ui| {
                            if recycle_menu_item(ui) {
                                delete = Some(Deletion::folder(dir));
                                ui.close();
                            }
                        });
                    }
                });
            });
        if delete.is_some() {
            self.confirm = delete;
        }
        if let Some(path) = toggle && !self.expanded.remove(&path) {
            self.expanded.insert(path);
        }
        if let Some(path) = select {
            self.selected = Some(path);
        }
    }

    /// The selected folder: header, then its treemap or the files directly inside it.
    fn details(&mut self, ui: &mut egui::Ui) {
        let Some(scan) = &self.scan else { return };
        let chain = chain(&scan.tree, self.selected.as_deref().unwrap_or(&scan.tree.path));
        let dir = *chain.last().unwrap();
        let mut select = None;
        ui.horizontal(|ui| {
            if ui.add_enabled(chain.len() > 1, egui::Button::new("Up")).clicked() {
                select = Some(chain[chain.len() - 2].path.clone());
            }
            if ui.button("Open in Explorer").clicked() {
                open_in_file_manager(&dir.path);
            }
            ui.strong(display(&dir.path));
        });
        ui.label(format!("{} on disk · {} folders · {} files", human(dir.allocated), dir.children.len(), dir.files));
        // Saturating: deleting files a stale scan never counted can leave a folder below its subfolders' total.
        let own_files = dir.files.saturating_sub(dir.children.iter().map(|c| c.files).sum());
        ui.horizontal(|ui| {
            ui.selectable_value(&mut self.tab, Tab::Treemap, "Treemap");
            ui.selectable_value(&mut self.tab, Tab::Files, format!("Files ({own_files})"));
            ui.selectable_value(&mut self.tab, Tab::Types, "Types")
                .on_hover_text("Space per file extension in this folder and all its subfolders");
            ui.selectable_value(&mut self.tab, Tab::Largest, "Largest")
                .on_hover_text(format!("The {LARGEST} biggest files in this folder and all its subfolders"));
        });
        ui.separator();

        match self.tab {
            Tab::Treemap => select = select.or(treemap(ui, dir, &mut self.tab)),
            Tab::Files => {
                // Read on demand: a whole-drive scan keeps only totals, not 1.5 million file names.
                // In the background like the breakdown below, since a big folder or a share can be slow.
                match self.files_pending.as_ref().map(|(path, job)| (path, job.poll())) {
                    Some((path, Poll::Done(listing))) => {
                        self.files_cache = Some((path.clone(), listing));
                        self.files_pending = None;
                    }
                    Some((_, Poll::Died)) => self.files_pending = None,
                    _ => {}
                }
                let ready = self.files_cache.as_ref().filter(|(path, _)| path == &dir.path);
                let running = self.files_pending.as_ref().is_some_and(|(path, _)| path == &dir.path);
                match ready {
                    Some((_, (list, errors))) => {
                        if let Some(file) = files_table(ui, &dir.path, list, *errors, self.deleting.is_none()) {
                            self.confirm = Some(file);
                        }
                    }
                    None => {
                        if !running {
                            let path = dir.path.clone();
                            self.files_pending = Some((dir.path.clone(), Job::spawn(ui.ctx(), move || files(&path))));
                        }
                        // Also in the frame that starts the job: the spinner keeps frames coming until it is done.
                        ui.horizontal(|ui| {
                            ui.spinner();
                            ui.label("Reading the files directly in this folder…");
                        });
                    }
                }
            }
            Tab::Types | Tab::Largest => {
                // One walk of the whole subtree feeds both tabs; it runs in the background like a scan.
                match self.breakdown_pending.as_ref().map(|(path, job)| (path, job.poll())) {
                    Some((path, Poll::Done(found))) => {
                        self.breakdown = Some((path.clone(), found));
                        self.breakdown_pending = None;
                    }
                    Some((_, Poll::Died)) => self.breakdown_pending = None,
                    _ => {}
                }
                let ready = self.breakdown.as_ref().filter(|(path, _)| path == &dir.path);
                let running = self.breakdown_pending.as_ref().is_some_and(|(path, _)| path == &dir.path);
                match ready {
                    Some((_, found)) if self.tab == Tab::Types => types_table(ui, &found.types, dir.allocated),
                    Some((_, found)) => {
                        let (folder, delete) = largest_table(ui, &dir.path, &found.largest, self.deleting.is_none());
                        select = select.or(folder);
                        if delete.is_some() {
                            self.confirm = delete;
                        }
                    }
                    None => {
                        if !running {
                            let path = dir.path.clone();
                            self.breakdown_pending = Some((dir.path.clone(), Job::spawn(ui.ctx(), move || breakdown(&path, LARGEST))));
                        }
                        ui.horizontal(|ui| {
                            ui.spinner();
                            ui.label("Reading every file in this folder and its subfolders…");
                        });
                    }
                }
            }
        }
        if let Some(path) = select {
            self.select(path);
        }
    }
}

/// How many files the Largest tab lists.
const LARGEST: usize = 100;

/// The look every table shares: `builder` with one column per spec and a header row of their titles.
fn with_columns<'a>(builder: TableBuilder<'a>, columns: &[(Column, &str)]) -> Table<'a> {
    let mut builder = builder.striped(true).resizable(true).cell_layout(Layout::left_to_right(Align::Center));
    for &(column, _) in columns {
        builder = builder.column(column);
    }
    builder.header(22.0, |mut header| {
        for &(_, title) in columns {
            header.col(|ui| {
                ui.strong(title);
            });
        }
    })
}

/// Lists the biggest files under `folder`; returns the folder of a clicked file (to show it in the
/// tree) and a file the user asked to move to the Recycle Bin.
fn largest_table(ui: &mut egui::Ui, folder: &Path, list: &[LargeFile], can_delete: bool) -> (Option<PathBuf>, Option<Deletion>) {
    if list.is_empty() {
        ui.label("No files here, or this folder cannot be read (no access, or a scanned image).");
        return (None, None);
    }
    let (mut select, mut delete) = (None, None);
    let columns = [
        (Column::remainder().at_least(200.0).clip(true), "File (click: show its folder)"),
        (Column::initial(90.0).at_least(70.0), "On disk"),
        (Column::initial(90.0).at_least(70.0), "Size"),
    ];
    with_columns(TableBuilder::new(ui).sense(Sense::click()), &columns)
        .body(|body| {
            body.rows(20.0, list.len(), |mut row| {
                let file = &list[row.index()];
                row.col(|ui| {
                    ui.label(file.path.strip_prefix(folder).unwrap_or(&file.path).display().to_string());
                });
                row.col(|ui| right_aligned(ui, human(file.allocated)));
                row.col(|ui| right_aligned(ui, human(file.size)));
                let response = row.response();
                if response.clicked() {
                    select = file.path.parent().map(Path::to_path_buf);
                }
                if can_delete {
                    response.context_menu(|ui| {
                        if recycle_menu_item(ui) {
                            delete = Some(Deletion::large(file));
                            ui.close();
                        }
                    });
                }
            });
        });
    (select, delete)
}

fn types_table(ui: &mut egui::Ui, list: &[TypeStat], total: u64) {
    if list.is_empty() {
        ui.label("No files here, or this folder cannot be read (no access, or a scanned image).");
        return;
    }
    let columns = [
        (Column::remainder().at_least(90.0).clip(true), "Type"),
        (Column::initial(90.0).at_least(70.0), "On disk"),
        (Column::initial(90.0).at_least(70.0), "Size"),
        (Column::initial(110.0).at_least(80.0), "% of folder"),
        (Column::initial(70.0).at_least(50.0), "Files"),
    ];
    with_columns(TableBuilder::new(ui), &columns)
        .body(|body| {
            body.rows(20.0, list.len(), |mut row| {
                let t = &list[row.index()];
                row.col(|ui| {
                    ui.label(&t.ext);
                });
                row.col(|ui| right_aligned(ui, human(t.allocated)));
                row.col(|ui| right_aligned(ui, human(t.size)));
                row.col(|ui| bar(ui, t.allocated, total));
                row.col(|ui| right_aligned(ui, t.files.to_string()));
            });
        });
}

/// Lists `folder`'s files; returns a file the user asked to move to the Recycle Bin.
fn files_table(ui: &mut egui::Ui, folder: &Path, list: &[FileInfo], errors: u64, can_delete: bool) -> Option<Deletion> {
    if list.is_empty() {
        ui.label(if errors > 0 { "This folder cannot be read here (no access, or a scanned image)." } else { "No files directly in this folder." });
        return None;
    }
    let mut delete = None;
    let columns = [
        (Column::remainder().at_least(160.0).clip(true), "File"),
        (Column::initial(90.0).at_least(70.0), "On disk"),
        (Column::initial(90.0).at_least(70.0), "Size"),
    ];
    with_columns(TableBuilder::new(ui), &columns)
        .body(|body| {
            body.rows(20.0, list.len(), |mut row| {
                let file = &list[row.index()];
                row.col(|ui| {
                    ui.label(&file.name);
                });
                row.col(|ui| right_aligned(ui, human(file.allocated)));
                row.col(|ui| right_aligned(ui, human(file.size)));
                if can_delete {
                    row.response().context_menu(|ui| {
                        if recycle_menu_item(ui) {
                            delete = Some(Deletion::file(folder, file));
                            ui.close();
                        }
                    });
                }
            });
        });
    delete
}

/// Draws `dir`'s treemap; returns the subfolder clicked. Clicking the block of loose files
/// switches to the Files tab.
fn treemap(ui: &mut egui::Ui, dir: &Dir, tab: &mut Tab) -> Option<PathBuf> {
    let mut select = None;
    // Subfolders plus one block for the files directly in this folder.
    let own = dir.allocated.saturating_sub(dir.children.iter().map(|c| c.allocated).sum());
    let mut items: Vec<(u64, Option<&Dir>)> = dir.children.iter().map(|c| (c.allocated, Some(c))).collect();
    items.push((own, None));
    items.sort_unstable_by_key(|&(size, _)| Reverse(size));
    let sizes: Vec<f64> = items.iter().map(|(size, _)| *size as f64).collect();

    let (area, response) = ui.allocate_exact_size(ui.available_size(), Sense::click());
    let layout = squarify(&sizes, [area.min.x, area.min.y, area.width(), area.height()]);
    let painter = ui.painter_at(area);
    let pointer = response.hover_pos();
    let mut hovered = None;
    for (n, ((size, child), [x, y, w, h])) in items.iter().zip(layout).enumerate() {
        let rect = Rect::from_min_size(pos2(x, y), vec2(w, h));
        if rect.width() < 1.0 || rect.height() < 1.0 {
            continue;
        }
        let fill = match child {
            Some(_) => Color32::from(Hsva::new((n as f32 * 0.137) % 1.0, 0.45, 0.80, 1.0)),
            None => Color32::from_gray(150),
        };
        painter.rect_filled(rect.shrink(1.0), 3.0, fill);
        let label = child.map_or("(files)".to_string(), |c| name(c, 1));
        if pointer.is_some_and(|p| rect.contains(p)) {
            painter.rect_stroke(rect.shrink(1.0), 3.0, Stroke::new(2.0, Color32::WHITE), StrokeKind::Inside);
            hovered = Some((format!("{label}\n{}", human(*size)), *child));
        }
        if rect.width() > 70.0 && rect.height() > 34.0 {
            painter.with_clip_rect(rect.shrink(3.0)).text(
                rect.min + vec2(5.0, 4.0),
                Align2::LEFT_TOP,
                format!("{label}\n{}", human(*size)),
                FontId::proportional(12.0),
                Color32::BLACK,
            );
        }
    }
    if let Some((text, child)) = hovered {
        if response.clicked() {
            match child {
                Some(child) => select = Some(child.path.clone()),
                None => *tab = Tab::Files,
            }
        }
        response.on_hover_text_at_pointer(text);
    }
    select
}

/// Squarified treemap (Bruls, Huizing, van Wijk): lays `sizes`, sorted largest first, out in
/// `rect` = [x, y, width, height] as rectangles with aspect ratios close to 1, areas proportional
/// to size. Trailing zero sizes get empty rectangles.
fn squarify(sizes: &[f64], rect: [f32; 4]) -> Vec<[f32; 4]> {
    let mut out = vec![[0.0; 4]; sizes.len()];
    let total: f64 = sizes.iter().sum();
    let [mut x, mut y, mut w, mut h] = rect;
    if total <= 0.0 || w <= 0.0 || h <= 0.0 {
        return out;
    }
    let scale = (w * h) as f64 / total;
    // Worst aspect ratio of a row laid along a side of length `side`.
    let worst = |row: &[f64], side: f64| {
        let sum = row.iter().sum::<f64>() * scale;
        let (big, small) = (row[0] * scale, row[row.len() - 1] * scale);
        (side * side * big / (sum * sum)).max(sum * sum / (side * side * small))
    };
    let mut i = 0;
    while i < sizes.len() && sizes[i] > 0.0 && w > 0.0 && h > 0.0 {
        let side = w.min(h) as f64;
        let mut end = i + 1;
        while end < sizes.len() && sizes[end] > 0.0 && worst(&sizes[i..=end], side) <= worst(&sizes[i..end], side) {
            end += 1;
        }
        let area = sizes[i..end].iter().sum::<f64>() * scale;
        if w >= h {
            // Column on the left, filled top to bottom.
            let width = (area / h as f64) as f32;
            let mut top = y;
            for k in i..end {
                let height = (sizes[k] * scale / width as f64) as f32;
                out[k] = [x, top, width, height];
                top += height;
            }
            x += width;
            w -= width;
        } else {
            // Row along the top, filled left to right.
            let height = (area / w as f64) as f32;
            let mut left = x;
            for k in i..end {
                let width = (sizes[k] * scale / height as f64) as f32;
                out[k] = [left, y, width, height];
                left += width;
            }
            y += height;
            h -= height;
        }
        i = end;
    }
    out
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        match self.pending.as_ref().map(Job::poll) {
            Some(Poll::Done(Ok(scan))) => {
                self.pending = None;
                self.expanded = HashSet::from([scan.tree.path.clone()]);
                self.selected = Some(scan.tree.path.clone());
                self.scan = Some(scan);
            }
            Some(Poll::Done(Err(e))) => {
                self.pending = None;
                self.error = Some(format!("{}: {e}", self.path.trim()));
            }
            Some(Poll::Died) => {
                self.pending = None;
                self.error = Some("The scan stopped unexpectedly".to_string());
            }
            Some(Poll::Running) | None => {}
        }
        self.finish_deletion();
        self.confirm_deletion(ui.ctx());
        egui::Panel::top("toolbar").show(ui, |ui| self.toolbar(ui));
        egui::Panel::bottom("status").show(ui, |ui| self.status(ui));
        if self.scan.is_some() {
            egui::Panel::right("treemap").resizable(true).default_size(520.0).show(ui, |ui| self.details(ui));
        }
        egui::CentralPanel::default().show(ui, |ui| self.tree(ui));
    }
}

#[cfg(test)]
mod tests {
    use super::{explorer_arg, openable, squarify};
    use std::fs;

    #[test]
    fn explorer_drive_root_argument_is_restored() {
        assert_eq!(explorer_arg("C:\"".to_string()), "C:\\");
        assert_eq!(explorer_arg(r"C:\Program Files".to_string()), r"C:\Program Files");
    }

    #[test]
    fn only_real_folders_are_opened() {
        let base = fs::canonicalize(std::env::temp_dir()).unwrap().join(format!("treespace-open-{}", std::process::id()));
        fs::create_dir_all(&base).unwrap();
        assert!(openable(&base).is_some(), "a real folder opens");
        assert!(openable(&base.join("missing")).is_none(), "a path from a scanned image does not");
        fs::write(base.join("evil.exe"), b"").unwrap();
        assert!(openable(&base.join("evil.exe")).is_none(), "a file is never handed over");
        #[cfg(windows)]
        {
            // A folder only reachable verbatim, whose normalized name is the file above.
            let tricky = base.join("evil.exe.");
            fs::create_dir(&tricky).unwrap();
            assert!(tricky.is_dir() && !std::path::Path::new(&super::display(&tricky)).is_dir());
            assert!(openable(&tricky).is_none(), "trailing-dot folder must not reach explorer as evil.exe");
            // Same trap for deletion: refused before the shell is called, nothing moves.
            assert!(super::platform::recycle(&tricky).is_err(), "trailing-dot folder must not be binned as evil.exe");
            assert!(tricky.is_dir() && base.join("evil.exe").is_file(), "both items untouched");
            // A name shown with U+FFFD may stand for another item: refused, nothing moves.
            let lossy = base.join("twin\u{FFFD}.txt");
            fs::write(&lossy, b"").unwrap();
            assert!(super::platform::recycle(&lossy).is_err() && lossy.is_file(), "U+FFFD names are never binned");
        }
        fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn squarify_fills_rect_proportionally() {
        let sizes = [60.0, 25.0, 10.0, 4.0, 1.0, 0.0];
        let rects = squarify(&sizes, [10.0, 20.0, 300.0, 200.0]);
        let total_area = 300.0 * 200.0;
        for (size, [x, y, w, h]) in sizes.iter().zip(&rects) {
            let expected = (*size / 100.0 * total_area) as f32;
            assert!((w * h - expected).abs() < 1.0, "area of {size}: {} vs {expected}", w * h);
            if *size > 0.0 {
                assert!(*x >= 10.0 - 1e-3 && *y >= 20.0 - 1e-3, "inside top-left");
                assert!(x + w <= 310.0 + 1e-3 && y + h <= 220.0 + 1e-3, "inside bottom-right");
            }
        }
        // No two rectangles overlap.
        for a in 0..rects.len() {
            for b in a + 1..rects.len() {
                let ([ax, ay, aw, ah], [bx, by, bw, bh]) = (rects[a], rects[b]);
                let overlap = (ax + aw).min(bx + bw) - ax.max(bx);
                let overlap_y = (ay + ah).min(by + bh) - ay.max(by);
                assert!(overlap <= 1e-3 || overlap_y <= 1e-3, "rects {a} and {b} overlap");
            }
        }
    }
}
