//! FSEvents 綁定 —— macOS 上等同於 NTFS USN Journal 的變更日誌。
//!
//! Everything 之所以能維持索引新鮮，靠的是 NTFS 的 USN Journal；macOS 這邊的
//! 對應物就是 FSEvents，而且具備兩個關鍵能力：
//!
//! * **歷史重放**：把上次存下的 `event_id` 當作 `sinceWhen` 傳入，開機後就能補齊
//!   daemon 沒在跑的那段期間所有變更，不必重掃。
//! * **失效偵測**：每個磁碟區的事件資料庫有一個 UUID，資料庫一旦被丟棄重建，
//!   UUID 就會變 —— 這時舊的 `event_id` 不可信，必須退回全量重掃。
//!
//! 這兩者搭配起來，才能安全地讓那 13 秒的全量掃描一輩子只付一次。

// 旗標常數保留完整一組，方便對照 FSEvents.h 判讀事件。
#![allow(non_upper_case_globals, non_snake_case, dead_code)]

use libc::{c_void, dev_t};
use std::sync::mpsc::Sender;

pub type CFAllocatorRef = *const c_void;
pub type CFStringRef = *const c_void;
pub type CFArrayRef = *const c_void;
pub type CFRunLoopRef = *const c_void;
pub type CFUUIDRef = *const c_void;
pub type FSEventStreamRef = *mut c_void;
pub type CFIndex = isize;

const kCFStringEncodingUTF8: u32 = 0x0800_0100;

// --- 建立串流時的旗標 ---
/// 事件以「單一檔案」而非「整個目錄」為單位回報。
pub const FLAG_FILE_EVENTS: u32 = 0x0000_0010;
/// 不延遲第一批事件。
pub const FLAG_NO_DEFER: u32 = 0x0000_0002;
/// 被監看的根目錄自己被搬動時也通知。
pub const FLAG_WATCH_ROOT: u32 = 0x0000_0004;

// --- 事件旗標 ---
/// 事件被丟棄，該子樹的內容不可信，必須重掃。
pub const EV_MUST_SCAN_SUBDIRS: u32 = 0x0000_0001;
pub const EV_USER_DROPPED: u32 = 0x0000_0002;
pub const EV_KERNEL_DROPPED: u32 = 0x0000_0004;
/// 事件 id 溢位回繞，舊的 id 不再具可比性。
pub const EV_IDS_WRAPPED: u32 = 0x0000_0008;
/// 歷史事件重放完畢的哨兵，之後才是即時事件。
pub const EV_HISTORY_DONE: u32 = 0x0000_0010;
pub const EV_ROOT_CHANGED: u32 = 0x0000_0020;
pub const EV_MOUNT: u32 = 0x0000_0040;
pub const EV_UNMOUNT: u32 = 0x0000_0080;
pub const EV_ITEM_CREATED: u32 = 0x0000_0100;
pub const EV_ITEM_REMOVED: u32 = 0x0000_0200;
pub const EV_ITEM_RENAMED: u32 = 0x0000_0800;
pub const EV_ITEM_MODIFIED: u32 = 0x0000_1000;
pub const EV_ITEM_IS_FILE: u32 = 0x0001_0000;
pub const EV_ITEM_IS_DIR: u32 = 0x0002_0000;
pub const EV_ITEM_IS_SYMLINK: u32 = 0x0004_0000;

/// 代表「從現在開始」的哨兵值，等同 header 裡的 kFSEventStreamEventIdSinceNow。
pub const SINCE_NOW: u64 = 0xFFFF_FFFF_FFFF_FFFF;

#[repr(C)]
struct FSEventStreamContext {
    version: CFIndex,
    info: *mut c_void,
    retain: *const c_void,
    release: *const c_void,
    copyDescription: *const c_void,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct CFUUIDBytes {
    b: [u8; 16],
}

type FSEventStreamCallback = extern "C" fn(
    stream: *const c_void,
    info: *mut c_void,
    num_events: usize,
    // 未指定 UseCFTypes 時，這裡是 char** 的 C 字串陣列，直接取用即可。
    event_paths: *mut c_void,
    event_flags: *const u32,
    event_ids: *const u64,
);

#[link(name = "CoreServices", kind = "framework")]
extern "C" {
    fn CFStringCreateWithBytes(
        alloc: CFAllocatorRef,
        bytes: *const u8,
        numBytes: CFIndex,
        encoding: u32,
        isExternalRepresentation: u8,
    ) -> CFStringRef;
    fn CFArrayCreate(
        alloc: CFAllocatorRef,
        values: *const *const c_void,
        numValues: CFIndex,
        callBacks: *const c_void,
    ) -> CFArrayRef;
    fn CFRelease(cf: *const c_void);
    fn CFRunLoopGetCurrent() -> CFRunLoopRef;
    fn CFRunLoopRun();

    static kCFTypeArrayCallBacks: c_void;
    static kCFRunLoopDefaultMode: CFStringRef;

    fn FSEventStreamCreate(
        allocator: CFAllocatorRef,
        callback: FSEventStreamCallback,
        context: *mut FSEventStreamContext,
        pathsToWatch: CFArrayRef,
        sinceWhen: u64,
        latency: f64,
        flags: u32,
    ) -> FSEventStreamRef;
    fn FSEventStreamScheduleWithRunLoop(
        stream: FSEventStreamRef,
        runLoop: CFRunLoopRef,
        runLoopMode: CFStringRef,
    );
    fn FSEventStreamStart(stream: FSEventStreamRef) -> u8;
    fn FSEventsGetCurrentEventId() -> u64;
    fn FSEventsCopyUUIDForDevice(dev: dev_t) -> CFUUIDRef;
    fn CFUUIDGetUUIDBytes(uuid: CFUUIDRef) -> CFUUIDBytes;
}

/// 單一檔案系統事件。
#[derive(Clone)]
pub struct Event {
    pub path: Vec<u8>,
    pub flags: u32,
    pub id: u64,
}

/// 取得目前系統全域的最新事件 id，存進索引當作下次的續傳點。
pub fn current_event_id() -> u64 {
    unsafe { FSEventsGetCurrentEventId() }
}

/// 取得某個裝置的事件資料庫 UUID。回傳全 0 代表取不到（該磁碟區沒有事件日誌），
/// 這種情況下不能信任任何存下來的 event_id。
pub fn device_uuid(dev: dev_t) -> [u8; 16] {
    unsafe {
        let u = FSEventsCopyUUIDForDevice(dev);
        if u.is_null() {
            return [0u8; 16];
        }
        let bytes = CFUUIDGetUUIDBytes(u);
        CFRelease(u);
        bytes.b
    }
}

/// 回呼時傳給 C 的上下文，透過 `info` 指標帶進 callback。
struct CallbackCtx {
    tx: Sender<Vec<Event>>,
}

extern "C" fn stream_callback(
    _stream: *const c_void,
    info: *mut c_void,
    num_events: usize,
    event_paths: *mut c_void,
    event_flags: *const u32,
    event_ids: *const u64,
) {
    if info.is_null() || num_events == 0 {
        return;
    }
    let ctx = unsafe { &*(info as *const CallbackCtx) };
    let paths = event_paths as *const *const libc::c_char;

    let mut batch = Vec::with_capacity(num_events);
    for i in 0..num_events {
        unsafe {
            let p = *paths.add(i);
            if p.is_null() {
                continue;
            }
            let bytes = std::ffi::CStr::from_ptr(p).to_bytes().to_vec();
            batch.push(Event {
                path: bytes,
                flags: *event_flags.add(i),
                id: *event_ids.add(i),
            });
        }
    }
    // 接收端已關閉就無事可做，忽略錯誤即可。
    let _ = ctx.tx.send(batch);
}

/// 在目前執行緒上啟動監看並進入 run loop（不會返回）。
///
/// `since` 傳入上次存下的 event_id 以重放離線期間的變更，或 `SINCE_NOW`
/// 表示只要之後的新事件。`latency` 是核心合併事件的秒數，調小則更即時。
pub fn watch_forever(paths: &[&str], since: u64, latency: f64, tx: Sender<Vec<Event>>) {
    unsafe {
        let cf_paths: Vec<CFStringRef> = paths
            .iter()
            .map(|p| {
                CFStringCreateWithBytes(
                    std::ptr::null(),
                    p.as_ptr(),
                    p.len() as CFIndex,
                    kCFStringEncodingUTF8,
                    0,
                )
            })
            .collect();
        let array = CFArrayCreate(
            std::ptr::null(),
            cf_paths.as_ptr() as *const *const c_void,
            cf_paths.len() as CFIndex,
            &kCFTypeArrayCallBacks as *const c_void,
        );

        // ctx 必須活得比串流久，所以刻意洩漏 —— daemon 生命週期內都需要它。
        let ctx = Box::into_raw(Box::new(CallbackCtx { tx }));
        let mut context = FSEventStreamContext {
            version: 0,
            info: ctx as *mut c_void,
            retain: std::ptr::null(),
            release: std::ptr::null(),
            copyDescription: std::ptr::null(),
        };

        let stream = FSEventStreamCreate(
            std::ptr::null(),
            stream_callback,
            &mut context,
            array,
            since,
            latency,
            FLAG_FILE_EVENTS | FLAG_NO_DEFER | FLAG_WATCH_ROOT,
        );
        for p in cf_paths {
            CFRelease(p);
        }
        CFRelease(array);

        if stream.is_null() {
            eprintln!("FSEventStreamCreate 失敗");
            return;
        }
        FSEventStreamScheduleWithRunLoop(stream, CFRunLoopGetCurrent(), kCFRunLoopDefaultMode);
        if FSEventStreamStart(stream) == 0 {
            eprintln!("FSEventStreamStart 失敗");
            return;
        }
        CFRunLoopRun();
    }
}
