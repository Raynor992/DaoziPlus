//! 音乐页「喜欢」按钮：本地收藏列表 + 尽力把收藏动作转发给当前播放器。
//!
//! Windows 的媒体会话（SMTC）没有「喜欢 / 收藏」指令，无法像切歌那样统一下发。
//! 因此这里做两件事：
//! 1. 岛仔自己的 `liked-songs.json` 持久化收藏状态，任何播放器都可靠、切歌自动识别；
//! 2. 点击时向系统发送当前播放器的「喜欢」快捷键（各客户端默认键 Ctrl+Alt+字母），
//!    由播放器把这首歌写进它账号的「我喜欢」。快捷键可能被用户改过或未启用全局，
//!    所以这一步是「尽力而为」，本地列表始终是按钮显示的准绳。

use std::collections::HashSet;
use std::path::PathBuf;

/// 收藏列表，按 `标题|歌手` 归一化后的键保存。
#[derive(Default, Clone, Debug)]
pub struct Likes {
    keys: HashSet<String>,
    path: Option<PathBuf>,
}

fn song_key(title: &str, artist: &str) -> String {
    let norm = |s: &str| s.trim().to_lowercase();
    format!("{}|{}", norm(title), norm(artist))
}

impl Likes {
    fn path() -> PathBuf { crate::model::config_dir().join("liked-songs.json") }

    pub fn load() -> Self { Self::load_from(Self::path()) }

    fn load_from(path: PathBuf) -> Self {
        let raw = std::fs::read(&path).unwrap_or_default();
        let mut keys = HashSet::new();
        if let Ok(value) = serde_json::from_slice::<serde_json::Value>(&raw) {
            if let Some(list) = value.as_array() {
                for item in list {
                    if let Some(key) = item.as_str() {
                        if !key.is_empty() { keys.insert(key.to_string()); }
                    }
                }
            }
        }
        Self { keys, path: Some(path) }
    }

    fn save(&self) {
        let mut list: Vec<&str> = self.keys.iter().map(String::as_str).collect();
        list.sort_unstable();
        let body = serde_json::to_string(&list).unwrap_or_else(|_| "[]".into());
        if let Some(path) = &self.path {
            if let Some(parent) = path.parent() { let _ = std::fs::create_dir_all(parent); }
            let _ = crate::model::atomic_write(path, body.as_bytes());
        }
    }

    pub fn is_liked(&self, title: &str, artist: &str) -> bool {
        !title.trim().is_empty() && self.keys.contains(&song_key(title, artist))
    }

    /// 切换收藏状态，返回切换后是否为「已喜欢」。
    pub fn toggle(&mut self, title: &str, artist: &str) -> bool {
        let key = song_key(title, artist);
        if key == "|" { return false; }
        let liked = if self.keys.remove(&key) { false } else { self.keys.insert(key); true };
        self.save();
        liked
    }

    pub fn len(&self) -> usize { self.keys.len() }
}

/// 当前播放器「喜欢」快捷键的字母键；None 表示没有已知映射。
/// 默认来源：网易云 Ctrl+Alt+L、QQ 音乐 Ctrl+Alt+V、酷狗 / 汽水 Ctrl+Alt+F。
/// 用户若在客户端里改过快捷键会失效，属预期限制。
pub(crate) fn like_key(source: &str) -> Option<u8> {
    let s = source.to_ascii_lowercase();
    if s.contains("cloudmusic") { return Some(b'L'); }
    if crate::smtc::is_qqmusic(&s) { return Some(b'V'); }
    if s.contains("kugou") || s.contains("soda") { return Some(b'F'); }
    None
}

/// 向系统发送当前播放器的「喜欢」快捷键（Ctrl+Alt+字母）。
/// 只是尽力同步，返回是否找到映射并成功投递；按钮状态不依赖此结果。
#[cfg(windows)]
pub fn notify_player(source: &str) -> bool {
    use windows::Win32::UI::Input::KeyboardAndMouse::{
        SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYEVENTF_KEYUP,
        KEYBD_EVENT_FLAGS, VIRTUAL_KEY, VK_CONTROL, VK_MENU,
    };
    let Some(letter) = like_key(source) else { return false; };
    // A..Z 的虚拟键码连续，字母直接换算，避免受输入法布局影响。
    let letter_vk = VIRTUAL_KEY(letter as u16);

    let key = |v: VIRTUAL_KEY, up: bool| INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 { ki: KEYBDINPUT {
            wVk: v,
            wScan: 0,
            dwFlags: if up { KEYEVENTF_KEYUP } else { KEYBD_EVENT_FLAGS(0) },
            time: 0,
            dwExtraInfo: 0,
        }},
    };
    let inputs = [
        key(VK_CONTROL, false), key(VK_MENU, false), key(letter_vk, false),
        key(letter_vk, true), key(VK_MENU, true), key(VK_CONTROL, true),
    ];
    unsafe { SendInput(&inputs, std::mem::size_of::<INPUT>() as i32) == inputs.len() as u32 }
}

#[cfg(not(windows))]
pub fn notify_player(_source: &str) -> bool { false }

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn like_key_maps_major_players() {
        assert_eq!(like_key("cloudmusic.exe"), Some(b'L'));
        assert_eq!(like_key("QQMusic.exe"), Some(b'V'));
        assert_eq!(like_key("Tencent.QQMusic"), Some(b'V'));
        assert_eq!(like_key("kugou.exe"), Some(b'F'));
        assert_eq!(like_key("SodaMusic.exe"), Some(b'F'));
        assert_eq!(like_key("bodian_pc.exe"), None);
        assert_eq!(like_key(""), None);
    }

    #[test]
    fn likes_toggle_and_persist_in_temp_dir() {
        let dir = std::env::temp_dir().join(format!("daozai-like-test-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let file = dir.join("liked-songs.json");
        std::fs::write(&file, "[\"晴天|周杰伦\"]").unwrap();

        let mut likes = Likes::load_from(file.clone());
        assert!(likes.is_liked("晴天", "周杰伦"));
        assert!(likes.is_liked("晴天", "周杰伦 "), "键应归一化忽略空白");
        assert!(likes.toggle("浪漫爱", "刘瑞琦"));
        assert!(likes.is_liked("浪漫爱", "刘瑞琦"));
        assert!(!likes.toggle("浪漫爱", "刘瑞琦"));
        assert!(!likes.is_liked("浪漫爱", "刘瑞琦"));
        let raw = std::fs::read_to_string(&file).unwrap();
        assert!(raw.contains("晴天|周杰伦"), "持久化应保留已有项：{raw}");
        assert!(!raw.contains("浪漫爱"), "取消后应写回：{raw}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
