//! 索引的序列化格式與載入。
//!
//! 格式刻意設計成「可以直接 mmap 回來當 slice 用」：所有節點都是 12 bytes 的
//! `repr(C)` 結構，檔案裡就是原樣的陣列，載入時不做任何反序列化或配置。
//! 因此 daemon 重啟的成本趨近於零 —— 那 13 秒的全量掃描一輩子只付一次。

use crate::scan::{DirNode, FileRec, ScanResult};
use std::io::Write;
use std::path::{Path, PathBuf};

pub const MAGIC: [u8; 8] = *b"MYLOCIDX";
pub const VERSION: u32 = 2;
/// 資料區起點。header 佔 192 bytes，之後所有段落都對齊到 8 bytes。
const DATA_START: u64 = 192;

/// 索引檔標頭，固定 192 bytes。
#[repr(C)]
#[derive(Clone, Copy)]
pub struct Header {
    pub magic: [u8; 8],
    pub version: u32,
    pub _pad0: u32,
    pub root_off: u64,
    pub root_len: u64,
    pub dir_names_off: u64,
    pub dir_names_len: u64,
    pub dir_nodes_off: u64,
    pub n_dirs: u64,
    pub file_names_off: u64,
    pub file_names_len: u64,
    /// 檔案紀錄依 `parent` 由小到大排序，因此可用二分搜尋取出某個目錄底下的
    /// 全部檔案 —— 這是 FSEvents 增量更新時做 diff 的前提。
    pub file_recs_off: u64,
    pub n_files: u64,
    pub scan_time: i64,
    /// FSEvents 續傳點：下次啟動從這個事件之後開始重放。
    pub event_id: u64,
    /// FSEvents 資料庫的 UUID。與存檔時不同就代表事件日誌已重建，
    /// event_id 失效，必須退回全量重掃。
    pub dev_uuid: [u8; 16],
    /// 目錄 id 依其 `parent` 排序後的清單。目錄本身必須維持原 id（檔案的
    /// `parent` 直接引用它），所以改用一份額外的索引來支援依父目錄查找。
    pub dir_by_parent_off: u64,
    pub n_dir_by_parent: u64,
    pub _reserved: [u8; 40],
}

/// 索引檔的預設位置，遵循 macOS 的快取目錄慣例。
pub fn default_path() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
    Path::new(&home).join("Library/Caches/mylocate/index.bin")
}

#[inline]
fn align8(n: u64) -> u64 {
    (n + 7) & !7
}

/// 把掃描結果序列化成索引檔的位元組內容。
///
/// 各 worker 的檔案分片在這裡串接：分片內的 `name_off` 是分片相對位移，
/// 串接時補上該分片的起始位移即可；`parent` 是全域 dir_id，天生就正確。
pub fn build(scan: &ScanResult, event_id: u64, dev_uuid: [u8; 16], scan_time: i64) -> Vec<u8> {
    // --- 先把各 worker 分片串成一份連續資料 ---
    // 分片內的 name_off 是分片相對位移，串接時補上該分片的起始位移；
    // parent 是全域 dir_id，天生就正確。
    let n_files_usize: usize = scan.shards.iter().map(|s| s.recs.len()).sum();
    let names_len_usize: usize = scan.shards.iter().map(|s| s.names.len()).sum();
    let mut tmp_names: Vec<u8> = Vec::with_capacity(names_len_usize);
    let mut tmp_recs: Vec<FileRec> = Vec::with_capacity(n_files_usize);
    for shard in scan.shards.iter() {
        let base = tmp_names.len() as u32;
        tmp_names.extend_from_slice(&shard.names);
        tmp_recs.extend(shard.recs.iter().map(|r| FileRec {
            name_off: r.name_off + base,
            parent: r.parent,
            name_len: r.name_len,
            flags: r.flags,
        }));
    }

    // --- 依 parent 排序，並照新順序重建名稱 arena ---
    // 只排序紀錄、不搬動名稱的話，搜尋時對 arena 的存取就會變成亂序而傷到
    // 快取；這裡多付一次重建成本，換取「搜尋循序、增量可二分」兩者兼得。
    let mut order: Vec<u32> = (0..tmp_recs.len() as u32).collect();
    order.sort_unstable_by_key(|&i| tmp_recs[i as usize].parent);

    let mut sorted_names: Vec<u8> = Vec::with_capacity(tmp_names.len());
    let mut sorted_recs: Vec<FileRec> = Vec::with_capacity(tmp_recs.len());
    for &i in order.iter() {
        let r = tmp_recs[i as usize];
        let s = r.name_off as usize;
        let off = sorted_names.len() as u32;
        sorted_names.extend_from_slice(&tmp_names[s..s + r.name_len as usize]);
        sorted_recs.push(FileRec { name_off: off, ..r });
    }
    drop(tmp_names);
    drop(tmp_recs);
    drop(order);

    // --- 目錄依 parent 排序的索引 ---
    // 目錄自身的 id 不能動（檔案的 parent 直接引用），所以另外做一份排序索引。
    // 順帶濾掉 dir_id 分段配發留下的空洞。
    let mut dir_by_parent: Vec<u32> = (0..scan.dirs.nodes.len() as u32)
        .filter(|&i| i == 0 || scan.dirs.nodes[i as usize].name_len > 0)
        .collect();
    dir_by_parent.sort_unstable_by_key(|&i| scan.dirs.nodes[i as usize].parent);

    let root_len = scan.dirs.nodes.first().map_or(0, |n| n.name_len as u64);
    let dir_names_len = scan.dirs.names.len() as u64;
    let n_dirs = scan.dirs.nodes.len() as u64;
    let file_names_len = sorted_names.len() as u64;
    let n_files = sorted_recs.len() as u64;
    let n_dir_by_parent = dir_by_parent.len() as u64;

    // 根路徑就存在 dir_names 的開頭，不另外複製一份。
    let root_off = DATA_START;
    let dir_names_off = DATA_START;
    let dir_nodes_off = align8(dir_names_off + dir_names_len);
    let dir_by_parent_off = align8(dir_nodes_off + n_dirs * 12);
    let file_names_off = align8(dir_by_parent_off + n_dir_by_parent * 4);
    let file_recs_off = align8(file_names_off + file_names_len);
    let total = file_recs_off + n_files * 12;

    let mut buf = vec![0u8; total as usize];
    let header = Header {
        magic: MAGIC,
        version: VERSION,
        _pad0: 0,
        root_off,
        root_len,
        dir_names_off,
        dir_names_len,
        dir_nodes_off,
        n_dirs,
        file_names_off,
        file_names_len,
        file_recs_off,
        n_files,
        scan_time,
        event_id,
        dev_uuid,
        dir_by_parent_off,
        n_dir_by_parent,
        _reserved: [0u8; 40],
    };
    unsafe {
        std::ptr::copy_nonoverlapping(
            &header as *const Header as *const u8,
            buf.as_mut_ptr(),
            std::mem::size_of::<Header>(),
        );
    }

    let at = |off: u64| off as usize;
    buf[at(dir_names_off)..at(dir_names_off) + scan.dirs.names.len()]
        .copy_from_slice(&scan.dirs.names);

    unsafe {
        std::ptr::copy_nonoverlapping(
            scan.dirs.nodes.as_ptr() as *const u8,
            buf.as_mut_ptr().add(at(dir_nodes_off)),
            scan.dirs.nodes.len() * 12,
        );
        std::ptr::copy_nonoverlapping(
            dir_by_parent.as_ptr() as *const u8,
            buf.as_mut_ptr().add(at(dir_by_parent_off)),
            dir_by_parent.len() * 4,
        );
        std::ptr::copy_nonoverlapping(
            sorted_recs.as_ptr() as *const u8,
            buf.as_mut_ptr().add(at(file_recs_off)),
            sorted_recs.len() * 12,
        );
    }
    buf[at(file_names_off)..at(file_names_off) + sorted_names.len()]
        .copy_from_slice(&sorted_names);

    buf
}

/// 原子性寫入：先寫暫存檔再 rename，避免中途失敗留下半截索引。
pub fn write_to(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("tmp");
    {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, path)
}

/// 索引的記憶體來源：mmap 回來的檔案，或剛掃描完還在記憶體裡的 buffer。
enum Backing {
    Owned(Vec<u8>),
    Mapped { ptr: *mut libc::c_void, len: usize },
}

impl Drop for Backing {
    fn drop(&mut self) {
        if let Backing::Mapped { ptr, len } = *self {
            unsafe { libc::munmap(ptr, len) };
        }
    }
}

// mmap 出來的區域唯讀且生命週期綁在 Index 上，跨執行緒共享是安全的。
unsafe impl Send for Backing {}
unsafe impl Sync for Backing {}

pub struct Index {
    backing: Backing,
}

impl Index {
    #[allow(dead_code)]
    pub fn from_vec(v: Vec<u8>) -> std::io::Result<Index> {
        let idx = Index {
            backing: Backing::Owned(v),
        };
        idx.validate()?;
        Ok(idx)
    }

    /// 以唯讀方式 mmap 索引檔。不複製、不解析，回傳即可用。
    pub fn open(path: &Path) -> std::io::Result<Index> {
        let f = std::fs::File::open(path)?;
        let len = f.metadata()?.len() as usize;
        if len < std::mem::size_of::<Header>() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "索引檔過小",
            ));
        }
        use std::os::unix::io::AsRawFd;
        let ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                len,
                libc::PROT_READ,
                libc::MAP_PRIVATE,
                f.as_raw_fd(),
                0,
            )
        };
        if ptr == libc::MAP_FAILED {
            return Err(std::io::Error::last_os_error());
        }
        let idx = Index {
            backing: Backing::Mapped { ptr, len },
        };
        idx.validate()?;
        Ok(idx)
    }

    fn bytes(&self) -> &[u8] {
        match &self.backing {
            Backing::Owned(v) => v,
            Backing::Mapped { ptr, len } => unsafe {
                std::slice::from_raw_parts(*ptr as *const u8, *len)
            },
        }
    }

    /// 檢查魔數、版本，以及每個段落都落在檔案範圍內 —— 索引檔可能來自
    /// 舊版本或被截斷，這裡擋掉才能安全地把它當成 slice 用。
    fn validate(&self) -> std::io::Result<()> {
        let bad = |m: &str| std::io::Error::new(std::io::ErrorKind::InvalidData, m.to_string());
        let b = self.bytes();
        if b.len() < std::mem::size_of::<Header>() {
            return Err(bad("索引檔過小"));
        }
        let h = self.header();
        if h.magic != MAGIC {
            return Err(bad("索引檔格式不符"));
        }
        if h.version != VERSION {
            return Err(bad("索引檔版本不符，請重新建立索引"));
        }
        let total = b.len() as u64;
        let ok = h.dir_names_off + h.dir_names_len <= total
            && h.dir_nodes_off + h.n_dirs * 12 <= total
            && h.dir_by_parent_off + h.n_dir_by_parent * 4 <= total
            && h.file_names_off + h.file_names_len <= total
            && h.file_recs_off + h.n_files * 12 <= total
            && h.root_off + h.root_len <= total;
        if !ok {
            return Err(bad("索引檔已損毀（段落超出範圍）"));
        }
        Ok(())
    }

    pub fn header(&self) -> &Header {
        unsafe { &*(self.bytes().as_ptr() as *const Header) }
    }

    pub fn root(&self) -> &[u8] {
        let h = self.header();
        &self.bytes()[h.root_off as usize..(h.root_off + h.root_len) as usize]
    }

    pub fn dir_names(&self) -> &[u8] {
        let h = self.header();
        &self.bytes()[h.dir_names_off as usize..(h.dir_names_off + h.dir_names_len) as usize]
    }

    pub fn file_names(&self) -> &[u8] {
        let h = self.header();
        &self.bytes()[h.file_names_off as usize..(h.file_names_off + h.file_names_len) as usize]
    }

    pub fn dir_nodes(&self) -> &[DirNode] {
        let h = self.header();
        unsafe {
            std::slice::from_raw_parts(
                self.bytes().as_ptr().add(h.dir_nodes_off as usize) as *const DirNode,
                h.n_dirs as usize,
            )
        }
    }

    pub fn file_recs(&self) -> &[FileRec] {
        let h = self.header();
        unsafe {
            std::slice::from_raw_parts(
                self.bytes().as_ptr().add(h.file_recs_off as usize) as *const FileRec,
                h.n_files as usize,
            )
        }
    }

    #[inline]
    pub fn file_name(&self, i: usize) -> &[u8] {
        let r = &self.file_recs()[i];
        let s = r.name_off as usize;
        &self.file_names()[s..s + r.name_len as usize]
    }

    #[inline]
    pub fn dir_name(&self, id: u32) -> &[u8] {
        let n = &self.dir_nodes()[id as usize];
        let s = n.name_off as usize;
        &self.dir_names()[s..s + n.name_len as usize]
    }

    /// 由 dir_id 往上爬回完整路徑。
    #[allow(dead_code)]
    pub fn dir_path(&self, mut id: u32) -> Vec<u8> {
        let nodes = self.dir_nodes();
        let mut parts: Vec<&[u8]> = Vec::new();
        let mut guard = 0;
        while id != u32::MAX && (id as usize) < nodes.len() && guard < 256 {
            parts.push(self.dir_name(id));
            id = nodes[id as usize].parent;
            guard += 1;
        }
        let mut out = Vec::with_capacity(parts.iter().map(|p| p.len() + 1).sum());
        for part in parts.iter().rev() {
            // 根節點存的是完整前綴（例如 "/Users/名稱"），不重複加分隔符。
            if !out.is_empty() && !out.ends_with(b"/") {
                out.push(b'/');
            }
            out.extend_from_slice(part);
        }
        out
    }

    /// 目錄 id 依 parent 排序後的清單。
    pub fn dir_by_parent(&self) -> &[u32] {
        let h = self.header();
        unsafe {
            std::slice::from_raw_parts(
                self.bytes().as_ptr().add(h.dir_by_parent_off as usize) as *const u32,
                h.n_dir_by_parent as usize,
            )
        }
    }

    /// 某個目錄底下的所有檔案，回傳在 `file_recs()` 中的索引區間。
    ///
    /// 紀錄已依 parent 排序，所以這是一次二分搜尋，而不是掃過 283 萬筆。
    pub fn files_in_dir(&self, parent: u32) -> std::ops::Range<usize> {
        let recs = self.file_recs();
        let lo = recs.partition_point(|r| r.parent < parent);
        let hi = recs.partition_point(|r| r.parent <= parent);
        lo..hi
    }

    /// 某個目錄底下的所有子目錄 id。
    pub fn child_dirs(&self, parent: u32) -> Vec<u32> {
        let nodes = self.dir_nodes();
        let dbp = self.dir_by_parent();
        let key = |&id: &u32| nodes[id as usize].parent;
        let lo = dbp.partition_point(|id| key(id) < parent);
        let hi = dbp.partition_point(|id| key(id) <= parent);
        dbp[lo..hi].to_vec()
    }

    /// 第 i 個檔案的完整路徑。
    pub fn file_path(&self, i: usize) -> Vec<u8> {
        let r = &self.file_recs()[i];
        let mut p = self.dir_path(r.parent);
        if !p.ends_with(b"/") {
            p.push(b'/');
        }
        p.extend_from_slice(self.file_name(i));
        p
    }
}
