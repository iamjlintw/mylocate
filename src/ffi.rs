//! macOS 檔案系統批次列舉的 FFI 綁定。
//!
//! 核心是 `getattrlistbulk(2)`：一次系統呼叫就能取回整個目錄下數百筆項目的
//! 名稱與 metadata，取代「readdir 一筆 + stat 一筆」的傳統做法。少掉的那些
//! 系統呼叫正是 `find` 慢的主因。Spotlight 自己的匯入器走的也是這條路。

// 這些常數是 getattrlistbulk 的完整屬性對照表，即使目前只用到其中幾個，
// 留著才能看懂上面的欄位位移推導。
#![allow(dead_code)]

use libc::{c_int, c_void, size_t};

extern "C" {
    /// 批次列舉目錄項目。回傳值為本次填入 `attr_buf` 的項目數，0 代表列舉結束。
    pub fn getattrlistbulk(
        dirfd: c_int,
        attr_list: *mut c_void,
        attr_buf: *mut c_void,
        attr_buf_size: size_t,
        options: u64,
    ) -> c_int;
}

/// `struct attrlist`，用來告訴核心我們想要哪些欄位。
#[repr(C)]
#[derive(Default)]
pub struct Attrlist {
    pub bitmapcount: u16,
    pub reserved: u16,
    pub commonattr: u32,
    pub volattr: u32,
    pub dirattr: u32,
    pub fileattr: u32,
    pub forkattr: u32,
}

/// attrlist 的 bitmap 組數，固定為 5。
pub const ATTR_BIT_MAP_COUNT: u16 = 5;

// --- common 屬性 ---
pub const ATTR_CMN_NAME: u32 = 0x0000_0001;
pub const ATTR_CMN_OBJTYPE: u32 = 0x0000_0008;
pub const ATTR_CMN_MODTIME: u32 = 0x0000_0400;
pub const ATTR_CMN_FILEID: u32 = 0x0200_0000;
pub const ATTR_CMN_ERROR: u32 = 0x2000_0000;
/// 必須索取，且永遠是回傳緩衝區裡的第一個欄位。
pub const ATTR_CMN_RETURNED_ATTRS: u32 = 0x8000_0000;

// --- file 屬性 ---
pub const ATTR_FILE_DATALENGTH: u32 = 0x0000_0200;

// --- 呼叫選項 ---
pub const FSOPT_NOFOLLOW: u64 = 0x0000_0001;
/// 關鍵選項：即使某欄位取不到，也照樣在緩衝區裡佔位（填 0）。
/// 有了它，每筆項目的欄位位移就是固定的，解析可以直接算 offset。
pub const FSOPT_PACK_INVAL_ATTRS: u64 = 0x0000_0008;

// --- vnode 型別 (fsobj_type_t) ---
pub const VREG: u32 = 1;
pub const VDIR: u32 = 2;
pub const VLNK: u32 = 5;

/// 每筆項目的欄位位移表。索取的屬性不同，排列就不同，因此把 attrlist 和它
/// 對應的位移綁在一起，避免兩邊改到不同步。
pub struct Layout {
    pub attrlist: Attrlist,
    /// 保證存在的最小長度，用來擋掉截斷的項目。
    pub min_len: usize,
}

/// 精簡版：只索取「目錄項目本身就有」的欄位。
///
/// APFS 的 dirent 直接帶著名稱、型別與 inode number，取這些欄位不需要另外去
/// 讀 inode record；而 `mtime`／`size` 存在 inode 裡，索取它們會讓核心對每個
/// 檔案多做一次 B-tree 查詢 —— 在數百萬檔案的規模下，這是壓倒性的成本。
/// 因此索引只建名稱與型別，大小與時間留到要顯示結果那幾十筆時才補查。
pub fn build_attrlist_fast() -> Layout {
    Layout {
        attrlist: Attrlist {
            bitmapcount: ATTR_BIT_MAP_COUNT,
            reserved: 0,
            commonattr: ATTR_CMN_RETURNED_ATTRS
                | ATTR_CMN_NAME
                | ATTR_CMN_OBJTYPE
                | ATTR_CMN_FILEID
                | ATTR_CMN_ERROR,
            volattr: 0,
            dirattr: 0,
            fileattr: 0,
            forkattr: 0,
        },
        //  0 len / 4..24 returned / 24 error / 28..36 name / 36 objtype / 40..48 fileid
        min_len: 48,
    }
}

// 每筆項目的欄位位移。以下排列是實際 dump 緩衝區核對出來的，有兩點跟
// 「按 bit 由低到高排列」的直覺推導不同，都踩過坑：
//
//  1. `ATTR_CMN_ERROR` 並不照它的 bit 位置（bit29）排在 FILEID 之後，而是
//     和 RETURNED_ATTRS 一樣屬於特殊欄位，緊接在 returned_attrs 後面。
//     漏掉這點會讓後面每個欄位整體位移 4 bytes。
//  2. `FSOPT_PACK_INVAL_ATTRS` 只在**同一個 attrgroup 內**補位。目錄項目
//     不屬於 file 群組，因此完全不會有 ATTR_FILE_DATALENGTH 這 8 bytes ——
//     目錄的 entry 到 offset 64 就結束了。讀 DATALENGTH 前必須先查
//     returned_attrs 的 fileattr 欄位。
//
//   0  u32          entry_length（整筆長度，含自己）
//   4  [u32; 5]     returned_attrs（commonattr, volattr, dirattr, fileattr, forkattr）
//  24  u32          ATTR_CMN_ERROR
//  28  attrreference_t { i32 offset; u32 length }   ATTR_CMN_NAME
//  36  u32          ATTR_CMN_OBJTYPE
//  40  timespec { i64 sec; i64 nsec }               ATTR_CMN_MODTIME
//  56  u64          ATTR_CMN_FILEID
//  64  u64          ATTR_FILE_DATALENGTH（僅一般檔案才存在）
//
// offset 28 與 64 都沒有自然對齊，核心不會插入 padding，所以一律用
// 非對齊讀取（from_ne_bytes 走 byte slice）來取值。
pub const OFF_NAME_REF: usize = 28;
pub const OFF_OBJTYPE: usize = 36;

/// 從 byte slice 非對齊讀取 u32。
#[inline]
pub fn rd_u32(buf: &[u8], off: usize) -> u32 {
    u32::from_ne_bytes([buf[off], buf[off + 1], buf[off + 2], buf[off + 3]])
}

/// 從 byte slice 非對齊讀取 i32。
#[inline]
pub fn rd_i32(buf: &[u8], off: usize) -> i32 {
    i32::from_ne_bytes([buf[off], buf[off + 1], buf[off + 2], buf[off + 3]])
}

/// 從 byte slice 非對齊讀取 i64。
#[inline]
pub fn rd_i64(buf: &[u8], off: usize) -> i64 {
    let mut b = [0u8; 8];
    b.copy_from_slice(&buf[off..off + 8]);
    i64::from_ne_bytes(b)
}
