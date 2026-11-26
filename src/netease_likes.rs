//! 读取网易云本地「我喜欢的音乐」歌单，用于岛仔红心状态与播放器保持一致。
//! 只读访问 webdb.dat（SQLite），不登录、不联网、不修改用户数据。
use std::collections::HashSet;
use std::ffi::{c_char, c_void, CString};
use std::path::PathBuf;
use std::time::{Duration, Instant};

#[link(name = "winsqlite3")]
extern "C" {
    fn sqlite3_open_v2(name: *const c_char, db: *mut *mut c_void, flags: i32, vfs: *const c_char) -> i32;
    fn sqlite3_close(db: *mut c_void) -> i32;
    fn sqlite3_busy_timeout(db: *mut c_void, milliseconds: i32) -> i32;
    fn sqlite3_prepare_v2(db: *mut c_void, sql: *const c_char, len: i32, statement: *mut *mut c_void, tail: *mut *const c_char) -> i32;
    fn sqlite3_step(statement: *mut c_void) -> i32;
    fn sqlite3_column_text(statement: *mut c_void, column: i32) -> *const u8;
    fn sqlite3_column_bytes(statement: *mut c_void, column: i32) -> i32;
    fn sqlite3_finalize(statement: *mut c_void) -> i32;
}
struct Database(*mut c_void);
impl Drop for Database { fn drop(&mut self) { unsafe { sqlite3_close(self.0); } } }
struct Statement(*mut c_void);
impl Drop for Statement { fn drop(&mut self) { unsafe { sqlite3_finalize(self.0); } } }

const SQLITE_ROW: i32 = 100;
const SQLITE_OK: i32 = 0;
/// 刷新节流：数据库由播放器进程写入，没必要每次绘制都读 127MB 文件。
const REFRESH_EVERY: Duration = Duration::from_secs(3);

fn db_path() -> Option<PathBuf> {
    Some(PathBuf::from(std::env::var_os("LOCALAPPDATA")?).join("NetEase/CloudMusic/Library/webdb.dat"))
}

/// 只读执行一条返回单列文本的查询，逐行回调。
fn query_text(db: *mut c_void, sql: &std::ffi::CStr, mut on_row: impl FnMut(&[u8])) -> bool {
    unsafe {
        let mut statement = std::ptr::null_mut();
        if sqlite3_prepare_v2(db, sql.as_ptr(), -1, &mut statement, std::ptr::null_mut()) != SQLITE_OK { return false; }
        let statement = Statement(statement);
        let mut ok = true;
        while sqlite3_step(statement.0) == SQLITE_ROW {
            let len = sqlite3_column_bytes(statement.0, 0);
            if len <= 0 { continue; }
            let data = sqlite3_column_text(statement.0, 0);
            if data.is_null() { continue; }
            on_row(std::slice::from_raw_parts(data, len as usize));
        }
        ok
    }
}

/// 从 historyPlaylists 找到用户自己的「XX喜欢的音乐」歌单 ID。
/// 历史表里也有别人账号的"喜欢的音乐"歌单（用户访问过），用户自己的总是 playtime 最新，取最新的那个。
fn favorite_playlist_id(db: *mut c_void) -> Option<String> {
    // 按 playtime 降序，最近访问的排前面。
    let sql = c"SELECT jsonStr FROM historyPlaylists ORDER BY playtime DESC";
    let mut found: Option<String> = None;
    let ok = query_text(db, sql, |bytes| {
        if found.is_some() { return; }
        let Ok(value) = serde_json::from_slice::<serde_json::Value>(bytes) else { return; };
        let name = value.get("name").and_then(|v| v.as_str()).unwrap_or_default();
        // 名称形如「Raynor92喜欢的音乐」；只认中文后缀，避免误匹配自建歌单。
        if name.ends_with("喜欢的音乐") {
            if let Some(id) = value.get("id").and_then(|v| v.as_str()) {
                found = Some(id.to_owned());
            }
        }
    });
    if ok { found } else { None }
}

/// 读取「我喜欢的音乐」歌单的全部歌曲 ID。
fn read_from_db() -> Option<HashSet<u64>> {
    let path = db_path()?;
    let path_c = CString::new(path.to_str()?).ok()?;
    unsafe {
        let mut db_ptr = std::ptr::null_mut();
        if sqlite3_open_v2(path_c.as_ptr(), &mut db_ptr, 1, std::ptr::null()) != SQLITE_OK || db_ptr.is_null() { return None; }
        let db = Database(db_ptr);
        sqlite3_busy_timeout(db.0, 50);
        let playlist_id = favorite_playlist_id(db.0)?;
        let sql = c"SELECT jsonStr FROM playlistTrackIds WHERE id=?1";
        let mut statement = std::ptr::null_mut();
        if sqlite3_prepare_v2(db.0, sql.as_ptr(), -1, &mut statement, std::ptr::null_mut()) != SQLITE_OK { return None; }
        let statement = Statement(statement);
        let id_c = CString::new(playlist_id).ok()?;
        // 绑定参数（避免拼接 SQL），用系统 sqlite3_bind_text。
        sqlite3_bind_text(statement.0, 1, id_c.as_ptr(), -1, None);
        let mut ids = HashSet::new();
        while sqlite3_step(statement.0) == SQLITE_ROW {
            let len = sqlite3_column_bytes(statement.0, 0);
            if len <= 0 { continue; }
            let data = sqlite3_column_text(statement.0, 0);
            if data.is_null() { continue; }
            let Ok(value) = serde_json::from_slice::<serde_json::Value>(std::slice::from_raw_parts(data, len as usize)) else { continue; };
            let Some(tracks) = value.get("trackIds").and_then(|v| v.as_array()) else { continue; };
            let Some(playlist) = value.get("id").and_then(|v| v.as_str()) else { continue; };
            if playlist != id_c.to_str().unwrap_or_default() { continue; }
            for track in tracks {
                if let Some(id) = track.get("id").and_then(|v| v.as_str()).and_then(|s| s.parse::<u64>().ok()) {
                    ids.insert(id);
                }
            }
        }
        Some(ids)
    }
}

extern "C" {
    fn sqlite3_bind_text(statement: *mut c_void, index: i32, value: *const c_char, n: i32, destructor: Option<unsafe extern "C" fn(*mut c_void)>) -> i32;
}

/// 带节流的收藏状态查询器：缓存结果，数据库文件变化或超过 3 秒才重读。
/// 用法：每帧（或定时）调用 `refresh()`，绘制时用 `is_liked()` 只读查询。
#[derive(Default)]
pub struct NeteaseLikes {
    ids: HashSet<u64>,
    last_read: Option<Instant>,
    last_mtime: Option<std::time::SystemTime>,
    available: bool,
}

impl NeteaseLikes {
    /// 数据库是否可用（能否读到「我喜欢的音乐」歌单）。
    pub fn available(&self) -> bool { self.available }

    /// 只读查询：当前歌曲是否被网易云标记为喜欢。数据库不可用或 song_id 缺失时返回 None。
    pub fn is_liked(&self, song_id: Option<u64>) -> Option<bool> {
        if !self.available { return None; }
        let id = song_id?;
        Some(self.ids.contains(&id))
    }

    /// 按节流策略刷新缓存，可安全每帧调用。
    pub fn refresh(&mut self) {
        let path = match db_path() { Some(p) => p, None => { self.available = false; return; } };
        let mtime = std::fs::metadata(&path).and_then(|m| m.modified()).ok();
        let fresh = self.last_read.is_some_and(|t| t.elapsed() < REFRESH_EVERY)
            && mtime == self.last_mtime;
        if fresh { return; }
        if let Some(ids) = read_from_db() {
            self.ids = ids;
            self.available = true;
        } else {
            self.available = false;
        }
        self.last_read = Some(Instant::now());
        self.last_mtime = mtime;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn favorite_name_matches_chinese_suffix() {
        // 只认「XX喜欢的音乐」这种名称格式。
        let is_fav = |name: &str| name.ends_with("喜欢的音乐");
        assert!(is_fav("Raynor92喜欢的音乐"));
        assert!(is_fav("张三喜欢的音乐"));
        assert!(!is_fav("我喜欢的摇滚"));
        assert!(!is_fav("喜欢的音乐清单"));
    }
}

