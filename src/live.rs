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
    ///
    /// 顯示順序與配額分配跟 `search.rs` 的離線路徑一致：**目錄先取，檔案補滿
    /// 剩餘額度**。兩邊若各行其是，同一道指令會因為 daemon 的起停而給出不同
    /// 結果 —— 例如 `ml node_modules -n 6` 原本在 daemon 路徑下六筆全是檔案，
    /// 一個目錄都排不進來。
    pub fn search(&self, q: &crate::search::Query, limit: usize) -> (Vec<Hit>, usize) {
        if q.is_empty() {
            return (Vec::new(), 0);
        }

        let mut total = 0usize;
        let mut dirs: Vec<Hit> = Vec::new();

        if q.wants_dirs() {
            // 基底的目錄（數量比檔案少一個數量級，單執行緒即可）
            let nodes = self.base.index.dir_nodes();
            for (id, node) in nodes.iter().enumerate() {
                if node.name_len == 0 || self.dead_dirs.get(id) {
                    continue;
                }
                let dname = self.base.index.dir_name(id as u32);
                if !q.matches(dname) {
                    continue;
                }
                if q.has_path_terms() {
                    // 先用便宜的篩子擋掉絕大多數，否則 26 萬個目錄每一個都要
                    // 往上爬一次組出完整路徑。
                    if !q.name_prefilter(dname) {
                        continue;
                    }
                    // 目錄的完整路徑就是它自己，直接比對即可。
                    if !q.path_ok_dir(&self.dir_path(id as u32)) {
                        continue;
                    }
                }
                total += 1;
                if dirs.len() < limit {
                    dirs.push(Hit::Dir(id as u32));
                }
            }

            // delta 的目錄
            let nb_dirs = self.n_base_dirs();
            for i in 0..self.add_dir_nodes.len() {
                if self.dead_add_dirs.get(i) {
                    continue;
                }
                let id = nb_dirs + i as u32;
                if !q.matches(self.dir_name(id)) {
                    continue;
                }
                if q.has_path_terms() {
                    if !q.name_prefilter(self.dir_name(id)) {
                        continue;
                    }
                    if !q.path_ok_dir(&self.dir_path(id)) {
                        continue;
                    }
                }
                total += 1;
                if dirs.len() < limit {
                    dirs.push(Hit::Dir(id));
                }
            }
        }

        let mut files: Vec<Hit> = Vec::new();

        if q.wants_files() {
            let (base_hits, base_total) = self.search_base_files(q, limit);
            total += base_total;
            files.extend(base_hits.into_iter().map(Hit::File));

            // delta 的檔案
            let nb_files = self.n_base_files();
            let mut buf: Vec<u8> = Vec::new();
            for i in 0..self.add_recs.len() {
                if self.dead_add_files.get(i) {
                    continue;
                }
                let id = nb_files + i as u32;
                if !q.matches(self.file_name(id)) {
                    continue;
                }
                if q.has_path_terms() {
                    if !q.name_prefilter(self.file_name(id)) {
                        continue;
                    }
                    let dir_path = self.dir_path(self.file_parent(id));
                    if !q.path_ok_file(&dir_path, self.file_name(id), &mut buf) {
                        continue;
                    }
                }
                total += 1;
                if files.len() < limit {
                    files.push(Hit::File(id));
                }
            }
        }

        // limit 是「總共顯示幾筆」，不是兩類各自的上限。
        dirs.truncate(limit);
        files.truncate(limit.saturating_sub(dirs.len()));
        let mut hits = dirs;
        hits.append(&mut files);
        (hits, total)
    }

    /// 基底檔案的比對，回傳（命中的 file id，命中總數）。
    ///
    /// 有檔名關鍵字時走 memmem 錨點；只有路徑關鍵字（例如 `ml codes/tool/`）時
    /// 沒有錨點可用，退回逐筆線性掃描。
    fn search_base_files(&self, q: &crate::search::Query, limit: usize) -> (Vec<u32>, usize) {
        let nthreads = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(8);

        let recs = self.base.index.file_recs();
        let names = self.base.index.file_names();

        // 基底佔了絕大多數項目，必須並行掃 —— 這裡若退回單執行緒，延遲會直接
        // 差上一個數量級。
        //
        // 有錨點時，掃描不是逐個檔名比對，而是把整個小寫 arena 當成一大塊
        // haystack 交給向量化的 memmem，命中後再二分回推是哪一筆紀錄。好處有二：
        // 記憶體存取完全循序，而且比對本身走 SIMD，不必逐 byte 做 case folding。
        let parts: Vec<(Vec<u32>, usize)> = match q.anchor() {
            Some(anchor) => {
                let lower = &self.base.lower[..];
                let alen = anchor.len();
                let chunk = lower.len().div_ceil(nthreads.max(1));
                std::thread::scope(|s| {
                    let mut handles = Vec::with_capacity(nthreads);
                    for t in 0..nthreads {
                        let start = t * chunk;
                        if start >= lower.len() {
                            break;
                        }
                        // 尾端多延伸 alen-1 個 byte，才不會漏掉正好跨在分段邊界上的
                        // 命中；命中位置若落在本段之外就交給下一段處理，因此不會
                        // 重複計算。
                        let end = (start + chunk + alen.saturating_sub(1)).min(lower.len());
                        let finder = memchr::memmem::Finder::new(anchor);
                        handles.push(s.spawn(move || {
                            let mut local: Vec<u32> = Vec::new();
                            let mut count = 0usize;
                            let mut last_rec = usize::MAX;
                            // 紀錄依 parent 排序，同一個目錄的檔案是連續的，
                            // 路徑組一次就好。
                            let mut cached_parent = u32::MAX;
                            let mut cached_path: Vec<u8> = Vec::new();
                            let mut buf: Vec<u8> = Vec::new();
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
                                let name = &names[off..off + nlen];
                                // 錨點只是縮小候選，沒有驗證任何關鍵字 ——
                                // 萬用字元的錨點只是它尾端的字面片段 —— 所以
                                // 這裡要做完整比對，不能只驗「其餘關鍵字」。
                                if !q.matches(name) {
                                    continue;
                                }
                                if q.has_path_terms() {
                                    if !q.name_prefilter(name) {
                                        continue;
                                    }
                                    if r.parent != cached_parent {
                                        cached_parent = r.parent;
                                        cached_path = self.base.index.dir_path(r.parent);
                                    }
                                    if !q.path_ok_file(&cached_path, name, &mut buf) {
                                        continue;
                                    }
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
                })
            }
            None => {
                // 沒有可當錨點的子字串關鍵字：memmem 無從下手，改逐筆掃。
                // 注意這裡仍可能有檔名關鍵字 —— `-b '*.pdf'` 就是這種情況，
                // 萬用字元當不了錨點但仍要比對檔名。漏掉這一步會讓整個索引
                // 都被當成命中。
                let n = recs.len();
                let chunk = n.div_ceil(nthreads.max(1));
                std::thread::scope(|s| {
                    let mut handles = Vec::with_capacity(nthreads);
                    for t in 0..nthreads {
                        let start = t * chunk;
                        if start >= n {
                            break;
                        }
                        let end = (start + chunk).min(n);
                        handles.push(s.spawn(move || {
                            let mut local: Vec<u32> = Vec::new();
                            let mut count = 0usize;
                            let mut cached_parent = u32::MAX;
                            let mut cached_path: Vec<u8> = Vec::new();
                            let mut buf: Vec<u8> = Vec::new();
                            for (k, r) in recs[start..end].iter().enumerate() {
                                let i = start + k;
                                if self.dead_files.get(i) {
                                    continue;
                                }
                                let off = r.name_off as usize;
                                let name = &names[off..off + r.name_len as usize];
                                if !q.matches(name) {
                                    continue;
                                }
                                if !q.name_prefilter(name) {
                                    continue;
                                }
                                // 過了篩子才組目錄路徑。紀錄依 parent 排序，
                                // 同一個目錄只會組一次。
                                if r.parent != cached_parent {
                                    cached_parent = r.parent;
                                    cached_path = self.base.index.dir_path(r.parent);
                                }
                                if !q.path_ok_file(&cached_path, name, &mut buf) {
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
                })
            }
        };

        let total: usize = parts.iter().map(|p| p.1).sum();
        let mut out: Vec<u32> = Vec::new();
        for (local, _) in parts.iter() {
            if out.len() >= limit {
                break;
            }
            let take = (limit - out.len()).min(local.len());
            out.extend_from_slice(&local[..take]);
        }
        (out, total)
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::search::{Options, Query, TypeFilter};
    use std::fs;
    use std::path::{Path, PathBuf};

    /// 測試用的查詢建構，省得每次都寫一長串 Options。
    fn q(s: &str, whole_path: bool, tf: TypeFilter) -> Query {
        Query::parse_opts(
            s,
            Options {
                whole_path,
                basename: false,
                type_filter: Some(tf),
            },
        )
    }

    /// `-b`：所有關鍵字都只比對檔名。
    fn q_basename(s: &str, tf: TypeFilter) -> Query {
        Query::parse_opts(
            s,
            Options {
                whole_path: false,
                basename: true,
                type_filter: Some(tf),
            },
        )
    }

    /// 在暫存目錄裡造一棵固定的樹，回傳根目錄路徑。
    ///
    /// 測試刻意走真正的 scan → build → Index → Live 這條路，而不是手工拼索引
    /// 結構：daemon 端最容易出錯的就是 name arena 的位移與 parent 排序，手工
    /// 造資料會把這些前提一起假設掉。
    fn make_tree(tag: &str) -> PathBuf {
        let root =
            std::env::temp_dir().join(format!("mylocate-test-{}-{}", std::process::id(), tag));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("foo")).unwrap();
        fs::create_dir_all(root.join("keep/deep")).unwrap();
        fs::create_dir_all(root.join("other")).unwrap();
        fs::write(root.join("foo.txt"), b"x").unwrap();
        fs::write(root.join("foo/note.txt"), b"x").unwrap();
        fs::write(root.join("keep/deep/README.md"), b"x").unwrap();
        fs::write(root.join("other/README.md"), b"x").unwrap();
        root
    }

    fn build_index(root: &Path) -> Vec<u8> {
        let scanned = crate::scan::scan(root.to_str().unwrap(), 1).unwrap();
        crate::index::build(&scanned, 0, [0u8; 16], 0)
    }

    fn live_of(bytes: &[u8]) -> Live {
        Live::new(crate::index::Index::from_vec(bytes.to_vec()).unwrap())
    }

    /// daemon 路徑的結果（相對於根目錄，方便斷言）。
    fn live_paths(l: &Live, root: &Path, q: &Query, limit: usize) -> (Vec<String>, usize) {
        let (hits, total) = l.search(q, limit);
        let prefix = format!("{}/", root.display());
        let v = hits
            .iter()
            .map(|h| {
                String::from_utf8_lossy(&l.path_of(*h))
                    .replace(&prefix, "")
                    .to_string()
            })
            .collect();
        (v, total)
    }

    /// 離線路徑的結果，格式與 `live_paths` 相同。
    fn index_paths(
        idx: &crate::index::Index,
        root: &Path,
        q: &Query,
        limit: usize,
    ) -> (Vec<String>, usize) {
        let h = crate::search::search(idx, q, limit, 1);
        let prefix = format!("{}/", root.display());
        let mut v: Vec<String> = h
            .dirs
            .iter()
            .map(|&d| {
                format!("{}/", String::from_utf8_lossy(&idx.dir_path(d))).replace(&prefix, "")
            })
            .collect();
        v.extend(h.files.iter().map(|&f| {
            String::from_utf8_lossy(&idx.file_path(f as usize))
                .replace(&prefix, "")
                .to_string()
        }));
        (v, h.total)
    }

    /// 目錄優先吃 limit 配額。
    ///
    /// 這正是兩條路徑原本不一致的地方：daemon 端先塞檔案，`ml node_modules -n 6`
    /// 六筆全是檔案，想找的目錄一個都排不進來。
    #[test]
    fn dirs_take_the_limit_first() {
        let root = make_tree("dirs-first");
        let bytes = build_index(&root);
        let live = live_of(&bytes);
        let q = q("foo", false, TypeFilter::All);

        let (paths, total) = live_paths(&live, &root, &q, 1);
        assert_eq!(paths, vec!["foo/"], "limit 1 時應該先給目錄");
        // 命中的是目錄 foo/ 與檔案 foo.txt；foo/note.txt 的檔名不含 foo，
        // 關鍵字不比對路徑時本來就不算。
        assert_eq!(total, 2);

        let _ = fs::remove_dir_all(&root);
    }

    /// 型別過濾。
    #[test]
    fn type_filter_selects_kind() {
        let root = make_tree("type-filter");
        let bytes = build_index(&root);
        let live = live_of(&bytes);

        let only_dirs = q("foo", false, TypeFilter::Dirs);
        let (paths, total) = live_paths(&live, &root, &only_dirs, 10);
        assert_eq!(paths, vec!["foo/"]);
        assert_eq!(total, 1);

        let only_files = q("foo", false, TypeFilter::Files);
        let (mut paths, total) = live_paths(&live, &root, &only_files, 10);
        paths.sort();
        assert_eq!(paths, vec!["foo.txt"]);
        assert_eq!(total, 1);

        let _ = fs::remove_dir_all(&root);
    }

    /// 含斜線的關鍵字改比對完整路徑，用來縮小範圍。
    #[test]
    fn path_term_narrows_by_directory() {
        let root = make_tree("path-term");
        let bytes = build_index(&root);
        let live = live_of(&bytes);

        let q = q("keep/deep README", false, TypeFilter::All);
        let (paths, total) = live_paths(&live, &root, &q, 10);
        assert_eq!(paths, vec!["keep/deep/README.md"]);
        assert_eq!(total, 1, "other/README.md 不在 keep/deep 底下，不該算進來");

        let _ = fs::remove_dir_all(&root);
    }

    /// 路徑關鍵字跨在目錄與檔名的邊界上。
    #[test]
    fn path_term_across_boundary() {
        let root = make_tree("boundary");
        let bytes = build_index(&root);
        let live = live_of(&bytes);

        let q = q("deep/readme", false, TypeFilter::All);
        let (paths, total) = live_paths(&live, &root, &q, 10);
        assert_eq!(paths, vec!["keep/deep/README.md"]);
        assert_eq!(total, 1);

        let _ = fs::remove_dir_all(&root);
    }

    /// 只有路徑關鍵字時沒有 memmem 錨點可用，會走線性掃描那條分支。
    #[test]
    fn path_only_query_uses_linear_scan() {
        let root = make_tree("path-only");
        let bytes = build_index(&root);
        let live = live_of(&bytes);

        let q = q("keep/deep", false, TypeFilter::All);
        let (mut paths, total) = live_paths(&live, &root, &q, 10);
        paths.sort();
        assert_eq!(paths, vec!["keep/deep/", "keep/deep/README.md"]);
        assert_eq!(total, 2);

        let _ = fs::remove_dir_all(&root);
    }

    /// 萬用字元要整段對齊完整路徑，不是子字串。
    #[test]
    fn glob_anchors_the_extension() {
        let root = make_tree("glob");
        fs::write(root.join("keep/report.pdf.bak"), b"x").unwrap();
        fs::write(root.join("keep/report.pdf"), b"x").unwrap();
        let bytes = build_index(&root);
        let live = live_of(&bytes);

        let (paths, total) = live_paths(&live, &root, &q("*.pdf", false, TypeFilter::All), 10);
        assert_eq!(paths, vec!["keep/report.pdf"], "report.pdf.bak 不該命中");
        assert_eq!(total, 1);

        // 子字串比對就會把 .bak 那筆也撈進來，兩者的差別正是加 glob 的理由
        let (mut paths, _) = live_paths(&live, &root, &q(".pdf", false, TypeFilter::All), 10);
        paths.sort();
        assert_eq!(paths, vec!["keep/report.pdf", "keep/report.pdf.bak"]);

        let _ = fs::remove_dir_all(&root);
    }

    /// `-b` 搭配萬用字元：沒有錨點可用，但檔名關鍵字仍然必須生效。
    ///
    /// 這是實測抓到的迴歸 —— daemon 的線性掃描分支原本假設「沒有錨點就等於
    /// 沒有檔名關鍵字」，於是 `ml -b '*.pdf'` 把整個索引 226 萬筆全當成命中。
    #[test]
    fn basename_glob_without_anchor_still_filters() {
        let root = make_tree("basename-glob");
        fs::write(root.join("keep/report.pdf"), b"x").unwrap();
        let bytes = build_index(&root);
        let live = live_of(&bytes);
        let idx = crate::index::Index::from_vec(bytes.clone()).unwrap();

        let query = q_basename("*.pdf", TypeFilter::All);
        let (paths, total) = live_paths(&live, &root, &query, 10);
        assert_eq!(paths, vec!["keep/report.pdf"]);
        assert_eq!(total, 1, "不該把整個索引都算成命中");

        // 離線路徑本來就是對的，順便確認兩邊一致
        let (paths2, total2) = index_paths(&idx, &root, &query, 10);
        assert_eq!(paths, paths2);
        assert_eq!(total, total2);

        let _ = fs::remove_dir_all(&root);
    }

    /// 兩條搜尋路徑必須給出一樣的結果。
    ///
    /// 這是整組測試的重點：daemon 起停不該改變任何一道指令的輸出。
    #[test]
    fn daemon_and_offline_paths_agree() {
        let root = make_tree("agree");
        let bytes = build_index(&root);
        let live = live_of(&bytes);
        let idx = crate::index::Index::from_vec(bytes.clone()).unwrap();

        let cases: Vec<(&str, bool, TypeFilter, usize)> = vec![
            ("foo", false, TypeFilter::All, 1),
            ("foo", false, TypeFilter::All, 10),
            ("readme", false, TypeFilter::All, 10),
            ("foo", false, TypeFilter::Dirs, 10),
            ("foo", false, TypeFilter::Files, 10),
            ("keep/deep README", false, TypeFilter::All, 10),
            ("deep/readme", false, TypeFilter::All, 10),
            ("keep/deep", false, TypeFilter::All, 10),
            ("keep deep", true, TypeFilter::All, 10),
            ("txt", false, TypeFilter::All, 1),
            ("*.txt", false, TypeFilter::All, 10),
            ("*.md", false, TypeFilter::All, 10),
            ("*/keep/*", false, TypeFilter::All, 10),
            ("*.md", false, TypeFilter::Files, 10),
            ("*keep*", false, TypeFilter::Dirs, 10),
            ("f?o.txt", false, TypeFilter::All, 10),
            ("*.md keep/", false, TypeFilter::All, 10),
        ];

        for (s, force_path, tf, limit) in cases {
            let q = q(s, force_path, tf);
            let (mut a, ta) = live_paths(&live, &root, &q, limit);
            let (mut b, tb) = index_paths(&idx, &root, &q, limit);
            a.sort();
            b.sort();
            assert_eq!(a, b, "查詢「{s}」在兩條路徑下結果不同");
            assert_eq!(ta, tb, "查詢「{s}」在兩條路徑下命中總數不同");
        }

        // -b 的案例另外跑一輪，涵蓋「有檔名關鍵字但沒有錨點」這條分支
        for s in ["*.md", "*.txt", "f?o.txt", "readme"] {
            let q = q_basename(s, TypeFilter::All);
            let (mut a, ta) = live_paths(&live, &root, &q, 10);
            let (mut b, tb) = index_paths(&idx, &root, &q, 10);
            a.sort();
            b.sort();
            assert_eq!(a, b, "查詢「-b {s}」在兩條路徑下結果不同");
            assert_eq!(ta, tb, "查詢「-b {s}」在兩條路徑下命中總數不同");
        }

        let _ = fs::remove_dir_all(&root);
    }
}
