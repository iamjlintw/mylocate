//! 搜尋引擎。
//!
//! 刻意不建倒排索引／trigram 之類的結構 —— Everything 本身也沒有，它就是對每個
//! 檔名做多執行緒字串比對。對百萬級的短字串來說，線性掃描是記憶體頻寬受限的
//! 問題，額外的索引結構只會膨脹記憶體並拖慢更新，反而得不償失。

use crate::index::Index;

/// 解析後的查詢：以空白分隔的多個關鍵字，全部命中才算 match（AND 語意）。
pub struct Query {
    /// 各關鍵字，一律轉為小寫以便做大小寫不敏感比對。
    terms: Vec<Vec<u8>>,
}

impl Query {
    pub fn parse(s: &str) -> Query {
        Query {
            terms: s
                .split_whitespace()
                .map(|t| t.bytes().map(|b| b.to_ascii_lowercase()).collect())
                .collect(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.terms.is_empty()
    }

    /// 名稱是否命中此查詢（所有關鍵字都要出現）。
    #[inline]
    pub fn matches(&self, name: &[u8]) -> bool {
        matches(name, &self.terms)
    }

    #[allow(dead_code)]
    pub fn terms(&self) -> &[Vec<u8>] {
        &self.terms
    }

    /// 選最長的關鍵字當作掃描錨點：愈長愈有鑑別度，候選就愈少。
    pub fn anchor(&self) -> Option<(usize, &[u8])> {
        self.terms
            .iter()
            .enumerate()
            .max_by_key(|(_, t)| t.len())
            .map(|(i, t)| (i, t.as_slice()))
    }

    /// 除了錨點以外的其他關鍵字都命中嗎。
    #[inline]
    pub fn matches_rest(&self, name: &[u8], anchor_idx: usize) -> bool {
        self.terms
            .iter()
            .enumerate()
            .all(|(i, t)| i == anchor_idx || contains_ci(name, t))
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

#[inline]
fn matches(name: &[u8], terms: &[Vec<u8>]) -> bool {
    terms.iter().all(|t| contains_ci(name, t))
}

pub struct Hits {
    /// 命中的檔案索引，最多 limit 筆。
    pub files: Vec<u32>,
    /// 命中的目錄索引（dir_id），最多 limit 筆。
    pub dirs: Vec<u32>,
    /// 命中總數（不受 limit 影響）。
    pub total: usize,
}

/// 對整個索引做搜尋。`threads` 為 0 時自動取 CPU 核心數。
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

    let recs = idx.file_recs();
    let names = idx.file_names();
    let terms = &q.terms;
    let n = recs.len();
    let chunk = n.div_ceil(nthreads.max(1));

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
                for (i, r) in recs[start..end].iter().enumerate() {
                    let off = r.name_off as usize;
                    let name = &names[off..off + r.name_len as usize];
                    if matches(name, terms) {
                        count += 1;
                        if local.len() < limit {
                            local.push((start + i) as u32);
                        }
                    }
                }
                (local, count)
            }));
        }
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });

    let total: usize = parts.iter().map(|p| p.1).sum();
    let mut files: Vec<u32> = Vec::new();
    for (local, _) in parts.iter_mut() {
        if files.len() >= limit {
            break;
        }
        let take = (limit - files.len()).min(local.len());
        files.extend_from_slice(&local[..take]);
    }

    // 目錄只有數十萬筆，單執行緒掃即可。
    let nodes = idx.dir_nodes();
    let dnames = idx.dir_names();
    let mut dirs = Vec::new();
    let mut dir_total = 0usize;
    for (id, node) in nodes.iter().enumerate() {
        if node.name_len == 0 {
            continue; // dir_id 分段配發留下的空洞
        }
        let off = node.name_off as usize;
        let name = &dnames[off..off + node.name_len as usize];
        if matches(name, terms) {
            dir_total += 1;
            if dirs.len() < limit {
                dirs.push(id as u32);
            }
        }
    }

    // limit 是「總共顯示幾筆」，不是兩類各自的上限：目錄先排，檔案補滿剩餘額度。
    dirs.truncate(limit);
    files.truncate(limit.saturating_sub(dirs.len()));

    Hits {
        files,
        dirs,
        total: total + dir_total,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let (idx, term) = q.anchor().unwrap();
        assert_eq!(term, b"webpack".as_slice());
        assert_eq!(q.terms()[idx], b"webpack".to_vec());
        assert!(Query::parse("").anchor().is_none());
    }

    /// matches_rest 必須跳過錨點本身，只驗其餘關鍵字。
    #[test]
    fn matches_rest_skips_anchor() {
        let q = Query::parse("webpack js");
        let (idx, _) = q.anchor().unwrap();
        // 檔名不含 webpack（錨點由呼叫端另行確認），其餘關鍵字 js 命中
        assert!(q.matches_rest(b"main.js", idx));
        assert!(!q.matches_rest(b"main.ts", idx));
    }
}
