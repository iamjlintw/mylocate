# mylocate

**繁體中文** | [简体中文](README.zh-CN.md) | [English](README.en.md)

[![CI](https://github.com/iamjlintw/mylocate/actions/workflows/ci.yml/badge.svg)](https://github.com/iamjlintw/mylocate/actions/workflows/ci.yml)

macOS 上的即時檔案搜尋，目標是做到跟 Windows 的 [Everything](https://www.voidtools.com/) 一樣的體感：**打字即出結果、索引永遠是新的**。

用 Rust 寫成，除了 `memchr`（SIMD 字串搜尋）之外沒有其他相依。

## 實測（M2 Pro，`$HOME` 底下 327 萬個檔案）

| | 建索引 | 單次查詢 | 覆蓋率 |
|---|---|---|---|
| **mylocate** | 16 秒（僅首次） | **6.6 ms**（P95 7.0） | 100% |
| Spotlight `mdfind` | 背景常駐 | 50 ms ~ 3.9 秒 | **42%** |
| `searchfs`（檔案系統 catalog） | 不需要 | 46.8 秒／次 | 100% |
| `find` | 不需要 | 10.7 秒／次 | 100% |
| `locate`（需先 `updatedb`） | 全掃一次 | 數百 ms ~ 數秒 | 每週才更新 |

覆蓋率是拿 `find` 的結果逐筆比對驗證的：`webpack.config`、`README`、`package-lock.json`、單字元查詢、多關鍵字 AND、目錄搜尋全部 100% 一致。

> Spotlight 只有 42% 是因為它會主動跳過 `node_modules`、`.git` 與含 `.noindex` 的目錄 —— 對開發機來說等於半殘。

## 需求

- macOS（Apple Silicon 與 Intel 皆可）
- [Rust toolchain](https://rustup.rs/)，用來編譯
- Xcode Command Line Tools（`xcode-select --install`）
- [fzf](https://github.com/junegunn/fzf)：選配，只有互動模式 `ml -i` 會用到

首次建立索引與 daemon 監看檔案變動需要讀取 `$HOME` 底下所有檔案。若掃描結果明顯偏少，到「系統設定 → 隱私權與安全性 → 完全取用磁碟」把終端機（或 `ml`）加進去。

## 安裝

```sh
./install.sh
```

腳本會編譯、把 `ml` 裝到 `~/.local/bin`、建立第一份索引，並詢問是否要註冊 launchd agent 讓 daemon 開機自動啟動。

## 使用

```sh
ml webpack.config        # 搜尋（大小寫不敏感）
ml webpack config        # 多個關鍵字為 AND
ml -n 0 README           # 不限筆數（預設 50）
ml -i                    # 互動模式，打字即時篩選（需要 fzf）
ml stats                 # 索引與 daemon 狀態
ml index [路徑]          # 重建索引（預設 $HOME）
ml daemon [路徑]         # 前景啟動常駐服務
```

daemon 在跑的話查詢會自動走它；沒跑就直接讀索引檔（慢一些，約 50 ms）。

## 解除安裝

```sh
launchctl unload -w ~/Library/LaunchAgents/com.mylocate.daemon.plist   # 若有註冊 agent
rm -f ~/Library/LaunchAgents/com.mylocate.daemon.plist
rm -f ~/.local/bin/ml
rm -rf ~/Library/Caches/mylocate                                        # 索引檔
```

## 運作方式

```
                  ┌─ 全量掃描（僅首次，16 秒）──────┐
   APFS ─────────►│ getattrlistbulk + openat 遞迴   │──► 索引檔（105 MB）
                  └─────────────────────────────────┘         │
                                                              │ mmap（零解析）
   檔案變動 ──► FSEvents ──► 重新列舉該目錄 + diff ──► delta ─┴──► 查詢 6.6 ms
```

三個關鍵設計：

**1. 索引持久化，全量掃描一輩子只付一次。**
節點是 12 bytes 的 `repr(C)` 結構，索引檔就是原樣的陣列，載入時 `mmap` 回來直接當 slice 用，不做任何反序列化。重開機後靠 FSEvents 的 `sinceWhen` 重放離線期間的變更補齊。

**2. 查詢完全不等鎖。**
上百 MB 的基底用 `Arc` 共享，更新時只複製那層很薄的 delta，改完再原子替換掉整份快照。查詢端拿到 `Arc` 就立刻放手，永遠不會被更新擋住。

這條路踩過兩次坑：一開始 updater 在寫鎖裡呼叫 `list_dir`，而 `$HOME` 底下各種 app 的快取活動幾乎不會停，查詢延遲直接惡化到 3.6 秒；把 I/O 移出鎖外後中位數回到 7 ms，但偶爾撞上寫鎖仍會飆到 100 ms。改成 copy-on-write 之後最大值才降到 7.4 ms。

**3. 搜尋是線性掃描，不是倒排索引。**
Everything 官方說法是「optimized multi-threaded strstr on every single filename」，它也沒有用任何索引結構。我們把整個小寫檔名 arena 當成一大塊 haystack 丟給向量化的 `memmem`，命中後二分回推是哪一筆 —— 記憶體存取完全循序，比對走 SIMD。

## 為什麼不用那些看似更好的方案

都實測過了，不是憑印象排除的：

| 方案 | 為什麼不用 |
|---|---|
| **SQLite 存 metadata** | 建索引不會變快（照樣得走一次 APFS B-tree），`LIKE '%x%'` 用不到索引會退化成全表掃描，更新也比改記憶體陣列慢。Everything 自己也沒用 SQL。 |
| **比照 Everything 直接讀磁碟** | Everything 快是因為 NTFS 的 MFT 是一整塊連續區域，順序讀就拿到全部 metadata。APFS 沒有等價物，而且開了 FileVault 之後 raw device 讀出來是密文。 |
| **`searchfs(2)` 檔案系統 catalog 搜尋** | 實測全碟一次 46.8 秒，比 `find` 還慢。「比 find 快 100 倍」是 HFS+ 時代的數字，APFS 換了結構後優勢就沒了。 |
| **`updatedb` / `locate`** | 內部就是 `find` 全掃、排程每週一次、查詢要線性解壓掃整個文字檔。三個環節全輸。 |
| **倒排索引 / trigram** | 記憶體會膨脹好幾倍、更新複雜，而線性掃描本來就只要 6.6 ms。 |

### 全量掃描的 16 秒是物理下限

`kern.maxvnodes` 是 247,308，只裝得下全部檔案的 7.5%，所以絕大多數目錄項目都得冷讀 SSD 上的 B-tree —— 這是 I/O bound，不是 CPU bound。實測 `sys` 時間 70~90 秒但 wall 固定在 13~16 秒，有效並行度卡在 5（NVMe 的並行深度上限），而 `user` 時間只有 0.6 秒。

試過但**確認無效**的方向：精簡 `getattrlistbulk` 索取的屬性（sys 降 22%，wall 反而變慢）、改用 `openat` 省去絕對路徑重新解析（無改善）、增加執行緒（8 之後就不再變快）。

唯一有效的優化是修掉 thundering herd —— 原本每掃完一個目錄就 `notify_all()`，44.5 萬次全體喚醒搶同一把鎖，改掉之後 12 執行緒從 20.3 秒降到 11.8 秒。

## 已知限制

- 索引範圍是單一磁碟區（掃描時會擋掉跨 volume 的遞迴），外接碟不會被包含。
- 不索引檔案內容，只搜檔名 —— 要搜內容請用 `rg`。
- 不存檔案大小與修改時間：那兩個欄位在 inode record 裡，索取它們會讓核心對每個檔案多做一次 B-tree 查詢。搜尋結果通常只看幾十筆，需要時對那幾筆補一次 `stat` 更划算。
- 事件溢位只會重掃受影響的子樹；只有根目錄被搬走或事件 id 回繞才會整份重建。delta 累積超過 20 萬筆時也會重建，期間查詢仍正常服務。
- 索引檔自己放在監看範圍內，daemon 會忽略它的事件 —— 否則重建時寫入的上百 MB 會回頭觸發自己，形成「重建→產生事件→再重建」的迴圈（實測 60 秒內重建了 7 次）。

## 授權

MIT，詳見 [LICENSE](LICENSE)。
