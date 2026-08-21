//! 常駐服務：把索引留在記憶體裡，並用 FSEvents 持續維持它的新鮮度。
//!
//! daemon 解決兩件事：
//!
//! 1. **冷啟動**：CLI 每次執行都要重新 mmap 上百 MB，第一次查詢會吃到 page
//!    fault。常駐之後索引一直在記憶體裡，查詢只剩純計算。
//! 2. **索引新鮮度**：這才是重點。沒有 daemon 就得手動重跑 `ml index`；有了
//!    FSEvents，檔案存檔後毫秒內就會進索引。

use crate::fsevents;
use crate::index;
use crate::live::Live;
use crate::search::Query;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

/// 共享的索引快照。
///
/// 外層 `Mutex` 只保護「指標交換」這個動作本身，持有時間是奈秒級；查詢端
/// 複製一份 `Arc` 就立刻放手，因此永遠不會被更新端擋住。
type Shared = Arc<Mutex<Arc<Live>>>;

/// 取出目前的快照。刻意寫成獨立函式，強調鎖只用來 clone 指標。
fn snapshot(shared: &Shared) -> Arc<Live> {
    shared.lock().unwrap().clone()
}

/// delta 累積超過這個量就重建快照，避免搜尋時要附帶掃描的 delta 愈拖愈長。
const COMPACT_THRESHOLD: usize = 200_000;

pub fn socket_path() -> PathBuf {
    index::default_path().with_file_name("daemon.sock")
}

/// 回應中標示結尾與統計的前綴字元（不會出現在路徑裡）。
pub const EOT: u8 = 0x04;

pub fn run(root: &str) -> std::io::Result<()> {
    let idx_path = index::default_path();
    let idx = match index::Index::open(&idx_path) {
        Ok(i) => i,
        Err(e) => {
            eprintln!("無法開啟索引（{e}）");
            eprintln!("請先執行：ml index");
            std::process::exit(1);
        }
    };

    // 決定要從哪裡接上事件流。
    let stored_id = idx.header().event_id;
    let stored_uuid = idx.header().dev_uuid;
    let cur_uuid = crate::root_device(root).map_or([0u8; 16], fsevents::device_uuid);

    let since = if stored_id == 0 || stored_uuid == [0u8; 16] {
        eprintln!("索引沒有 FSEvents 續傳點，只接收今後的變更（建議重跑 ml index）");
        fsevents::SINCE_NOW
    } else if stored_uuid != cur_uuid {
        // 事件資料庫被重建過，舊 id 指向的位置已不存在，重放會漏事件。
        eprintln!("FSEvents 資料庫已重建（UUID 不符），索引可能不完整，建議重跑 ml index");
        fsevents::SINCE_NOW
    } else {
        eprintln!("自 event_id={stored_id} 起重放離線期間的變更");
        stored_id
    };

    let mut live_init = Live::new(idx);
    // 索引檔就放在監看範圍內，重建時寫入的上百 MB 不該回頭觸發自己。
    if let Some(dir) = idx_path.parent() {
        live_init.ignore_path(dir.as_os_str().as_encoded_bytes());
    }
    let live: Shared = Arc::new(Mutex::new(Arc::new(live_init)));
    {
        let s = snapshot(&live);
        eprintln!("索引就緒：{} 個目錄 / {} 個檔案", s.total_dirs(), s.total_files());
    }

    // --- FSEvents 監看執行緒 ---
    let (tx, rx) = std::sync::mpsc::channel();
    let watch_root = root.to_string();
    std::thread::spawn(move || {
        fsevents::watch_forever(&[&watch_root], since, 0.15, tx);
    });

    // --- 套用變更的執行緒 ---
    let live_up = Arc::clone(&live);
    let root_owned = root.to_string();
    std::thread::spawn(move || {
        for batch in rx {
            let items: Vec<(Vec<u8>, u32)> =
                batch.into_iter().map(|e| (e.path, e.flags)).collect();

            // 重放離線期間的變更時，索引會有一小段追不上的時間；用哨兵事件
            // 明確告訴使用者何時真正同步完成，免得誤以為新檔案沒被索引到。
            let history_done = items
                .iter()
                .any(|(_, f)| f & crate::fsevents::EV_HISTORY_DONE != 0);

            // 這個迴圈的重點是「絕不在鎖裡做 I/O」。$HOME 底下各種 app 的快取
            // activity 幾乎不會停，只要寫鎖裡出現一次 list_dir，查詢就會被拖到
            // 幾百毫秒。所以拆成四步，讀目錄一律在鎖外完成。

            // 1) 取快照（不等鎖），在它上面算出工作清單
            let cur = snapshot(&live_up);
            let plan = cur.plan(&items);
            let need_full = plan.need_full;
            if !plan.notes.is_empty() {
                eprintln!("事件處理：{}", plan.notes.join("；"));
            }

            // 2) 鎖外：所有 I/O 都在這裡做完
            let rescan_trees: Vec<(u32, crate::live::Subtree)> = if plan.rescan_ids.is_empty() {
                Vec::new()
            } else {
                eprintln!("重掃 {} 棵子樹", plan.rescan_ids.len());
                plan.rescan_ids
                    .iter()
                    .map(|(id, path)| {
                        let name = match path.iter().rposition(|&b| b == b'/') {
                            Some(p) => path[p + 1..].to_vec(),
                            None => path.clone(),
                        };
                        (*id, crate::live::read_subtree(path, &name))
                    })
                    .collect()
            };

            let listings: Vec<(u32, Vec<u8>, Option<Vec<(Vec<u8>, bool, bool)>>)> = plan
                .refresh
                .into_iter()
                .map(|(id, path)| {
                    let e = crate::scan::list_dir(&path).ok();
                    (id, path, e)
                })
                .collect();

            // 3) 複製出一份新快照並套用差異。複製的只是那層薄 delta，
            //    上百 MB 的基底是 Arc 共享的，不會被搬動。
            let mut next = (*cur).clone();
            for (id, tree) in rescan_trees.iter() {
                next.rescan_subtree(*id, tree);
            }
            let mut pending: Vec<(u32, Vec<u8>)> = Vec::new();
            for (id, path, entries) in listings {
                for name in next.apply_listing(id, entries) {
                    let mut p = path.clone();
                    if !p.ends_with(b"/") {
                        p.push(b'/');
                    }
                    p.extend_from_slice(&name);
                    pending.push((id, p));
                }
            }

            // 4) 新出現的子樹同樣在鎖外讀完，再掛進這份快照
            for (parent, path) in pending {
                let name = match path.iter().rposition(|&b| b == b'/') {
                    Some(p) => path[p + 1..].to_vec(),
                    None => path.clone(),
                };
                let tree = crate::live::read_subtree(&path, &name);
                next.graft(parent, &tree);
            }

            // 5) 原子替換。到這一步為止查詢端都還在用舊快照，完全沒被擋過。
            let bloated = next.delta_len() > COMPACT_THRESHOLD;
            if history_done {
                eprintln!(
                    "離線期間的變更已重放完畢，索引同步（目錄 {} / 檔案 {}）",
                    next.total_dirs(),
                    next.total_files()
                );
            }
            *live_up.lock().unwrap() = Arc::new(next);

            if need_full || bloated {
                let why = if need_full {
                    "事件溢位或磁碟區變動"
                } else {
                    "delta 過大"
                };
                eprintln!("重建索引快照（{why}）…");
                if let Err(e) = rebuild(&root_owned, &live_up) {
                    eprintln!("重建失敗：{e}");
                }
            }
        }
    });

    // --- 查詢服務 ---
    let sock = socket_path();
    if let Some(dir) = sock.parent() {
        std::fs::create_dir_all(dir)?;
    }
    // 清掉上次沒收乾淨的 socket 檔，否則 bind 會失敗。
    let _ = std::fs::remove_file(&sock);
    let listener = UnixListener::bind(&sock)?;
    eprintln!("開始服務：{}", sock.display());

    for stream in listener.incoming() {
        match stream {
            Ok(s) => {
                let live = Arc::clone(&live);
                std::thread::spawn(move || {
                    let _ = handle_client(s, live);
                });
            }
            Err(e) => eprintln!("accept 失敗：{e}"),
        }
    }
    Ok(())
}

/// 重新全量掃描並替換掉記憶體中的索引，同時把新快照寫回磁碟。
fn rebuild(root: &str, live: &Shared) -> std::io::Result<()> {
    let event_id = fsevents::current_event_id();
    let dev_uuid = crate::root_device(root).map_or([0u8; 16], fsevents::device_uuid);
    let result = crate::scan::scan(root, 0)?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let bytes = index::build(&result, event_id, dev_uuid, now);
    let path = index::default_path();
    index::write_to(&path, &bytes)?;

    let fresh = index::Index::open(&path)?;
    let mut rebuilt = Live::new(fresh);
    if let Some(dir) = path.parent() {
        rebuilt.ignore_path(dir.as_os_str().as_encoded_bytes());
    }
    *live.lock().unwrap() = Arc::new(rebuilt);
    eprintln!("索引快照已重建");
    Ok(())
}

fn handle_client(stream: UnixStream, live: Shared) -> std::io::Result<()> {
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut line = String::new();
    if reader.read_line(&mut line)? == 0 {
        return Ok(());
    }

    let mut out = std::io::BufWriter::new(stream);
    let mut parts = line.trim_end_matches('\n').splitn(3, '\t');
    let cmd = parts.next().unwrap_or("");
    match cmd {
        "S" => {
            let limit: usize = parts.next().and_then(|s| s.parse().ok()).unwrap_or(50);
            let limit = if limit == 0 { usize::MAX } else { limit };
            let q = Query::parse(parts.next().unwrap_or(""));

            let t0 = std::time::Instant::now();
            let g = snapshot(&live);
            let (hits, total) = g.search(&q, limit);
            let us = t0.elapsed().as_micros();

            for h in hits.iter() {
                out.write_all(&g.path_of(*h))?;
                out.write_all(b"\n")?;
            }
            writeln!(out, "{}{}\t{}", EOT as char, total, us)?;
        }
        "I" => {
            let g = snapshot(&live);
            writeln!(
                out,
                "{}{}\t{}\t{}\t{}",
                EOT as char,
                g.total_dirs(),
                g.total_files(),
                g.delta_len(),
                g.dead_len()
            )?;
        }
        _ => {
            writeln!(out, "{}0\t0", EOT as char)?;
        }
    }
    out.flush()
}

/// 客戶端：把查詢交給 daemon。daemon 沒在跑就回傳 None，讓呼叫端自行退回
/// 直接讀索引檔的路徑 —— 沒裝 daemon 的人也要能正常使用。
pub fn query_daemon(req: &str) -> Option<(Vec<String>, String)> {
    let sock = socket_path();
    if !Path::new(&sock).exists() {
        return None;
    }
    let mut s = UnixStream::connect(&sock).ok()?;
    s.write_all(req.as_bytes()).ok()?;
    s.flush().ok()?;

    let reader = BufReader::new(s);
    let mut lines = Vec::new();
    let mut tail = String::new();
    for l in reader.lines() {
        let l = l.ok()?;
        if let Some(rest) = l.strip_prefix(EOT as char) {
            tail = rest.to_string();
            break;
        }
        lines.push(l);
    }
    Some((lines, tail))
}
