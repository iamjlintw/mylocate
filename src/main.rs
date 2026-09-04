mod daemon;
mod ffi;
mod fsevents;
mod index;
mod live;
mod scan;
mod search;

use std::io::Write;
use std::time::Instant;

const USAGE: &str = "\
mylocate — macOS 上的即時檔案搜尋

用法：
  ml <關鍵字>...        搜尋（多個關鍵字為 AND，大小寫不敏感）
  ml index [路徑]       建立索引（預設為 $HOME）
  ml daemon [路徑]      常駐服務，用 FSEvents 即時維持索引新鮮
  ml watch [路徑]       印出檔案系統事件（除錯用）
  ml -i                 互動模式（需要 fzf，打字即時篩選）
  ml stats              顯示索引與 daemon 狀態

選項：
  -n <數量>             最多顯示幾筆（預設 50，0 為不限）
  -V, --version         顯示版本

daemon 在跑的話，查詢會自動走它；否則直接讀索引檔。
";

/// 取得某路徑所在裝置的 dev_t，用來向 FSEvents 索取事件資料庫 UUID。
pub fn root_device(path: &str) -> Option<libc::dev_t> {
    let mut c: Vec<i8> = path.bytes().map(|b| b as i8).collect();
    c.push(0);
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    if unsafe { libc::stat(c.as_ptr(), &mut st) } != 0 {
        return None;
    }
    Some(st.st_dev)
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() {
        print!("{USAGE}");
        return;
    }

    match args[0].as_str() {
        "index" => cmd_index(args.get(1).map(|s| s.as_str())),
        "stats" => cmd_stats(),
        "watch" => cmd_watch(args.get(1).map(|s| s.as_str())),
        "daemon" => {
            let home = std::env::var("HOME").unwrap_or_else(|_| "/".into());
            let root = args.get(1).cloned().unwrap_or(home);
            if let Err(e) = daemon::run(&root) {
                eprintln!("daemon 結束：{e}");
                std::process::exit(1);
            }
        }
        "-i" | "--interactive" => cmd_interactive(),
        "-h" | "--help" => print!("{USAGE}"),
        "-V" | "--version" => println!("mylocate {}", env!("CARGO_PKG_VERSION")),
        _ => cmd_search(&args),
    }
}

fn cmd_index(root: Option<&str>) {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/".into());
    let root = root.unwrap_or(&home);

    eprintln!("掃描 {root} …");

    // 續傳點必須在掃描「開始之前」就取好：掃描途中發生的變更會落在這個 id
    // 之後，日後重放時仍能補上。反過來先掃再取，中間的變更就永遠遺失了。
    // 重放同一筆變更是無害的 —— 更新走的是「重新列舉並 diff」，本身冪等。
    let event_id = fsevents::current_event_id();
    let dev_uuid = root_device(root).map_or([0u8; 16], fsevents::device_uuid);

    let t0 = Instant::now();
    let result = match scan::scan(root, 0) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("掃描失敗：{e}");
            std::process::exit(1);
        }
    };
    let scan_time = t0.elapsed();
    let s = &result.stats;

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let bytes = index::build(&result, event_id, dev_uuid, now);
    let path = index::default_path();
    if let Err(e) = index::write_to(&path, &bytes) {
        eprintln!("寫入索引失敗：{e}");
        std::process::exit(1);
    }

    eprintln!(
        "完成：{} 項（目錄 {} / 檔案 {}），無法讀取 {} 個目錄",
        s.dirs + s.files,
        s.dirs,
        s.files,
        s.errors
    );
    eprintln!(
        "掃描 {:.2} 秒　索引 {:.1} MB → {}",
        scan_time.as_secs_f64(),
        bytes.len() as f64 / 1_048_576.0,
        path.display()
    );
}

/// 印出 FSEvents 事件流，用來驗證監看管線是否正常。
fn cmd_watch(root: Option<&str>) {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/".into());
    let root = root.unwrap_or(&home).to_string();

    let cur = fsevents::current_event_id();
    eprintln!("監看 {root}");
    eprintln!("目前 event_id = {cur}（Ctrl-C 結束）");

    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        fsevents::watch_forever(&[&root], fsevents::SINCE_NOW, 0.1, tx);
    });

    for batch in rx {
        for e in batch {
            println!(
                "[{}] {} {}",
                e.id,
                describe_flags(e.flags),
                String::from_utf8_lossy(&e.path)
            );
        }
    }
}

fn describe_flags(f: u32) -> String {
    let mut v: Vec<&str> = Vec::new();
    if f & fsevents::EV_ITEM_CREATED != 0 {
        v.push("建立");
    }
    if f & fsevents::EV_ITEM_REMOVED != 0 {
        v.push("刪除");
    }
    if f & fsevents::EV_ITEM_RENAMED != 0 {
        v.push("改名");
    }
    if f & fsevents::EV_ITEM_MODIFIED != 0 {
        v.push("修改");
    }
    if f & fsevents::EV_ITEM_IS_DIR != 0 {
        v.push("目錄");
    }
    if f & fsevents::EV_ITEM_IS_FILE != 0 {
        v.push("檔案");
    }
    if f & fsevents::EV_MUST_SCAN_SUBDIRS != 0 {
        v.push("**需重掃**");
    }
    if f & fsevents::EV_HISTORY_DONE != 0 {
        v.push("歷史重放完畢");
    }
    if v.is_empty() {
        format!("0x{f:x}")
    } else {
        v.join("+")
    }
}

/// 互動模式：交給 fzf 當前端，每次按鍵都重新查詢。
///
/// 之所以每次擊鍵都能重跑整個查詢而不卡頓，靠的是 daemon 把索引留在記憶體裡 ——
/// 單次查詢只要數毫秒，遠低於人打字的間隔。
fn cmd_interactive() {
    if std::process::Command::new("fzf")
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| !s.success())
        .unwrap_or(true)
    {
        eprintln!("互動模式需要 fzf，請先安裝：brew install fzf");
        std::process::exit(1);
    }

    let exe = std::env::current_exe()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| "ml".into());

    let status = std::process::Command::new("fzf")
        .args([
            "--disabled", // 交給 mylocate 過濾，fzf 只負責顯示
            "--layout=reverse",
            "--height=80%",
            "--prompt=mylocate> ",
            "--header=輸入即可即時搜尋 · Enter 輸出路徑 · Ctrl-C 離開",
            "--bind",
            &format!("change:reload({exe} -n 200 {{q}} 2>/dev/null || true)"),
        ])
        // 一開始沒有查詢字串，先給一份空清單。
        .env("FZF_DEFAULT_COMMAND", "true")
        .status();

    if let Err(e) = status {
        eprintln!("啟動 fzf 失敗：{e}");
        std::process::exit(1);
    }
}

fn open_index() -> index::Index {
    let path = index::default_path();
    match index::Index::open(&path) {
        Ok(i) => i,
        Err(e) => {
            eprintln!("無法開啟索引（{e}）");
            eprintln!("請先執行：ml index");
            std::process::exit(1);
        }
    }
}

fn cmd_stats() {
    let idx = open_index();
    let h = idx.header();
    println!("索引檔　　：{}", index::default_path().display());
    println!("根目錄　　：{}", String::from_utf8_lossy(idx.root()));
    println!("目錄數　　：{}", h.n_dirs);
    println!("檔案數　　：{}", h.n_files);
    println!("建立時間　：{}", chrono_like(h.scan_time));
    println!("FSEvents　：event_id={}", h.event_id);
    match daemon::query_daemon("I\t\t\n") {
        Some((_, tail)) => {
            let v: Vec<&str> = tail.split('\t').collect();
            println!(
                "daemon　　：執行中（目錄 {} / 檔案 {}，delta {} 筆，已刪 {} 筆）",
                v.first().unwrap_or(&"?"),
                v.get(1).unwrap_or(&"?"),
                v.get(2).unwrap_or(&"?"),
                v.get(3).unwrap_or(&"?")
            );
        }
        None => println!("daemon　　：未執行（ml daemon 可啟動）"),
    }
}

/// 不引入額外相依，簡單把 unix timestamp 轉成本地可讀字串。
fn chrono_like(ts: i64) -> String {
    let out = std::process::Command::new("date")
        .args(["-r", &ts.to_string(), "+%Y-%m-%d %H:%M:%S"])
        .output();
    match out {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).trim().to_string(),
        _ => ts.to_string(),
    }
}

fn cmd_search(args: &[String]) {
    let mut limit = 50usize;
    let mut terms: Vec<String> = Vec::new();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "-n" => {
                if let Some(v) = args.get(i + 1).and_then(|s| s.parse::<usize>().ok()) {
                    limit = if v == 0 { usize::MAX } else { v };
                    i += 1;
                }
            }
            other => terms.push(other.to_string()),
        }
        i += 1;
    }
    if terms.is_empty() {
        print!("{USAGE}");
        return;
    }

    let query = terms.join(" ");

    // daemon 在跑就交給它：索引已在記憶體，省掉每次 mmap 上百 MB 的冷啟動。
    let n = if limit == usize::MAX { 0 } else { limit };
    if let Some((lines, tail)) = daemon::query_daemon(&format!("S\t{n}\t{query}\n")) {
        let stdout = std::io::stdout();
        let mut out = std::io::BufWriter::new(stdout.lock());
        for l in lines.iter() {
            let _ = writeln!(out, "{l}");
        }
        let _ = out.flush();
        let mut t = tail.split('\t');
        let total: usize = t.next().and_then(|v| v.parse().ok()).unwrap_or(0);
        let us: f64 = t.next().and_then(|v| v.parse().ok()).unwrap_or(0.0);
        eprintln!(
            "── {} 筆結果（顯示 {}）· {:.2} ms · daemon",
            total,
            lines.len(),
            us / 1000.0
        );
        return;
    }

    let idx = open_index();
    let q = search::Query::parse(&query);

    let t0 = Instant::now();
    let hits = search::search(&idx, &q, limit, 0);
    let elapsed = t0.elapsed();

    // 大量結果時走 BufWriter，避免每行一次 write syscall 主導了總時間。
    let stdout = std::io::stdout();
    let mut out = std::io::BufWriter::new(stdout.lock());
    for &d in hits.dirs.iter() {
        let _ = out.write_all(&idx.dir_path(d));
        let _ = out.write_all(b"/\n");
    }
    for &f in hits.files.iter() {
        let _ = out.write_all(&idx.file_path(f as usize));
        let _ = out.write_all(b"\n");
    }
    let _ = out.flush();

    let shown = hits.dirs.len() + hits.files.len();
    eprintln!(
        "── {} 筆結果（顯示 {}）· {:.2} ms · 索引 {} 項",
        hits.total,
        shown,
        elapsed.as_secs_f64() * 1000.0,
        idx.header().n_files + idx.header().n_dirs
    );
}
