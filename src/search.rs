//! 搜尋引擎。
//!
//! 刻意不建倒排索引／trigram 之類的結構 —— Everything 本身也沒有，它就是對每個
//! 檔名做多執行緒字串比對。對百萬級的短字串來說，線性掃描是記憶體頻寬受限的
//! 問題，額外的索引結構只會膨脹記憶體並拖慢更新，反而得不償失。
//!
//! 這裡同時是「查詢語意」的單一定義處。實際掃描有兩份實作 —— daemon 在跑時走
//! `live.rs`（記憶體快照、memmem 向量化），沒跑時走本檔的 [`search`]（mmap 索引、
//! 逐筆比對）—— 但兩者的比對規則、型別過濾與「目錄優先吃 limit 配額」的顯示
//! 順序都必須由本檔的 [`Query`] 決定，否則同一道指令會因為 daemon 的起停而給出
//! 不同結果。
//!
//! # 查詢語意
//!
//! 關鍵字之間一律是 AND，一律不分大小寫。每個關鍵字各自決定怎麼比對：
//!
//! | 關鍵字長相 | 比對方式 | 比對對象 |
//! |---|---|---|
//! | 一般字串 | 子字串 | 檔名 |
//! | 含 `/` | 子字串 | 完整路徑 |
//! | 含 `*`、`?`、`[` | 萬用字元，整段對齊 | 完整路徑 |
//!
//! 萬用字元的規則跟 locate/plocate 一致：`*` 會跨過斜線，而且必須整段命中，
//! 所以 `*.pdf` 能把副檔名釘在結尾。`-b` 可以把全部關鍵字改成只比對檔名，
//! `-w` 則相反，全部改成比對完整路徑。

use crate::index::Index;

/// 型別過濾：對應 `-t d` / `-t f`。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TypeFilter {
    /// 目錄與檔案都要（預設）。
    All,
    /// 只要目錄。
    Dirs,
    /// 只要檔案。
    Files,
}

/// 解析查詢時的選項，對應命令列旗標。
#[derive(Clone, Copy, Default)]
pub struct Options {
    /// `-w` / `--path`：所有關鍵字都比對完整路徑。
    pub whole_path: bool,
    /// `-b`：所有關鍵字都只比對檔名，含斜線與萬用字元的也一樣。
    pub basename: bool,
    /// `-t d` / `-t f`。
    pub type_filter: Option<TypeFilter>,
}

/// 單一關鍵字的比對方式。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Kind {
    /// 子字串，出現在任何位置都算。
    Substr,
    /// 萬用字元，必須整段對齊。
    Glob,
}

#[derive(Clone)]
struct Term {
    /// 已轉小寫的關鍵字。
    pat: Vec<u8>,
    kind: Kind,
    /// 萬用字元樣式尾端的字面片段，可以拿來當 memmem 的錨點。
    ///
    /// glob 是整段對齊的，所以「樣式結尾的字面片段」必然是被比對字串的結尾。
    /// 只要它不含斜線，就一定完整落在這個項目自己的名字裡 —— 於是
    /// `*.pdf` 可以先用 `.pdf` 掃檔名 arena 篩掉 99% 的候選，再對少數命中
    /// 做整段對齊。樣式結尾是 `*`、`?` 或字元集合時沒有這個片段，只能線性掃。
    glob_anchor: Option<Vec<u8>>,
}

impl Term {
    #[inline]
    fn hits(&self, hay: &[u8]) -> bool {
        match self.kind {
            Kind::Substr => contains_ci(hay, &self.pat),
            Kind::Glob => glob_match(&self.pat, hay),
        }
    }
}

/// 找出 `[...]` 的收尾位置，回傳 `]` 之後的索引；沒有收尾就回 None。
fn class_end(pat: &[u8], start: usize) -> Option<usize> {
    let mut i = start + 1;
    if i < pat.len() && (pat[i] == b'!' || pat[i] == b'^') {
        i += 1;
    }
    let mut first = true;
    while i < pat.len() {
        if pat[i] == b']' && !first {
            return Some(i + 1);
        }
        first = false;
        i += 1;
    }
    None
}

/// 取出樣式**尾端**的字面片段：樣式必須以它結尾，後面不能有任何萬用字元。
///
/// 回傳 None 代表樣式以萬用字元收尾，或尾端片段含斜線（那就可能跨到目錄
/// 名去，不保證出現在項目自己的名字裡），兩種情況都不能拿來當錨點。
fn trailing_literal(pat: &[u8]) -> Option<Vec<u8>> {
    let mut cur: Vec<u8> = Vec::new();
    let mut i = 0;
    while i < pat.len() {
        match pat[i] {
            b'*' | b'?' => {
                cur.clear();
                i += 1;
            }
            b'[' => match class_end(pat, i) {
                Some(next) => {
                    cur.clear();
                    i = next;
                }
                None => {
                    // 沒有收尾的 [ 退化成一般字元
                    cur.push(b'[');
                    i += 1;
                }
            },
            b'\\' if i + 1 < pat.len() => {
                cur.push(pat[i + 1]);
                i += 2;
            }
            c => {
                cur.push(c);
                i += 1;
            }
        }
    }
    if cur.is_empty() || cur.contains(&b'/') {
        None
    } else {
        Some(cur)
    }
}

/// 關鍵字含這些字元就當萬用字元處理，規則與 locate 相同。
fn has_glob_chars(t: &[u8]) -> bool {
    t.iter().any(|&b| b == b'*' || b == b'?' || b == b'[')
}

/// 解析後的查詢。
pub struct Query {
    /// 比對檔名的關鍵字。
    terms: Vec<Term>,
    /// 比對完整路徑的關鍵字。
    path_terms: Vec<Term>,
    type_filter: TypeFilter,
    /// 路徑關鍵字裡子字串類的最大長度，用來決定跨目錄邊界比對窗要往前取多少。
    max_path_substr: usize,
    /// 路徑關鍵字裡有沒有萬用字元。有的話比對窗不夠用，必須組出完整路徑。
    path_has_glob: bool,
}

impl Query {
    /// 不帶任何旗標的解析。目前只有測試在用，正式路徑一律走 `parse_opts`。
    #[allow(dead_code)]
    pub fn parse(s: &str) -> Query {
        Query::parse_opts(s, Options::default())
    }

    pub fn parse_opts(s: &str, opts: Options) -> Query {
        let mut terms: Vec<Term> = Vec::new();
        let mut path_terms: Vec<Term> = Vec::new();
        for t in s.split_whitespace() {
            let pat: Vec<u8> = t.bytes().map(|b| b.to_ascii_lowercase()).collect();
            let kind = if has_glob_chars(&pat) {
                Kind::Glob
            } else {
                Kind::Substr
            };
            // 預設的歸類：萬用字元與含斜線的比對完整路徑，其餘比對檔名。
            // 檔名本身不可能含斜線，所以關鍵字一旦出現斜線，使用者要的必然是
            // 「某個位置底下」而不是「名字長這樣」。
            let to_path = if opts.basename {
                false
            } else {
                opts.whole_path || kind == Kind::Glob || pat.contains(&b'/')
            };
            let glob_anchor = if kind == Kind::Glob {
                trailing_literal(&pat)
            } else {
                None
            };
            let term = Term {
                pat,
                kind,
                glob_anchor,
            };
            if to_path {
                path_terms.push(term);
            } else {
                terms.push(term);
            }
        }
        let max_path_substr = path_terms
            .iter()
            .filter(|t| t.kind == Kind::Substr)
            .map(|t| t.pat.len())
            .max()
            .unwrap_or(0);
        let path_has_glob = path_terms.iter().any(|t| t.kind == Kind::Glob);
        Query {
            terms,
            path_terms,
            type_filter: opts.type_filter.unwrap_or(TypeFilter::All),
            max_path_substr,
            path_has_glob,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.terms.is_empty() && self.path_terms.is_empty()
    }

    /// 有沒有檔名關鍵字。掃描端是用 `anchor()` 回傳 None 判斷有沒有錨點的，
    /// 這個方法留給測試表達意圖。
    #[allow(dead_code)]
    pub fn has_name_terms(&self) -> bool {
        !self.terms.is_empty()
    }

    pub fn has_path_terms(&self) -> bool {
        !self.path_terms.is_empty()
    }

    pub fn wants_dirs(&self) -> bool {
        self.type_filter != TypeFilter::Files
    }

    pub fn wants_files(&self) -> bool {
        self.type_filter != TypeFilter::Dirs
    }

    /// 名稱是否命中此查詢（所有檔名關鍵字都要出現）。
    ///
    /// 查詢若只有路徑關鍵字，這裡對任何名稱都成立，過濾交給 `path_ok_*`。
    #[inline]
    pub fn matches(&self, name: &[u8]) -> bool {
        self.terms.iter().all(|t| t.hits(name))
    }

    /// 掃描用的錨點：一段必然出現在**項目名稱**裡的字面片段，愈長愈好。
    ///
    /// 可以當錨點的有兩種。一是比對檔名的子字串關鍵字，理由顯然。二是萬用
    /// 字元樣式尾端的字面片段 —— 不論它比對的是檔名還是完整路徑，因為 glob
    /// 整段對齊，那段字面必然是結尾，不含斜線就代表它落在名稱內。
    ///
    /// 比對完整路徑的**子字串**關鍵字不能當錨點：`codes/tool` 可能整段都在
    /// 目錄路徑裡，掃檔名 arena 永遠掃不到。
    ///
    /// 錨點只用來縮小候選範圍，不代表該關鍵字已經驗過，呼叫端仍要做完整比對。
    pub fn anchor(&self) -> Option<&[u8]> {
        let from_name = self.terms.iter().filter_map(|t| match t.kind {
            Kind::Substr => Some(t.pat.as_slice()),
            Kind::Glob => t.glob_anchor.as_deref(),
        });
        let from_path = self.path_terms.iter().filter_map(|t| match t.kind {
            Kind::Substr => None,
            Kind::Glob => t.glob_anchor.as_deref(),
        });
        from_name.chain(from_path).max_by_key(|n| n.len())
    }

    /// 組出完整路徑之前的便宜篩子。
    ///
    /// 每個路徑萬用字元關鍵字的尾端字面片段都必須出現在這個項目的名稱裡，
    /// 理由同 `anchor()`。用在目錄掃描上特別有感 —— 否則 26 萬個目錄每一個
    /// 都要往上爬一次組出完整路徑。
    #[inline]
    pub fn name_prefilter(&self, name: &[u8]) -> bool {
        self.path_terms.iter().all(|t| match &t.glob_anchor {
            Some(a) => contains_ci(name, a),
            None => true,
        })
    }

    /// 目錄自身的完整路徑是否通過所有路徑關鍵字。
    #[inline]
    pub fn path_ok_dir(&self, dir_path: &[u8]) -> bool {
        self.path_terms.iter().all(|t| t.hits(dir_path))
    }

    /// 檔案的完整路徑是否通過所有路徑關鍵字。
    ///
    /// 沒有萬用字元時不逐筆組出完整路徑：子字串關鍵字若沒有整段落在目錄路徑裡，
    /// 唯一的可能就是跨過最後那個 `/`，所以只要用「目錄路徑的尾巴 + / + 檔名」
    /// 當比對窗就夠，尾巴取關鍵字長度減一即可涵蓋所有跨邊界的情況。
    ///
    /// 有萬用字元就沒有這個捷徑 —— glob 是整段對齊的，少一個字元都不成立 ——
    /// 只能老實組出完整路徑。`buf` 由呼叫端重複使用，掃描百萬筆時才不會每筆都
    /// 配置一次記憶體。
    #[inline]
    pub fn path_ok_file(&self, dir_path: &[u8], name: &[u8], buf: &mut Vec<u8>) -> bool {
        if self.path_terms.is_empty() {
            return true;
        }
        if self.path_has_glob {
            buf.clear();
            buf.extend_from_slice(dir_path);
            if !buf.ends_with(b"/") {
                buf.push(b'/');
            }
            buf.extend_from_slice(name);
            return self.path_terms.iter().all(|t| t.hits(buf));
        }
        let keep = self.max_path_substr.saturating_sub(1);
        let start = dir_path.len().saturating_sub(keep);
        buf.clear();
        buf.extend_from_slice(&dir_path[start..]);
        buf.push(b'/');
        buf.extend_from_slice(name);
        // 每個關鍵字各自判斷：可能 A 整段在目錄路徑裡，而 B 跨在邊界上。
        self.path_terms
            .iter()
            .all(|t| t.hits(dir_path) || t.hits(buf))
    }
}

#[inline(always)]
fn lower(b: u8) -> u8 {
    // 只折疊 ASCII：非 ASCII 位元組原樣比對，UTF-8 多位元組序列因此仍能正確配對。
    if b.is_ascii_uppercase() {
        b | 0x20
    } else {
        b
    }
}

/// 大小寫不敏感的子字串比對。`needle` 必須已經是小寫。
#[inline]
fn contains_ci(hay: &[u8], needle: &[u8]) -> bool {
    let n = needle.len();
    if n == 0 {
        return true;
    }
    if n > hay.len() {
        return false;
    }
    let first = needle[0];
    for i in 0..=(hay.len() - n) {
        if lower(hay[i]) == first {
            let mut j = 1;
            while j < n && lower(hay[i + j]) == needle[j] {
                j += 1;
            }
            if j == n {
                return true;
            }
        }
    }
    false
}

/// 字元集合 `[...]` 的比對。`start` 指向 `[`。
///
/// 回傳「`]` 之後的位置」與是否命中；沒有收尾的 `]` 就回 None，由呼叫端把 `[`
/// 當成一般字元處理 —— 這是 fnmatch 的行為，使用者打 `[` 不該整個查詢失效。
fn class_match(pat: &[u8], start: usize, ch: u8) -> Option<(usize, bool)> {
    let mut i = start + 1;
    let mut neg = false;
    if i < pat.len() && (pat[i] == b'!' || pat[i] == b'^') {
        neg = true;
        i += 1;
    }
    let mut matched = false;
    // 緊接在 [ 或 [! 後面的 ] 是一般字元，不算收尾。
    let mut first = true;
    let c = lower(ch);
    while i < pat.len() {
        if pat[i] == b']' && !first {
            return Some((i + 1, matched != neg));
        }
        first = false;
        if i + 2 < pat.len() && pat[i + 1] == b'-' && pat[i + 2] != b']' {
            let (a, b) = (lower(pat[i]), lower(pat[i + 2]));
            if a <= c && c <= b {
                matched = true;
            }
            i += 3;
        } else {
            if lower(pat[i]) == c {
                matched = true;
            }
            i += 1;
        }
    }
    None
}

/// fnmatch 風格的萬用字元比對，大小寫不敏感，整段對齊。
///
/// 跟 locate/plocate 一樣**不**把 `/` 當邊界：`*` 會跨過斜線，所以
/// `*/tool/*.rs` 能一路橫跨多層目錄。`pat` 必須已經是小寫。
///
/// 用迭代加回溯而不是遞迴：路徑最長可到數百位元組，遞迴版在 `*` 很多時會退化。
fn glob_match(pat: &[u8], text: &[u8]) -> bool {
    let (mut p, mut t) = (0usize, 0usize);
    // 上一個 `*` 的位置，以及它目前吃到哪裡。
    let (mut star_p, mut star_t) = (usize::MAX, 0usize);
    while t < text.len() {
        if p < pat.len() {
            match pat[p] {
                b'*' => {
                    star_p = p;
                    star_t = t;
                    p += 1;
                    continue;
                }
                b'?' => {
                    p += 1;
                    t += 1;
                    continue;
                }
                b'[' => match class_match(pat, p, text[t]) {
                    Some((next, true)) => {
                        p = next;
                        t += 1;
                        continue;
                    }
                    Some((_, false)) => {}
                    None => {
                        // 沒有收尾的 ]，當成一般字元比對
                        if lower(pat[p]) == lower(text[t]) {
                            p += 1;
                            t += 1;
                            continue;
                        }
                    }
                },
                b'\\' if p + 1 < pat.len() => {
                    if lower(pat[p + 1]) == lower(text[t]) {
                        p += 2;
                        t += 1;
                        continue;
                    }
                }
                c => {
                    if lower(c) == lower(text[t]) {
                        p += 1;
                        t += 1;
                        continue;
                    }
                }
            }
        }
        // 走不下去了：退回上一個 `*`，讓它多吃一個字元再試。
        if star_p != usize::MAX {
            star_t += 1;
            t = star_t;
            p = star_p + 1;
            continue;
        }
        return false;
    }
    // 樣式剩下的必須全是 `*` 才算整段命中。
    while p < pat.len() && pat[p] == b'*' {
        p += 1;
    }
    p == pat.len()
}

pub struct Hits {
    /// 命中的檔案索引，最多 limit 筆。
    pub files: Vec<u32>,
    /// 命中的目錄索引（dir_id），最多 limit 筆。
    pub dirs: Vec<u32>,
    /// 命中總數（不受 limit 影響）。
    pub total: usize,
}

/// 對整個索引做搜尋（沒有 daemon 時走這條）。`threads` 為 0 時自動取 CPU 核心數。
///
/// 顯示順序與配額分配必須跟 `live.rs` 一致：目錄先取，檔案補滿剩餘額度。
pub fn search(idx: &Index, q: &Query, limit: usize, threads: usize) -> Hits {
    if q.is_empty() {
        return Hits {
            files: Vec::new(),
            dirs: Vec::new(),
            total: 0,
        };
    }
    let nthreads = if threads == 0 {
        std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(8)
    } else {
        threads
    };

    // --- 目錄（數十萬筆，單執行緒即可）---
    let nodes = idx.dir_nodes();
    let dnames = idx.dir_names();
    let mut dirs: Vec<u32> = Vec::new();
    let mut dir_total = 0usize;
    if q.wants_dirs() {
        for (id, node) in nodes.iter().enumerate() {
            if node.name_len == 0 {
                continue; // dir_id 分段配發留下的空洞
            }
            let off = node.name_off as usize;
            let name = &dnames[off..off + node.name_len as usize];
            if !q.matches(name) {
                continue;
            }
            if q.has_path_terms() {
                // 先用便宜的篩子擋掉絕大多數，再組完整路徑。
                if !q.name_prefilter(name) {
                    continue;
                }
                // 目錄的完整路徑就是它自己，直接比對即可。
                if !q.path_ok_dir(&idx.dir_path(id as u32)) {
                    continue;
                }
            }
            dir_total += 1;
            if dirs.len() < limit {
                dirs.push(id as u32);
            }
        }
    }

    // --- 檔案 ---
    let recs = idx.file_recs();
    let names = idx.file_names();
    let n = recs.len();
    let chunk = n.div_ceil(nthreads.max(1));

    let mut files: Vec<u32> = Vec::new();
    let mut file_total = 0usize;
    if q.wants_files() && n > 0 {
        // 每個執行緒掃自己那一段，各自收集命中；分段是連續的，所以對 name arena
        // 的存取也維持循序，能吃滿記憶體頻寬。
        let mut parts: Vec<(Vec<u32>, usize)> = std::thread::scope(|s| {
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
                    // 紀錄依 parent 排序，同一個目錄的檔案是連續的，路徑組一次就好。
                    let mut cached_parent = u32::MAX;
                    let mut cached_path: Vec<u8> = Vec::new();
                    let mut buf: Vec<u8> = Vec::new();
                    for (i, r) in recs[start..end].iter().enumerate() {
                        let off = r.name_off as usize;
                        let name = &names[off..off + r.name_len as usize];
                        if !q.matches(name) {
                            continue;
                        }
                        if q.has_path_terms() {
                            if !q.name_prefilter(name) {
                                continue;
                            }
                            if r.parent != cached_parent {
                                cached_parent = r.parent;
                                cached_path = idx.dir_path(r.parent);
                            }
                            if !q.path_ok_file(&cached_path, name, &mut buf) {
                                continue;
                            }
                        }
                        count += 1;
                        if local.len() < limit {
                            local.push((start + i) as u32);
                        }
                    }
                    (local, count)
                }));
            }
            handles.into_iter().map(|h| h.join().unwrap()).collect()
        });

        file_total = parts.iter().map(|p| p.1).sum();
        for (local, _) in parts.iter_mut() {
            if files.len() >= limit {
                break;
            }
            let take = (limit - files.len()).min(local.len());
            files.extend_from_slice(&local[..take]);
        }
    }

    // limit 是「總共顯示幾筆」，不是兩類各自的上限：目錄先排，檔案補滿剩餘額度。
    dirs.truncate(limit);
    files.truncate(limit.saturating_sub(dirs.len()));

    Hits {
        files,
        dirs,
        total: file_total + dir_total,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn q_opts(s: &str, whole_path: bool, basename: bool, tf: TypeFilter) -> Query {
        Query::parse_opts(
            s,
            Options {
                whole_path,
                basename,
                type_filter: Some(tf),
            },
        )
    }

    /// 大小寫不敏感：關鍵字在 parse 時轉小寫，比對時再逐位元組折疊 haystack。
    #[test]
    fn case_insensitive() {
        let q = Query::parse("readme");
        assert!(q.matches(b"README.md"));
        assert!(q.matches(b"ReadMe.txt"));
        assert!(q.matches(b"readme"));
        // 反向也要成立：查詢寫大寫，檔名是小寫
        assert!(Query::parse("README").matches(b"readme.md"));
    }

    /// 多個關鍵字是 AND，且與出現順序無關。
    #[test]
    fn multiple_terms_are_anded() {
        let q = Query::parse("webpack config");
        assert!(q.matches(b"webpack.config.js"));
        assert!(q.matches(b"config-for-webpack"));
        assert!(!q.matches(b"webpack.js"));
        assert!(!q.matches(b"config.js"));
    }

    /// 單字元查詢：長度 1 是最容易寫錯邊界的情況，README 宣稱與 find 逐筆一致。
    #[test]
    fn single_character_query() {
        let q = Query::parse("a");
        assert!(q.matches(b"a"));
        assert!(q.matches(b"bar"));
        assert!(q.matches(b"A"));
        assert!(!q.matches(b"xyz"));
    }

    /// 關鍵字比檔名長時必須直接判否，不能讓 `hay.len() - n` 反向溢位。
    #[test]
    fn needle_longer_than_name() {
        assert!(!Query::parse("readme.markdown").matches(b"readme"));
        assert!(!Query::parse("x").matches(b""));
    }

    /// 命中位置落在開頭或結尾都算數。
    #[test]
    fn matches_at_both_ends() {
        let q = Query::parse("lock");
        assert!(q.matches(b"lockfile"));
        assert!(q.matches(b"package-lock"));
    }

    /// 只折疊 ASCII：UTF-8 多位元組序列原樣比對，中文檔名才不會被折壞。
    #[test]
    fn non_ascii_compared_verbatim() {
        let q = Query::parse("報表");
        assert!(q.matches("年度報表.xlsx".as_bytes()));
        assert!(!q.matches("年度報告.xlsx".as_bytes()));
    }

    /// 空查詢要能被辨識出來，否則會把整個索引當成全數命中。
    #[test]
    fn empty_query_is_detected() {
        assert!(Query::parse("").is_empty());
        assert!(Query::parse("   ").is_empty());
        assert!(!Query::parse("x").is_empty());
    }

    /// 錨點取最長的關鍵字：愈長鑑別度愈高，候選就愈少。
    #[test]
    fn anchor_picks_longest_term() {
        let q = Query::parse("js webpack cfg");
        assert_eq!(q.anchor(), Some(b"webpack".as_slice()));
        assert!(Query::parse("").anchor().is_none());
    }

    /// 萬用字元沒辦法當 memmem 的錨點，全是萬用字元時要回 None 讓掃描端改線性掃。
    #[test]
    fn basename_glob_can_still_anchor() {
        let q = q_opts("*.pdf", false, true, TypeFilter::All);
        assert!(q.has_name_terms());
        assert_eq!(q.anchor(), Some(b".pdf".as_slice()));
        // 混用時挑最長的
        let q = q_opts("*.pdf report", false, true, TypeFilter::All);
        assert_eq!(q.anchor(), Some(b"report".as_slice()));
    }

    /// 萬用字元的錨點是樣式尾端的字面片段。
    #[test]
    fn glob_anchor_is_the_trailing_literal() {
        assert_eq!(Query::parse("*.pdf").anchor(), Some(b".pdf".as_slice()));
        assert_eq!(
            Query::parse("*report*.pdf").anchor(),
            Some(b".pdf".as_slice())
        );
        assert_eq!(Query::parse("foo?.txt").anchor(), Some(b".txt".as_slice()));
        // 以萬用字元收尾就沒有錨點可用
        assert_eq!(Query::parse("*.pdf*").anchor(), None);
        assert_eq!(Query::parse("*/tool/*").anchor(), None);
        assert_eq!(Query::parse("report?").anchor(), None);
        // 尾端片段含斜線時不能用：它可能整段落在目錄名裡，掃檔名 arena 掃不到
        assert_eq!(Query::parse("*deep/readme").anchor(), None);
        // 跳脫過的萬用字元是一般字元，要算進字面片段
        assert_eq!(Query::parse("*a\\*b").anchor(), Some(b"a*b".as_slice()));
    }

    /// 有子字串關鍵字時挑最長的當錨點，萬用字元的尾端片段也一起競爭。
    #[test]
    fn anchor_picks_the_longest_candidate() {
        assert_eq!(
            Query::parse("js *.markdown").anchor(),
            Some(b".markdown".as_slice())
        );
        assert_eq!(
            Query::parse("webpack *.js").anchor(),
            Some(b"webpack".as_slice())
        );
    }

    /// 比對完整路徑的**子字串**關鍵字不能當錨點：它可能整段都在目錄路徑裡。
    #[test]
    fn path_substring_term_is_not_an_anchor() {
        let q = Query::parse("codes/tool");
        assert!(q.has_path_terms());
        assert_eq!(q.anchor(), None);
    }

    /// 便宜篩子：路徑萬用字元的尾端片段必須出現在項目名稱裡。
    #[test]
    fn name_prefilter_uses_glob_anchor() {
        let q = Query::parse("*.pdf");
        assert!(q.name_prefilter(b"report.pdf"));
        assert!(!q.name_prefilter(b"report.txt"));
        // 沒有錨點時篩子不能誤擋
        let q = Query::parse("*/tool/*");
        assert!(q.name_prefilter(b"anything"));
        let q = Query::parse("codes/tool");
        assert!(q.name_prefilter(b"anything"));
    }

    /// 含斜線的關鍵字自動改走路徑比對，不再被當成檔名的一部分。
    #[test]
    fn slash_term_becomes_path_term() {
        let q = Query::parse("tool/mylocate README");
        assert!(q.has_name_terms());
        assert!(q.has_path_terms());
        assert!(q.matches(b"README.md"));
        assert!(!q.matches(b"main.rs"));
        assert!(q.path_ok_dir(b"/Users/x/codes/tool/mylocate"));
        assert!(!q.path_ok_dir(b"/Users/x/codes/tool/printer"));
    }

    /// -w 會把所有關鍵字都轉成路徑關鍵字，連不含斜線的也一樣。
    #[test]
    fn whole_path_converts_every_term() {
        let q = q_opts("codes mylocate", true, false, TypeFilter::All);
        assert!(!q.has_name_terms());
        assert!(q.has_path_terms());
        assert!(q.path_ok_dir(b"/Users/x/codes/tool/mylocate"));
        assert!(!q.path_ok_dir(b"/Users/x/codes/tool/printer"));
        assert!(q.matches("任何名字".as_bytes()));
    }

    /// -b 相反：連含斜線與萬用字元的關鍵字都拉回來只比對檔名。
    #[test]
    fn basename_forces_name_matching() {
        let q = q_opts("*.pdf", false, true, TypeFilter::All);
        assert!(!q.has_path_terms());
        assert!(q.matches(b"report.pdf"));
        assert!(!q.matches(b"report.pdf.bak"));
    }

    /// 路徑關鍵字跨在目錄與檔名的邊界上也要能命中。
    #[test]
    fn path_term_across_dir_and_name_boundary() {
        let q = Query::parse("mylocate/readme");
        let mut buf = Vec::new();
        assert!(q.path_ok_file(b"/Users/x/codes/tool/mylocate", b"README.md", &mut buf));
        assert!(!q.path_ok_file(b"/Users/x/codes/tool/printer", b"README.md", &mut buf));
        // 目錄路徑短於比對窗時不能溢位
        assert!(q.path_ok_file(b"/mylocate", b"readme", &mut buf));
    }

    /// 多個路徑關鍵字各自判斷：一個落在目錄裡、一個跨邊界，兩者都要算命中。
    #[test]
    fn path_terms_evaluated_independently() {
        let q = Query::parse("codes/tool mylocate/readme");
        let mut buf = Vec::new();
        assert!(q.path_ok_file(b"/Users/x/codes/tool/mylocate", b"README.md", &mut buf));
        assert!(!q.path_ok_file(b"/Users/x/other/mylocate", b"README.md", &mut buf));
    }

    /// 型別過濾的語意。
    #[test]
    fn type_filter_gates_each_kind() {
        let all = q_opts("x", false, false, TypeFilter::All);
        assert!(all.wants_dirs() && all.wants_files());
        let d = q_opts("x", false, false, TypeFilter::Dirs);
        assert!(d.wants_dirs() && !d.wants_files());
        let f = q_opts("x", false, false, TypeFilter::Files);
        assert!(!f.wants_dirs() && f.wants_files());
    }

    /// 只有路徑關鍵字時查詢不算空，否則會被當成沒輸入而直接回傳空結果。
    #[test]
    fn path_only_query_is_not_empty() {
        let q = Query::parse("tool/mylocate");
        assert!(!q.is_empty());
        assert!(!q.has_name_terms());
    }

    // --- 萬用字元 ---

    /// glob 是整段對齊，不是子字串：`*.pdf` 才能真的把副檔名釘在結尾。
    #[test]
    fn glob_is_anchored_not_substring() {
        assert!(glob_match(b"*.pdf", b"/a/b/report.pdf"));
        assert!(!glob_match(b"*.pdf", b"/a/b/report.pdf.bak"));
        assert!(glob_match(b"*.pdf*", b"/a/b/report.pdf.bak"));
        assert!(glob_match(b"*", b""));
        assert!(!glob_match(b"", b"x"));
        assert!(glob_match(b"", b""));
    }

    /// `*` 會跨過斜線，跟 locate/plocate 一致。
    #[test]
    fn glob_star_crosses_slashes() {
        assert!(glob_match(
            b"/users/*/codes/*.rs",
            b"/users/jlin/codes/tool/x.rs"
        ));
        assert!(glob_match(b"*tool*", b"/users/jlin/codes/tool/x.rs"));
    }

    /// `?` 剛好一個字元。
    #[test]
    fn glob_question_matches_exactly_one() {
        assert!(glob_match(b"a?c", b"abc"));
        assert!(!glob_match(b"a?c", b"ac"));
        assert!(!glob_match(b"a?c", b"abbc"));
    }

    /// 字元集合，含範圍與否定。
    #[test]
    fn glob_character_class() {
        assert!(glob_match(b"file[0-9].txt", b"file7.txt"));
        assert!(!glob_match(b"file[0-9].txt", b"filex.txt"));
        assert!(glob_match(b"file[!0-9].txt", b"filex.txt"));
        assert!(!glob_match(b"file[!0-9].txt", b"file7.txt"));
        assert!(glob_match(b"[abc]x", b"bx"));
        // 緊接在 [ 後面的 ] 是一般字元
        assert!(glob_match(b"[]]x", b"]x"));
        // 沒有收尾的 ] 時，[ 退化成一般字元，不該讓整個查詢失效
        assert!(glob_match(b"a[bc", b"a[bc"));
    }

    /// 反斜線跳脫，讓使用者查得到名字裡真的有 * 的檔案。
    #[test]
    fn glob_backslash_escapes() {
        assert!(glob_match(b"a\\*b", b"a*b"));
        assert!(!glob_match(b"a\\*b", b"axxb"));
    }

    /// glob 大小寫不敏感，與整個工具的預設一致。
    #[test]
    fn glob_is_case_insensitive() {
        assert!(glob_match(b"*.pdf", b"/a/REPORT.PDF"));
    }

    /// 多個 `*` 的回溯不能退化成指數時間，也不能誤判。
    #[test]
    fn glob_backtracking_terminates() {
        assert!(glob_match(b"*a*b*c*", b"xxaxxbxxcxx"));
        assert!(!glob_match(b"*a*b*c*d", b"xxaxxbxxcxx"));
        let long = vec![b'x'; 4096];
        assert!(!glob_match(b"*a*a*a*a*a*a*b", &long));
    }

    /// 含萬用字元的關鍵字預設比對完整路徑，所以 `*.pdf` 直接可用。
    #[test]
    fn glob_term_defaults_to_path_scope() {
        let q = Query::parse("*.pdf");
        assert!(q.has_path_terms());
        assert!(!q.has_name_terms());
        let mut buf = Vec::new();
        assert!(q.path_ok_file(b"/Users/x/docs", b"report.pdf", &mut buf));
        assert!(!q.path_ok_file(b"/Users/x/docs", b"report.pdf.bak", &mut buf));
        assert!(q.path_ok_dir(b"/Users/x/weird.pdf"));
    }

    /// 萬用字元與子字串關鍵字混用時，兩種比對窗不能互相污染。
    #[test]
    fn glob_and_substring_path_terms_mix() {
        let q = Query::parse("codes/tool *.rs");
        let mut buf = Vec::new();
        assert!(q.path_ok_file(b"/Users/x/codes/tool/mylocate/src", b"live.rs", &mut buf));
        assert!(!q.path_ok_file(b"/Users/x/codes/tool/mylocate/src", b"live.txt", &mut buf));
        assert!(!q.path_ok_file(b"/Users/x/other/src", b"live.rs", &mut buf));
    }
}
