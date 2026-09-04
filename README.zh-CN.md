# mylocate

[繁體中文](README.zh-TW.md) | **简体中文** | [English](README.md)

[![CI](https://github.com/iamjlintw/mylocate/actions/workflows/ci.yml/badge.svg)](https://github.com/iamjlintw/mylocate/actions/workflows/ci.yml)

macOS 上的实时文件搜索，目标是做到跟 Windows 的 [Everything](https://www.voidtools.com/) 一样的体验：**输入即出结果、索引永远是新的**。

用 Rust 写成，除了 `memchr`（SIMD 字符串搜索）之外没有其他依赖。

## 实测（M2 Pro，`$HOME` 下 327 万个文件）

| | 建索引 | 单次查询 | 覆盖率 |
|---|---|---|---|
| **mylocate** | 16 秒（仅首次） | **6.6 ms**（P95 7.0） | 100% |
| Spotlight `mdfind` | 后台常驻 | 50 ms ~ 3.9 秒 | **42%** |
| `searchfs`（文件系统 catalog） | 不需要 | 46.8 秒／次 | 100% |
| `find` | 不需要 | 10.7 秒／次 | 100% |
| `locate`（需先 `updatedb`） | 全扫一次 | 数百 ms ~ 数秒 | 每周才更新 |

覆盖率是拿 `find` 的结果逐条比对验证的：`webpack.config`、`README`、`package-lock.json`、单字符查询、多关键词 AND、目录搜索全部 100% 一致。

> Spotlight 只有 42% 是因为它会主动跳过 `node_modules`、`.git` 与含 `.noindex` 的目录 —— 对开发机来说等于残废。

## 环境要求

- macOS（Apple Silicon 与 Intel 均可）
- [Rust toolchain](https://rustup.rs/)，用来编译
- Xcode Command Line Tools（`xcode-select --install`）
- [fzf](https://github.com/junegunn/fzf)：可选，只有交互模式 `ml -i` 会用到

首次建立索引与 daemon 监视文件变动需要读取 `$HOME` 下所有文件。若扫描结果明显偏少，到「系统设置 → 隐私与安全性 → 完全磁盘访问权限」把终端（或 `ml`）加进去。

## 安装

```sh
./install.sh
```

脚本会编译、把 `ml` 装到 `~/.local/bin`、建立第一份索引，并询问是否要注册 launchd agent 让 daemon 开机自动启动。

## 使用

```sh
ml webpack.config        # 搜索（大小写不敏感）
ml webpack config        # 多个关键词为 AND
ml -n 0 README           # 不限条数（默认 50）
ml -i                    # 交互模式，输入即时筛选（需要 fzf）
ml stats                 # 索引与 daemon 状态
ml index [路径]          # 重建索引（默认 $HOME）
ml daemon [路径]         # 前台启动常驻服务
ml -V                    # 显示版本
```

daemon 在跑的话查询会自动走它；没跑就直接读索引文件（慢一些，约 50 ms）。

## 卸载

```sh
launchctl unload -w ~/Library/LaunchAgents/com.mylocate.daemon.plist   # 若已注册 agent
rm -f ~/Library/LaunchAgents/com.mylocate.daemon.plist
rm -f ~/.local/bin/ml
rm -rf ~/Library/Caches/mylocate                                        # 索引文件
```

## 工作原理

```
                  ┌─ 全量扫描（仅首次，16 秒）──────┐
   APFS ─────────►│ getattrlistbulk + openat 递归   │──► 索引文件（105 MB）
                  └─────────────────────────────────┘         │
                                                              │ mmap（零解析）
   文件变动 ──► FSEvents ──► 重新枚举该目录 + diff ──► delta ─┴──► 查询 6.6 ms
```

三个关键设计：

**1. 索引持久化，全量扫描一辈子只付一次。**
节点是 12 bytes 的 `repr(C)` 结构，索引文件就是原样的数组，加载时 `mmap` 回来直接当 slice 用，不做任何反序列化。重启后靠 FSEvents 的 `sinceWhen` 重放离线期间的变更补齐。

**2. 查询完全不等锁。**
上百 MB 的基底用 `Arc` 共享，更新时只复制那层很薄的 delta，改完再原子替换掉整份快照。查询端拿到 `Arc` 就立刻放手，永远不会被更新挡住。

这条路踩过两次坑：一开始 updater 在写锁里调用 `list_dir`，而 `$HOME` 下各种 app 的缓存活动几乎不会停，查询延迟直接恶化到 3.6 秒；把 I/O 移出锁外后中位数回到 7 ms，但偶尔撞上写锁仍会飙到 100 ms。改成 copy-on-write 之后最大值才降到 7.4 ms。

**3. 搜索是线性扫描，不是倒排索引。**
Everything 官方说法是「optimized multi-threaded strstr on every single filename」，它也没有用任何索引结构。我们把整个小写文件名 arena 当成一大块 haystack 丢给向量化的 `memmem`，命中后二分回推是哪一条 —— 内存访问完全顺序，比对走 SIMD。

## 为什么不用那些看似更好的方案

都实测过了，不是凭印象排除的：

| 方案 | 为什么不用 |
|---|---|
| **SQLite 存 metadata** | 建索引不会变快（照样得走一次 APFS B-tree），`LIKE '%x%'` 用不到索引会退化成全表扫描，更新也比改内存数组慢。Everything 自己也没用 SQL。 |
| **仿照 Everything 直接读磁盘** | Everything 快是因为 NTFS 的 MFT 是一整块连续区域，顺序读就拿到全部 metadata。APFS 没有等价物，而且开了 FileVault 之后 raw device 读出来是密文。 |
| **`searchfs(2)` 文件系统 catalog 搜索** | 实测全盘一次 46.8 秒，比 `find` 还慢。「比 find 快 100 倍」是 HFS+ 时代的数字，APFS 换了结构后优势就没了。 |
| **`updatedb` / `locate`** | 内部就是 `find` 全扫、计划任务每周一次、查询要线性解压扫整个文本文件。三个环节全输。 |
| **倒排索引 / trigram** | 内存会膨胀好几倍、更新复杂，而线性扫描本来就只要 6.6 ms。 |

### 全量扫描的 16 秒是物理下限

`kern.maxvnodes` 是 247,308，只装得下全部文件的 7.5%，所以绝大多数目录项都得冷读 SSD 上的 B-tree —— 这是 I/O bound，不是 CPU bound。实测 `sys` 时间 70~90 秒但 wall 固定在 13~16 秒，有效并发度卡在 5（NVMe 的并发深度上限），而 `user` 时间只有 0.6 秒。

试过但**确认无效**的方向：精简 `getattrlistbulk` 索取的属性（sys 降 22%，wall 反而变慢）、改用 `openat` 省去绝对路径重新解析（无改善）、增加线程（8 之后就不再变快）。

唯一有效的优化是修掉 thundering herd —— 原本每扫完一个目录就 `notify_all()`，44.5 万次全体唤醒抢同一把锁，改掉之后 12 线程从 20.3 秒降到 11.8 秒。

## 已知限制

- 索引范围是单个卷（扫描时会挡掉跨 volume 的递归），外接硬盘不会被包含。
- 不索引文件内容，只搜文件名 —— 要搜内容请用 `rg`。
- 不存文件大小与修改时间：那两个字段在 inode record 里，索取它们会让内核对每个文件多做一次 B-tree 查询。搜索结果通常只看几十条，需要时对那几条补一次 `stat` 更划算。
- 事件溢出只会重扫受影响的子树；只有根目录被移走或事件 id 回绕才会整份重建。delta 累积超过 20 万条时也会重建，期间查询仍正常服务。
- 索引文件自己放在监视范围内，daemon 会忽略它的事件 —— 否则重建时写入的上百 MB 会回头触发自己，形成「重建→产生事件→再重建」的循环（实测 60 秒内重建了 7 次）。

## 许可证

MIT，详见 [LICENSE](LICENSE)。
