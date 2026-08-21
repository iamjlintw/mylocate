//! 並行全量掃描器。
//!
//! 索引結構刻意分成兩張表：
//!
//! * **目錄表** —— 目錄數量相對少，每個目錄配一個全域 `dir_id`，並記下自己的
//!   `parent`，形成一棵樹。完整路徑靠往上爬這棵樹重建，不必為每個檔案存一份
//!   長路徑字串。
//! * **檔案表** —— 數量以百萬計，是效能關鍵。
//!
//! 兩張表在掃描期間都是 **per-worker 分片、完全無鎖**：`dir_id` 由一個 atomic
//! 計數器分段配發（一次要一批放在本地用），所以各分片的 parent 參照天生就是
//! 全域有效的，掃完直接依 id 填回同一個陣列即可，不需要重新映射。
//!
//! 每個 worker 認領一棵子樹後就一路遞迴下去，子目錄用 `openat(父 fd, 名稱)`
//! 開啟，不必把絕對路徑從根重新解析一次。只有在偵測到有人閒置時，才把手上
//! 一半的子目錄讓進全域佇列 —— 共享狀態因此極少被碰到。
//!
//! 附帶一提，這裡的效能天花板不是 CPU 而是核心：`kern.maxvnodes` 遠小於檔案
//! 總數，絕大多數目錄項目都得冷讀 APFS 的 B-tree。所以全量掃描只做一次，
//! 之後改由 FSEvents 增量維護。

use crate::ffi::*;
use libc::{c_int, c_void, close, open, O_DIRECTORY, O_RDONLY};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};

/// 目錄節點。名稱存在 `DirTable::names` arena 裡。
///
/// `repr(C)` 且剛好 12 bytes 無內部填充 —— 索引檔就是把這個陣列原樣寫出，
/// 載入時 mmap 回來直接當 slice 用，不需要任何反序列化。
#[repr(C)]
#[derive(Clone, Copy)]
pub struct DirNode {
    pub name_off: u32,
    /// 父目錄的 dir_id；根目錄為 `u32::MAX`。
    pub parent: u32,
    pub name_len: u16,
    pub _pad: u16,
}

impl Default for DirNode {
    fn default() -> Self {
        DirNode {
            name_off: 0,
            parent: u32::MAX,
            name_len: 0,
            _pad: 0,
        }
    }
}

/// 檔案節點，同樣是 12 bytes 的 `repr(C)`。
///
/// 刻意不存 size 與 mtime：那兩個欄位存在 inode record 裡，索取它們會讓核心
/// 對每個檔案多做一次 B-tree 查詢，而搜尋結果實際上只顯示幾十筆 —— 需要時
/// 對那幾筆補一次 stat 就好。省下 282 萬次查詢與 45 MB 記憶體。
#[repr(C)]
#[derive(Clone, Copy)]
pub struct FileRec {
    pub name_off: u32,
    pub parent: u32,
    pub name_len: u16,
    /// bit0：是否為 symlink
    pub flags: u16,
}

/// `FileRec::flags` 的位元定義
pub const FLAG_SYMLINK: u16 = 1 << 0;

#[derive(Default)]
pub struct DirTable {
    pub names: Vec<u8>,
    pub nodes: Vec<DirNode>,
}

/// 單一 worker 產出的檔案分片。
#[derive(Default)]
pub struct FileShard {
    pub names: Vec<u8>,
    pub recs: Vec<FileRec>,
}

impl FileShard {
    #[inline]
    fn push(&mut self, name: &[u8], parent: u32, is_link: bool) {
        let off = self.names.len() as u32;
        self.names.extend_from_slice(name);
        self.recs.push(FileRec {
            name_off: off,
            parent,
            name_len: name.len() as u16,
            flags: if is_link { FLAG_SYMLINK } else { 0 },
        });
    }
}

struct Task {
    path: Vec<u8>,
    dir_id: u32,
}

/// 一次向全域計數器索取的 dir_id 數量。
const ID_BATCH: u32 = 512;

/// 全域共享狀態。worker 盡量少碰它。
struct Shared {
    tasks: Mutex<Vec<Task>>,
    cv: Condvar,
    idle: AtomicUsize,
    workers: usize,
    done: AtomicBool,
    next_dir_id: AtomicU32,
    errors: AtomicUsize,
}

/// 單一 worker 的完整產出。
struct WorkerOut {
    files: FileShard,
    dir_names: Vec<u8>,
    /// (全域 dir_id, 節點)；節點的 name_off 此時仍是本分片 arena 的相對位移。
    dir_nodes: Vec<(u32, DirNode)>,
}

pub struct ScanStats {
    pub dirs: usize,
    pub files: usize,
    pub errors: usize,
}

pub struct ScanResult {
    pub dirs: DirTable,
    pub shards: Vec<FileShard>,
    pub stats: ScanStats,
}

/// 對 `root` 底下做全量掃描。`threads` 為 0 時自動取 CPU 核心數。
pub fn scan(root: &str, threads: usize) -> std::io::Result<ScanResult> {
    let nthreads = if threads == 0 {
        std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(8)
    } else {
        threads
    };

    let root_trimmed = root.trim_end_matches('/');
    let root_bytes: Vec<u8> = if root_trimmed.is_empty() {
        b"/".to_vec()
    } else {
        root_trimmed.as_bytes().to_vec()
    };

    // 掃描起點所在的 device，用來擋住跨磁碟區（外接碟、網路掛載）的遞迴。
    let root_dev = stat_dev(&root_bytes)?;

    // 根目錄固定拿 dir_id 0，其餘由 worker 從 1 開始分段配發。
    let shared = Arc::new(Shared {
        tasks: Mutex::new(vec![Task {
            path: root_bytes.clone(),
            dir_id: 0,
        }]),
        cv: Condvar::new(),
        idle: AtomicUsize::new(0),
        workers: nthreads,
        done: AtomicBool::new(false),
        next_dir_id: AtomicU32::new(1),
        errors: AtomicUsize::new(0),
    });

    let mut handles = Vec::with_capacity(nthreads);
    for _ in 0..nthreads {
        let shared = Arc::clone(&shared);
        handles.push(std::thread::spawn(move || worker(&shared, root_dev)));
    }

    let mut outs = Vec::with_capacity(nthreads);
    for h in handles {
        outs.push(h.join().expect("worker panic"));
    }

    // --- 合併各分片的目錄表 ---
    // dir_id 是全域配發的，直接依 id 填回同一個陣列；分段配發沒用完的 id 會留下
    // 空洞（預設節點），不會被任何檔案參照到，無害。
    let total_ids = shared.next_dir_id.load(Ordering::Relaxed) as usize;
    let mut names: Vec<u8> = Vec::with_capacity(root_bytes.len() + 1);
    let mut nodes: Vec<DirNode> = vec![DirNode::default(); total_ids];

    names.extend_from_slice(&root_bytes);
    nodes[0] = DirNode {
        name_off: 0,
        parent: u32::MAX,
        name_len: root_bytes.len() as u16,
        _pad: 0,
    };

    let mut real_dirs = 1usize;
    let mut shards = Vec::with_capacity(nthreads);
    for out in outs {
        let base = names.len() as u32;
        names.extend_from_slice(&out.dir_names);
        real_dirs += out.dir_nodes.len();
        for (id, mut node) in out.dir_nodes {
            node.name_off += base;
            nodes[id as usize] = node;
        }
        shards.push(out.files);
    }

    let stats = ScanStats {
        dirs: real_dirs,
        files: shards.iter().map(|s| s.recs.len()).sum(),
        errors: shared.errors.load(Ordering::Relaxed),
    };
    Ok(ScanResult {
        dirs: DirTable { names, nodes },
        shards,
        stats,
    })
}

/// 列舉單一目錄的直接內容，回傳 `(名稱, 是否為目錄, 是否為 symlink)`。
///
/// 供 FSEvents 增量更新使用：事件旗標會被合併（同一路徑可能同時帶著
/// 建立/刪除/修改），無法據以推斷檔案的最終狀態，所以一律重新列舉該目錄，
/// 拿實際內容跟索引做 diff。一次系統呼叫就能涵蓋這個目錄的所有變更。
pub fn list_dir(path: &[u8]) -> std::io::Result<Vec<(Vec<u8>, bool, bool)>> {
    let cpath = to_cstring(path);
    let fd = unsafe { open(cpath.as_ptr(), O_RDONLY | O_DIRECTORY | libc::O_NOFOLLOW) };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }

    let mut layout = build_attrlist_fast();
    let mut buf = vec![0u8; 64 * 1024];
    let mut out = Vec::new();

    loop {
        let n = unsafe {
            getattrlistbulk(
                fd,
                &mut layout.attrlist as *mut Attrlist as *mut c_void,
                buf.as_mut_ptr() as *mut c_void,
                buf.len(),
                FSOPT_NOFOLLOW | FSOPT_PACK_INVAL_ATTRS,
            )
        };
        if n < 0 {
            let e = std::io::Error::last_os_error();
            unsafe { close(fd) };
            return Err(e);
        }
        if n == 0 {
            break;
        }

        let mut off = 0usize;
        for _ in 0..n {
            if off + layout.min_len > buf.len() {
                break;
            }
            let entry = &buf[off..];
            let entry_len = rd_u32(entry, 0) as usize;
            if entry_len < layout.min_len || off + entry_len > buf.len() {
                break;
            }
            let name_rel = rd_i32(entry, OFF_NAME_REF) as isize;
            let name_len_with_nul = rd_u32(entry, OFF_NAME_REF + 4) as usize;
            let name_start = (OFF_NAME_REF as isize + name_rel) as usize;
            if name_len_with_nul >= 2 && name_start + name_len_with_nul <= entry_len {
                let name = &entry[name_start..name_start + name_len_with_nul - 1];
                let objtype = rd_u32(entry, OFF_OBJTYPE);
                match objtype {
                    VDIR => out.push((name.to_vec(), true, false)),
                    VREG => out.push((name.to_vec(), false, false)),
                    VLNK => out.push((name.to_vec(), false, true)),
                    _ => {}
                }
            }
            off += entry_len;
        }
    }
    unsafe { close(fd) };
    Ok(out)
}

fn stat_dev(path: &[u8]) -> std::io::Result<i32> {
    let cpath = to_cstring(path);
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    if unsafe { libc::stat(cpath.as_ptr(), &mut st) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(st.st_dev)
}

fn to_cstring(b: &[u8]) -> Vec<i8> {
    let mut v: Vec<i8> = Vec::with_capacity(b.len() + 1);
    v.extend(b.iter().map(|&c| c as i8));
    v.push(0);
    v
}

/// 遞迴每一層用到的暫存區。依深度放進 pool 重複使用，避免每個目錄都重新配置。
#[derive(Default)]
struct Scratch {
    names: Vec<u8>,
    /// (在 names 中的位移, 長度)
    subdirs: Vec<(u32, u16)>,
    /// 與 subdirs 一一對應的全域 dir_id
    ids: Vec<u32>,
}

/// 遞迴深度上限。超過就改走全域佇列，避免 fd 與呼叫堆疊無限增長。
const MAX_DEPTH: usize = 64;

struct Ctx<'a> {
    shared: &'a Shared,
    files: FileShard,
    dir_names: Vec<u8>,
    dir_nodes: Vec<(u32, DirNode)>,
    id_cursor: u32,
    id_end: u32,
    buf: Vec<u8>,
    layout: Layout,
    pool: Vec<Scratch>,
    /// 目前遞迴所在目錄的完整路徑，只在需要把工作分享出去時才用得到。
    path: Vec<u8>,
    root_dev: i32,
}

impl<'a> Ctx<'a> {
    /// 無鎖配發 dir_id：一次向全域計數器要一批，用完再要。
    #[inline]
    fn alloc_id(&mut self) -> u32 {
        if self.id_cursor == self.id_end {
            let base = self.shared.next_dir_id.fetch_add(ID_BATCH, Ordering::Relaxed);
            self.id_cursor = base;
            self.id_end = base + ID_BATCH;
        }
        let id = self.id_cursor;
        self.id_cursor += 1;
        id
    }

    /// 深度優先走完 `fd` 這棵子樹。`fd` 由呼叫端負責關閉。
    ///
    /// 子目錄一律用 `openat(fd, name)` 打開 —— 核心只需要在已開啟的父目錄裡查
    /// 一個名稱，而不是把整條絕對路徑從根再解析一次。目錄樹愈深，省得愈多。
    fn scan_subtree(&mut self, fd: c_int, dir_id: u32, depth: usize) {
        let mut sc = self.pool.pop().unwrap_or_default();
        sc.names.clear();
        sc.subdirs.clear();
        sc.ids.clear();

        let ok = enumerate_dir(
            fd,
            &mut self.buf,
            &mut self.layout,
            &mut self.files,
            &mut sc,
            dir_id,
        );
        if ok.is_err() {
            self.shared.errors.fetch_add(1, Ordering::Relaxed);
        }

        // 先替所有子目錄配好 id 並登記節點，之後才決定哪些自己走、哪些分出去。
        for &(off, len) in sc.subdirs.iter() {
            let id = self.alloc_id();
            let name = &sc.names[off as usize..off as usize + len as usize];
            let name_off = self.dir_names.len() as u32;
            self.dir_names.extend_from_slice(name);
            self.dir_nodes.push((
                id,
                DirNode {
                    name_off,
                    parent: dir_id,
                    name_len: len,
                    _pad: 0,
                },
            ));
            sc.ids.push(id);
        }

        // 有人閒著就把一半的子樹讓出去（這時才需要組出絕對路徑）。太深也一律
        // 讓出去，換取遞迴深度歸零。
        let too_deep = depth >= MAX_DEPTH;
        let share_from = if too_deep {
            0
        } else if sc.subdirs.len() > 1 && self.shared.idle.load(Ordering::Relaxed) > 0 {
            sc.subdirs.len() / 2
        } else {
            sc.subdirs.len()
        };

        if share_from < sc.subdirs.len() {
            let mut handout = Vec::with_capacity(sc.subdirs.len() - share_from);
            for i in share_from..sc.subdirs.len() {
                let (off, len) = sc.subdirs[i];
                let name = &sc.names[off as usize..off as usize + len as usize];
                let mut full = Vec::with_capacity(self.path.len() + 1 + name.len());
                full.extend_from_slice(&self.path);
                if !full.ends_with(b"/") {
                    full.push(b'/');
                }
                full.extend_from_slice(name);
                handout.push(Task {
                    path: full,
                    dir_id: sc.ids[i],
                });
            }
            let mut tasks = self.shared.tasks.lock().unwrap();
            let was_empty = tasks.is_empty();
            tasks.extend(handout);
            drop(tasks);
            if was_empty {
                self.shared.cv.notify_all();
            }
            sc.subdirs.truncate(share_from);
        }

        // 其餘子目錄留給自己，沿著 fd 直接往下鑽。
        let path_len = self.path.len();
        for i in 0..sc.subdirs.len() {
            let (off, len) = sc.subdirs[i];
            let nl = len as usize;
            let mut namebuf = [0u8; 256];
            if nl >= namebuf.len() {
                continue;
            }
            namebuf[..nl].copy_from_slice(&sc.names[off as usize..off as usize + nl]);
            namebuf[nl] = 0;

            let child = unsafe {
                libc::openat(
                    fd,
                    namebuf.as_ptr() as *const i8,
                    O_RDONLY | O_DIRECTORY | libc::O_NOFOLLOW,
                )
            };
            if child < 0 {
                self.shared.errors.fetch_add(1, Ordering::Relaxed);
                continue;
            }
            // 擋掉跨磁碟區：外接碟與網路掛載不該被這趟掃描拖住。
            let mut st: libc::stat = unsafe { std::mem::zeroed() };
            if unsafe { libc::fstat(child, &mut st) } != 0 || st.st_dev != self.root_dev {
                unsafe { close(child) };
                continue;
            }

            if !self.path.ends_with(b"/") {
                self.path.push(b'/');
            }
            self.path.extend_from_slice(&namebuf[..nl]);
            self.scan_subtree(child, sc.ids[i], depth + 1);
            self.path.truncate(path_len);
            unsafe { close(child) };
        }

        self.pool.push(sc);
    }
}

fn worker(shared: &Shared, root_dev: i32) -> WorkerOut {
    let mut ctx = Ctx {
        shared,
        files: FileShard::default(),
        dir_names: Vec::new(),
        dir_nodes: Vec::new(),
        id_cursor: 0,
        id_end: 0,
        // 每次系統呼叫拿回一大批項目，緩衝區給大一點以攤平呼叫成本。
        buf: vec![0u8; 256 * 1024],
        layout: build_attrlist_fast(),
        pool: Vec::new(),
        path: Vec::with_capacity(4096),
        root_dev,
    };

    while let Some(task) = next_task(shared) {
        let cpath = to_cstring(&task.path);
        let fd = unsafe { open(cpath.as_ptr(), O_RDONLY | O_DIRECTORY | libc::O_NOFOLLOW) };
        if fd < 0 {
            shared.errors.fetch_add(1, Ordering::Relaxed);
            continue;
        }
        ctx.path.clear();
        ctx.path.extend_from_slice(&task.path);
        ctx.scan_subtree(fd, task.dir_id, 0);
        unsafe { close(fd) };
    }

    WorkerOut {
        files: ctx.files,
        dir_names: ctx.dir_names,
        dir_nodes: ctx.dir_nodes,
    }
}

/// 從全域佇列取下一個子樹。回傳 None 代表整趟掃描結束。
fn next_task(shared: &Shared) -> Option<Task> {
    let mut tasks = shared.tasks.lock().unwrap();
    loop {
        if let Some(t) = tasks.pop() {
            return Some(t);
        }
        if shared.done.load(Ordering::Acquire) {
            return None;
        }
        // 佇列空了：登記自己閒置。所有 worker 都閒置即代表整棵樹走完。
        let idle = shared.idle.fetch_add(1, Ordering::SeqCst) + 1;
        if idle == shared.workers {
            shared.done.store(true, Ordering::Release);
            shared.cv.notify_all();
            shared.idle.fetch_sub(1, Ordering::SeqCst);
            return None;
        }
        let (g, _) = shared
            .cv
            .wait_timeout(tasks, std::time::Duration::from_millis(5))
            .unwrap();
        tasks = g;
        shared.idle.fetch_sub(1, Ordering::SeqCst);
    }
}

/// 列舉單一已開啟的目錄：檔案直接寫進分片，子目錄名稱收進 scratch。
fn enumerate_dir(
    fd: c_int,
    buf: &mut [u8],
    layout: &mut Layout,
    files: &mut FileShard,
    sc: &mut Scratch,
    dir_id: u32,
) -> Result<(), ()> {
    loop {
        let n = unsafe {
            getattrlistbulk(
                fd,
                &mut layout.attrlist as *mut Attrlist as *mut c_void,
                buf.as_mut_ptr() as *mut c_void,
                buf.len(),
                FSOPT_NOFOLLOW | FSOPT_PACK_INVAL_ATTRS,
            )
        };
        if n < 0 {
            return Err(());
        }
        if n == 0 {
            break; // 列舉結束
        }

        let mut off = 0usize;
        for _ in 0..n {
            if off + layout.min_len > buf.len() {
                break;
            }
            let entry = &buf[off..];
            let entry_len = rd_u32(entry, 0) as usize;
            if entry_len < layout.min_len || off + entry_len > buf.len() {
                break;
            }

            // 名稱：attrreference 的 offset 是相對於它自己的位置。
            let name_rel = rd_i32(entry, OFF_NAME_REF) as isize;
            let name_len_with_nul = rd_u32(entry, OFF_NAME_REF + 4) as usize;
            let name_start = (OFF_NAME_REF as isize + name_rel) as usize;

            if name_len_with_nul >= 2 && name_start + name_len_with_nul <= entry_len {
                let name = &entry[name_start..name_start + name_len_with_nul - 1];
                let objtype = rd_u32(entry, OFF_OBJTYPE);
                match objtype {
                    VDIR => {
                        let o = sc.names.len() as u32;
                        sc.names.extend_from_slice(name);
                        sc.subdirs.push((o, name.len() as u16));
                    }
                    VREG | VLNK => files.push(name, dir_id, objtype == VLNK),
                    _ => {}
                }
            }
            off += entry_len;
        }
    }
    Ok(())
}
