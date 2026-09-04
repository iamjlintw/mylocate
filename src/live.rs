//! 可即時更新的索引。
//!
//! 基底（`Index`）是 mmap 進來的不可變快照，所有變更累積在一層薄薄的 delta 上：
//!
//! * 刪除 → 在 bitmap 上標記，基底本身不動
//! * 新增 → 追加到 delta 的陣列
//!
//! 這樣做的好處是基底完全不需要重寫，而搜尋時對基底的掃描仍是循序的 —— 也就是
//! 說增量更新不會侵蝕搜尋效能。delta 累積過多時才需要重建快照。
//!
//! 目錄 id 是一個連續空間：`0..base.n_dirs` 屬於基底，之後的屬於 delta；
//! 檔案 id 也比照辦理。呼叫端因此不必區分兩者。

use crate::index::Index;
use crate::scan::{self, DirNode, FileRec, FLAG_SYMLINK};
use std::collections::HashMap;
use std::sync::Arc;

/// 位元圖，用來標記基底中已被刪除的項目。
#[derive(Clone)]
struct BitSet {
    bits: Vec<u64>,
}

impl BitSet {
    fn new(n: usize) -> BitSet {
        BitSet {
            bits: vec![0u64; n.div_ceil(64)],
        }
    }
    #[inline]
    fn set(&mut self, i: usize) {
        if let Some(w) = self.bits.get_mut(i >> 6) {
            *w |= 1u64 << (i & 63);
        }
    }
    #[inline]
    fn get(&self, i: usize) -> bool {
        self.bits
            .get(i >> 6)
            .is_some_and(|w| w & (1u64 << (i & 63)) != 0)
    }
    fn count(&self) -> usize {
        self.bits.iter().map(|w| w.count_ones() as usize).sum()
    }
}

/// 不可變的基底，被所有快照共享。
///
/// 它佔了索引的絕大部分（上百 MB），抽出來用 `Arc` 共享之後，產生一份新快照
/// 就只需要複製那層很薄的 delta，因此更新可以走 copy-on-write。
pub struct BaseData {
    pub index: Index,
    /// 基底檔名 arena 的全小寫副本。
    ///
    /// 存在的唯一理由是讓大小寫不敏感的比對能用上 SIMD：needle 先轉小寫，
    /// haystack 也已是小寫，就能直接丟給向量化的 memmem，不必逐 byte 做
    /// case folding。只在記憶體裡建（啟動時約數十毫秒），不寫進索引檔，
    /// 所以索引格式不受影響。
    pub lower: Vec<u8>,
}

/// 索引在某個時間點的快照。
///
/// 查詢端永遠拿著一個 `Arc<Live>`，完全不需要等鎖；更新端複製一份、改完再
/// 原子替換掉。這是讀多寫少的正解 —— 先前用 `RwLock` 直接保護整份索引時，
/// 查詢中位數雖然只有 7 ms，但偶爾撞上寫鎖就會飆到 100 ms。
#[derive(Clone)]
pub struct Live {
    base: Arc<BaseData>,
    add_lower: Vec<u8>,
    dead_files: BitSet,
    dead_dirs: BitSet,

    add_names: Vec<u8>,
    add_recs: Vec<FileRec>,
    add_dir_names: Vec<u8>,
    add_dir_nodes: Vec<DirNode>,

    /// delta 中「父目錄 → 子目錄 id」的索引，避免線性掃描。
    add_children: HashMap<u32, Vec<u32>>,
    /// delta 中「父目錄 → 檔案在 add_recs 的位置」的索引。
    add_files: HashMap<u32, Vec<u32>>,
    /// delta 裡已被刪除的項目（新增後又刪掉）。
    dead_add_files: BitSet,
    dead_add_dirs: BitSet,

    /// 事件要忽略的路徑前綴。
    ///
    /// 索引檔自己就放在監看範圍內，每次重建都會寫入上百 MB —— 若不排除，
    /// daemon 就會被自己寫檔產生的事件觸發，形成「重建→產生事件→再重建」
    /// 的回饋迴圈。
    ignore_prefixes: Vec<Vec<u8>>,
}

/// 搜尋命中的項目。id 位於 `Live` 的統一空間。
#[derive(Clone, Copy)]
pub enum Hit {
    File(u32),
    Dir(u32),
}

/// 一批事件換算出來的工作清單。
///
/// 刻意把「重新列舉單層」和「整棵重掃」分開：事件溢位只代表那個子樹的內容
/// 不可信，沒有理由把整份索引丟掉重建。
#[derive(Default)]
pub struct Plan {
    /// 重新列舉這些目錄的直接內容。
    pub refresh: Vec<(u32, Vec<u8>)>,
    /// 待換算成 dir_id 的重掃路徑（plan() 內部用）。
    rescan: Vec<Vec<u8>>,
    /// 需要整棵重掃的子樹。
    pub rescan_ids: Vec<(u32, Vec<u8>)>,
    /// 只有根目錄被搬走、事件 id 回繞這類情況才需要全量重建。
    pub need_full: bool,
    /// 診斷訊息，讓日誌能說明為什麼做了昂貴的動作。
    pub notes: Vec<String>,
}

impl Plan {
    fn note(&mut self, s: &str) {
        let msg = s.to_string();
        if !self.notes.contains(&msg) {
            self.notes.push(msg);
        }
    }
}

/// 一棵在鎖外讀好、等著接上索引的子樹。
///
/// 內容是扁平化的：每筆都記著自己的「本地父編號」，編號 0 固定是子樹根自己。
/// 扁平化的目的，是讓真正接上索引的動作退化成純記憶體操作 —— 遞迴讀目錄那些
/// 系統呼叫全部發生在鎖外，寫鎖只負責把結果掛上去。
#[derive(Default)]
pub struct Subtree {
    /// (本地父編號, 名稱)；第 0 筆是子樹根。
    pub dirs: Vec<(u32, Vec<u8>)>,
    /// (本地父編號, 名稱, 是否為 symlink)
    pub files: Vec<(u32, Vec<u8>, bool)>,
}

/// 在鎖外遞迴讀出一整棵子樹。
///
/// 這是整條更新路徑上唯一會做大量 I/O 的地方，刻意設計成不碰任何共享狀態，
/// 這樣它跑多久都不會擋到查詢。
pub fn read_subtree(root_path: &[u8], root_name: &[u8]) -> Subtree {
    let mut t = Subtree::default();
    t.dirs.push((u32::MAX, root_name.to_vec()));

    // 以堆疊取代遞迴：新目錄可能很深（例如剛 clone 下來的專案）。
    let mut stack: Vec<(u32, Vec<u8>)> = vec![(0, root_path.to_vec())];
    let mut guard = 0usize;
    while let Some((local_id, path)) = stack.pop() {
        guard += 1;
        // 單次事件不該無限膨脹；超過就交給 daemon 的重建機制收拾。
        if guard > 200_000 {
            break;
        }
        let entries = match scan::list_dir(&path) {
            Ok(v) => v,
            Err(_) => continue,
        };
        for (name, is_dir, is_link) in entries {
            if is_dir {
                let mut child = path.clone();
                if !child.ends_with(b"/") {
                    child.push(b'/');
                }
                child.extend_from_slice(&name);
                t.dirs.push((local_id, name));
                stack.push(((t.dirs.len() - 1) as u32, child));
            } else {
                t.files.push((local_id, name, is_link));
            }
        }
    }
    t
}

impl Live {
    pub fn new(base: Index) -> Live {
        let nf = base.header().n_files as usize;
        let nd = base.header().n_dirs as usize;
        let lower: Vec<u8> = base
            .file_names()
            .iter()
            .map(|b| b.to_ascii_lowercase())
            .collect();
        Live {
            base: Arc::new(BaseData { index: base, lower }),
            add_lower: Vec::new(),
            dead_files: BitSet::new(nf),
            dead_dirs: BitSet::new(nd),
            add_names: Vec::new(),
            add_recs: Vec::new(),
            add_dir_names: Vec::new(),
            add_dir_nodes: Vec::new(),
            add_children: HashMap::new(),
            add_files: HashMap::new(),
            dead_add_files: BitSet::new(0),
            dead_add_dirs: BitSet::new(0),
            ignore_prefixes: Vec::new(),
        }
    }

    /// 設定要忽略事件的路徑前綴（例如索引檔自己所在的目錄）。
    pub fn ignore_path(&mut self, prefix: &[u8]) {
        self.ignore_prefixes.push(prefix.to_vec());
    }

    #[inline]
    fn ignored(&self, path: &[u8]) -> bool {
        self.ignore_prefixes.iter().any(|p| path.starts_with(p))
    }

    #[allow(dead_code)]
    pub fn base(&self) -> &Index {
        &self.base.index
    }

    fn n_base_dirs(&self) -> u32 {
        self.base.index.header().n_dirs as u32
    }
    fn n_base_files(&self) -> u32 {
        self.base.index.header().n_files as u32
    }

    /// delta 累積的項目數，用來決定何時該重建快照。
    pub fn delta_len(&self) -> usize {
        self.add_recs.len() + self.add_dir_nodes.len()
    }
    pub fn dead_len(&self) -> usize {
        self.dead_files.count() + self.dead_dirs.count()
    }

    pub fn total_files(&self) -> usize {
        self.n_base_files() as usize + self.add_recs.len()
            - self.dead_files.count()
            - self.dead_add_files.count()
    }
    pub fn total_dirs(&self) -> usize {
        self.n_base_dirs() as usize + self.add_dir_nodes.len()
            - self.dead_dirs.count()
            - self.dead_add_dirs.count()
    }

    // --- 統一的節點存取（自動分辨基底或 delta）---

    fn dir_alive(&self, id: u32) -> bool {
        let nb = self.n_base_dirs();
        if id < nb {
            !self.dead_dirs.get(id as usize)
        } else {
            !self.dead_add_dirs.get((id - nb) as usize)
        }
    }

    fn file_alive(&self, id: u32) -> bool {
        let nb = self.n_base_files();
        if id < nb {
            !self.dead_files.get(id as usize)
        } else {
            !self.dead_add_files.get((id - nb) as usize)
        }
    }

    pub fn dir_name(&self, id: u32) -> &[u8] {
        let nb = self.n_base_dirs();
        if id < nb {
            self.base.index.dir_name(id)
        } else {
            let n = &self.add_dir_nodes[(id - nb) as usize];
            let s = n.name_off as usize;
            &self.add_dir_names[s..s + n.name_len as usize]
        }
    }

    fn dir_parent(&self, id: u32) -> u32 {
        let nb = self.n_base_dirs();
        if id < nb {
            self.base.index.dir_nodes()[id as usize].parent
        } else {
            self.add_dir_nodes[(id - nb) as usize].parent
        }
    }

    pub fn file_name(&self, id: u32) -> &[u8] {
        let nb = self.n_base_files();
        if id < nb {
            self.base.index.file_name(id as usize)
        } else {
            let r = &self.add_recs[(id - nb) as usize];
            let s = r.name_off as usize;
            &self.add_names[s..s + r.name_len as usize]
        }
    }

    fn file_parent(&self, id: u32) -> u32 {
        let nb = self.n_base_files();
        if id < nb {
            self.base.index.file_recs()[id as usize].parent
        } else {
            self.add_recs[(id - nb) as usize].parent
        }
    }

    pub fn dir_path(&self, mut id: u32) -> Vec<u8> {
        let mut parts: Vec<&[u8]> = Vec::new();
        let mut guard = 0;
        while id != u32::MAX && guard < 256 {
            parts.push(self.dir_name(id));
            id = self.dir_parent(id);
            guard += 1;
        }
        let mut out = Vec::with_capacity(parts.iter().map(|p| p.len() + 1).sum());
        for part in parts.iter().rev() {
            if !out.is_empty() && !out.ends_with(b"/") {
                out.push(b'/');
            }
            out.extend_from_slice(part);
        }
        out
    }

    pub fn file_path(&self, id: u32) -> Vec<u8> {
        let mut p = self.dir_path(self.file_parent(id));
        if !p.ends_with(b"/") {
            p.push(b'/');
        }
        p.extend_from_slice(self.file_name(id));
        p
    }

    /// 某目錄底下所有還活著的子目錄。
    pub fn child_dirs(&self, parent: u32) -> Vec<u32> {
        let mut out: Vec<u32> = Vec::new();
        if parent < self.n_base_dirs() {
            out.extend(
                self.base
                    .index
                    .child_dirs(parent)
                    .into_iter()
                    .filter(|&id| id != parent && self.dir_alive(id)),
            );
        }
        if let Some(v) = self.add_children.get(&parent) {
            out.extend(v.iter().copied().filter(|&id| self.dir_alive(id)));
        }
        out
    }

    /// 某目錄底下所有還活著的檔案。
    pub fn files_in_dir(&self, parent: u32) -> Vec<u32> {
        let mut out: Vec<u32> = Vec::new();
        if parent < self.n_base_dirs() {
            for i in self.base.index.files_in_dir(parent) {
                if self.file_alive(i as u32) {
                    out.push(i as u32);
                }
            }
        }
        if let Some(v) = self.add_files.get(&parent) {
            let nb = self.n_base_files();
            out.extend(v.iter().map(|&i| i + nb).filter(|&id| self.file_alive(id)));
        }
        out
    }

    /// 由絕對路徑找出對應的目錄 id。
    pub fn lookup_dir(&self, path: &[u8]) -> Option<u32> {
        let root = self.base.index.root().to_vec();
        if !path.starts_with(&root) {
            return None;
        }
        let mut cur = 0u32;
        for seg in path[root.len()..].split(|&b| b == b'/') {
            if seg.is_empty() {
                continue;
            }
            let found = self
                .child_dirs(cur)
                .into_iter()
                .find(|&id| self.dir_name(id) == seg)?;
            cur = found;
        }
        Some(cur)
    }

    // --- delta 的寫入操作 ---

    fn push_dir(&mut self, name: &[u8], parent: u32) -> u32 {
        let off = self.add_dir_names.len() as u32;
        self.add_dir_names.extend_from_slice(name);
        self.add_dir_nodes.push(DirNode {
            name_off: off,
            parent,
            name_len: name.len() as u16,
            _pad: 0,
        });
        let id = self.n_base_dirs() + (self.add_dir_nodes.len() - 1) as u32;
        self.dead_add_dirs
            .bits
            .resize(self.add_dir_nodes.len().div_ceil(64).max(1), 0);
        self.add_children.entry(parent).or_default().push(id);
        id
    }

    fn push_file(&mut self, name: &[u8], parent: u32, is_link: bool) {
        let off = self.add_names.len() as u32;
        self.add_names.extend_from_slice(name);
        self.add_lower
            .extend(name.iter().map(|b| b.to_ascii_lowercase()));
        self.add_recs.push(FileRec {
            name_off: off,
            parent,
            name_len: name.len() as u16,
            flags: if is_link { FLAG_SYMLINK } else { 0 },
        });
        let local = (self.add_recs.len() - 1) as u32;
        self.dead_add_files
            .bits
            .resize(self.add_recs.len().div_ceil(64).max(1), 0);
        self.add_files.entry(parent).or_default().push(local);
    }

    fn kill_file(&mut self, id: u32) {
        let nb = self.n_base_files();
        if id < nb {
            self.dead_files.set(id as usize);
        } else {
            self.dead_add_files.set((id - nb) as usize);
        }
    }

    /// 標記整棵子樹為已刪除。
    fn remove_subtree(&mut self, id: u32) {
        let mut stack = vec![id];
        let mut guard = 0;
        while let Some(d) = stack.pop() {
            guard += 1;
            if guard > 2_000_000 {
                break;
            }
            for f in self.files_in_dir(d) {
                self.kill_file(f);
            }
            for c in self.child_dirs(d) {
                stack.push(c);
            }
            let nb = self.n_base_dirs();
            if d < nb {
                self.dead_dirs.set(d as usize);
            } else {
                self.dead_add_dirs.set((d - nb) as usize);
            }
        }
    }

    /// 第一階段（讀鎖）：把事件路徑換算成「需要重新列舉的目錄」。
    ///
    /// 不看事件旗標推斷發生了什麼 —— FSEvents 會把同一路徑的多個事件合併
    /// （同一筆可能同時是建立+刪除+修改），旗標無從得知最終狀態。一律重新
    /// 列舉該目錄再跟索引比對，才是可靠且冪等的做法。
    pub fn plan(&self, events: &[(Vec<u8>, u32)]) -> Plan {
        use crate::fsevents as fse;
        let mut plan = Plan::default();
        let mut wanted: Vec<Vec<u8>> = Vec::new();
        let root = self.base.index.root();

        for (path, flags) in events {
            if self.ignored(path) {
                continue;
            }
            // 整個索引真正失效的情況其實只有兩種：被監看的根目錄自己被搬走，
            // 或事件 id 回繞導致續傳點失去意義。
            if flags & (fse::EV_ROOT_CHANGED | fse::EV_IDS_WRAPPED) != 0 {
                plan.need_full = true;
                plan.note("根目錄變動或事件 id 回繞");
                continue;
            }

            // 掛載／卸載：只有發生在我們監看範圍內才有意義。macOS 會頻繁掛卸
            // Time Machine 的本機快照，若不分青紅皂白一律重建，daemon 會幾乎
            // 一直在做全量掃描。
            if flags & (fse::EV_MOUNT | fse::EV_UNMOUNT) != 0 {
                if path.starts_with(root) {
                    plan.rescan.push(path.clone());
                    plan.note("監看範圍內有磁碟區掛載／卸載");
                } else {
                    plan.note("忽略範圍外的磁碟區掛載／卸載");
                }
                continue;
            }

            // 事件被丟棄：只有「這個子樹」的內容不可信，重掃它就好，
            // 不需要動到整份索引。
            if flags & (fse::EV_MUST_SCAN_SUBDIRS | fse::EV_USER_DROPPED | fse::EV_KERNEL_DROPPED)
                != 0
            {
                plan.rescan.push(path.clone());
                let p = String::from_utf8_lossy(path).to_string();
                plan.note(&format!("事件溢位，需重掃子樹：{p}"));
                continue;
            }

            let parent = match path.iter().rposition(|&b| b == b'/') {
                Some(0) => b"/".to_vec(),
                Some(p) => path[..p].to_vec(),
                None => continue,
            };
            if !wanted.contains(&parent) {
                wanted.push(parent);
            }
            // 事件本身是目錄的話，它的內容也要一起對齊。
            if flags & fse::EV_ITEM_IS_DIR != 0 && !wanted.contains(path) {
                wanted.push(path.clone());
            }
        }

        for d in wanted {
            match self.resolve_or_ancestor(&d) {
                Some((id, p)) => {
                    if !plan.refresh.iter().any(|(i, _)| *i == id) {
                        plan.refresh.push((id, p));
                    }
                }
                None => continue,
            }
        }

        // 重掃目標換算成 dir_id；若整個根都要重掃，那跟全量重建是同一件事。
        for d in std::mem::take(&mut plan.rescan) {
            if let Some((id, p)) = self.resolve_or_ancestor(&d) {
                if id == 0 {
                    plan.need_full = true;
                } else if !plan.rescan_ids.iter().any(|(i, _)| *i == id) {
                    plan.rescan_ids.push((id, p));
                }
            }
        }
        plan
    }

    /// 把路徑解析成 dir_id；若該目錄還不在索引裡，就退回最近的已知祖先。
    fn resolve_or_ancestor(&self, path: &[u8]) -> Option<(u32, Vec<u8>)> {
        if let Some(id) = self.lookup_dir(path) {
            return Some((id, path.to_vec()));
        }
        let mut probe = path.to_vec();
        for _ in 0..64 {
            let cut = match probe.iter().rposition(|&b| b == b'/') {
                Some(0) | None => return None,
                Some(p) => p,
            };
            probe.truncate(cut);
            if let Some(id) = self.lookup_dir(&probe) {
                return Some((id, probe));
            }
        }
        None
    }

    /// 整棵子樹重建：先把舊的標記為刪除，再把鎖外讀好的新內容接回原位。
    pub fn rescan_subtree(&mut self, dir_id: u32, tree: &Subtree) {
        let parent = self.dir_parent(dir_id);
        self.remove_subtree(dir_id);
        self.graft(parent, tree);
    }

    /// 第三階段（寫鎖）：把鎖外讀到的目錄內容跟索引比對並套用差異。
    ///
    /// `entries` 為 `None` 代表該目錄已經不存在。回傳的是「新出現、還需要往下
    /// 掃描」的子目錄名稱 —— 掃描本身留給呼叫端在鎖外做。
    pub fn apply_listing(
        &mut self,
        dir_id: u32,
        entries: Option<Vec<crate::scan::DirEntry>>,
    ) -> Vec<Vec<u8>> {
        let entries = match entries {
            Some(v) => v,
            None => {
                if dir_id != 0 {
                    self.remove_subtree(dir_id);
                }
                return Vec::new();
            }
        };

        let mut cur_files: HashMap<Vec<u8>, u32> = self
            .files_in_dir(dir_id)
            .into_iter()
            .map(|id| (self.file_name(id).to_vec(), id))
            .collect();
        let mut cur_dirs: HashMap<Vec<u8>, u32> = self
            .child_dirs(dir_id)
            .into_iter()
            .map(|id| (self.dir_name(id).to_vec(), id))
            .collect();

        let mut fresh_dirs = Vec::new();
        for (name, is_dir, is_link) in entries {
            if is_dir {
                if cur_dirs.remove(&name).is_none() {
                    fresh_dirs.push(name);
                }
            } else if cur_files.remove(&name).is_none() {
                self.push_file(&name, dir_id, is_link);
            }
        }

        // 沒被實際內容消掉的，就是已經消失的項目。
        for (_, id) in cur_files {
            self.kill_file(id);
        }
        for (_, id) in cur_dirs {
            self.remove_subtree(id);
        }
        fresh_dirs
    }

    /// 第五階段（寫鎖）：把鎖外讀好的整棵子樹接上索引，純記憶體操作。
    pub fn graft(&mut self, parent: u32, tree: &Subtree) {
        let mut ids: Vec<u32> = Vec::with_capacity(tree.dirs.len());
        for (i, (local_parent, name)) in tree.dirs.iter().enumerate() {
            let p = if i == 0 {
                parent
            } else {
                match ids.get(*local_parent as usize) {
                    Some(&v) => v,
                    None => continue,
                }
            };
            ids.push(self.push_dir(name, p));
        }
        for (local_parent, name, is_link) in tree.files.iter() {
            if let Some(&p) = ids.get(*local_parent as usize) {
                self.push_file(name, p, *is_link);
            }
        }
    }

    /// 掃過所有還活著的項目做比對。
    ///
    /// 基底部分是連續的記憶體，掃描維持循序；delta 通常很小，附帶掃完即可。
    pub fn search(&self, q: &crate::search::Query, limit: usize) -> (Vec<Hit>, usize) {
        let nthreads = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(8);

        // 基底佔了絕大多數項目，必須並行掃 —— 這裡若退回單執行緒，延遲會直接
        // 差上一個數量級。delta 通常只有幾千筆，附帶在最後單執行緒掃完即可。
        //
        // 掃描不是逐個檔名比對，而是把整個小寫 arena 當成一大塊 haystack 交給
        // 向量化的 memmem，命中後再二分回推是哪一筆紀錄。好處有二：記憶體存取
        // 完全循序，而且比對本身走 SIMD，不必逐 byte 做 case folding。
        let recs = self.base.index.file_recs();
        let names = self.base.index.file_names();
        let lower = &self.base.lower[..];
        let (anchor_idx, anchor) = match q.anchor() {
            Some(a) => a,
            None => return (Vec::new(), 0),
        };
        let alen = anchor.len();
        let chunk = lower.len().div_ceil(nthreads.max(1));

        let parts: Vec<(Vec<u32>, usize)> = std::thread::scope(|s| {
            let mut handles = Vec::with_capacity(nthreads);
            for t in 0..nthreads {
                let start = t * chunk;
                if start >= lower.len() {
                    break;
                }
                // 尾端多延伸 alen-1 個 byte，才不會漏掉正好跨在分段邊界上的命中；
                // 命中位置若落在本段之外就交給下一段處理，因此不會重複計算。
                let end = (start + chunk + alen.saturating_sub(1)).min(lower.len());
                let finder = memchr::memmem::Finder::new(anchor);
                handles.push(s.spawn(move || {
                    let mut local: Vec<u32> = Vec::new();
                    let mut count = 0usize;
                    let mut last_rec = usize::MAX;
                    for pos in finder
                        .find_iter(&lower[start..end])
                        .map(|p| p + start)
                        .take_while(|&p| p < start + chunk)
                    {
                        // name_off 隨紀錄遞增，二分即可回推所屬檔案。
                        let i = recs.partition_point(|r| r.name_off as usize <= pos) - 1;
                        // 同一個檔名可能被命中多次，只算一次。
                        if i == last_rec {
                            continue;
                        }
                        let r = &recs[i];
                        let off = r.name_off as usize;
                        let nlen = r.name_len as usize;
                        // 命中必須完整落在這個檔名內，不能跨過名稱邊界。
                        if pos + alen > off + nlen {
                            continue;
                        }
                        last_rec = i;
                        if self.dead_files.get(i) {
                            continue;
                        }
                        if !q.matches_rest(&names[off..off + nlen], anchor_idx) {
                            continue;
                        }
                        count += 1;
                        if local.len() < limit {
                            local.push(i as u32);
                        }
                    }
                    (local, count)
                }));
            }
            handles.into_iter().map(|h| h.join().unwrap()).collect()
        });

        let mut total: usize = parts.iter().map(|p| p.1).sum();
        let mut hits: Vec<Hit> = Vec::new();
        for (local, _) in parts.iter() {
            for &i in local.iter() {
                if hits.len() >= limit {
                    break;
                }
                hits.push(Hit::File(i));
            }
        }

        // 基底的目錄（數量比檔案少一個數量級，單執行緒即可）
        let nodes = self.base.index.dir_nodes();
        for (id, node) in nodes.iter().enumerate() {
            if node.name_len == 0 || self.dead_dirs.get(id) {
                continue;
            }
            if q.matches(self.base.index.dir_name(id as u32)) {
                total += 1;
                if hits.len() < limit {
                    hits.push(Hit::Dir(id as u32));
                }
            }
        }

        // delta 的目錄與檔案
        let nb_dirs = self.n_base_dirs();
        for i in 0..self.add_dir_nodes.len() {
            if self.dead_add_dirs.get(i) {
                continue;
            }
            let id = nb_dirs + i as u32;
            if q.matches(self.dir_name(id)) {
                total += 1;
                if hits.len() < limit {
                    hits.push(Hit::Dir(id));
                }
            }
        }
        let nb_files = self.n_base_files();
        for i in 0..self.add_recs.len() {
            if self.dead_add_files.get(i) {
                continue;
            }
            let id = nb_files + i as u32;
            if q.matches(self.file_name(id)) {
                total += 1;
                if hits.len() < limit {
                    hits.push(Hit::File(id));
                }
            }
        }

        (hits, total)
    }

    pub fn path_of(&self, h: Hit) -> Vec<u8> {
        match h {
            Hit::File(i) => self.file_path(i),
            Hit::Dir(i) => {
                let mut p = self.dir_path(i);
                p.push(b'/');
                p
            }
        }
    }
}
