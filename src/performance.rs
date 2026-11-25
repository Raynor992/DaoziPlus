//! 性能页：CPU / GPU / 内存 / 网络实时占用。
//!
//! 采样放在独立线程里，每秒更新一次；界面只读快照，不在 UI 线程做系统调用。
//! 视觉上复用岛仔的 Card 画布、字号表和主题色，作为普通功能页融入翻页序列。

use std::collections::VecDeque;
use std::f32::consts::{FRAC_PI_2, TAU};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use eframe::egui::Color32;

use crate::app::App;
use crate::paint::{self, Card, H};
use crate::theme::{self, W};

const HISTORY: usize = 28;

/// 岛上性能环的直径（逻辑点）。收起态与展开态共用同一套配色与画法。
pub(crate) const RING_DIAM: f32 = 22.0;
/// 四个独立小圆的直径（收起态）。
pub(crate) const PIPS_DIAM: f32 = 15.0;
/// 四个小圆之间的间距（收起态）。
pub(crate) const PIPS_GAP: f32 = 3.0;
/// 常驻态为四个性能小圆让出的岛宽：四圆实占宽度加左右各 2 点留白。
pub(crate) const PIPS_SLOT_W: f32 = 4.0 * PIPS_DIAM + 3.0 * PIPS_GAP + 4.0;
/// 收起态歌词栏为性能环让出的宽度。
pub(crate) const RING_GAP: f32 = 30.0;

/// 界面读取的一份快照。所有占用率都是 0..100，速度为字节/秒。
#[derive(Clone, Debug)]
pub struct Snapshot {
    pub cpu: f32,
    pub gpu: Option<f32>,
    pub gpu_mem: Option<(f32, f32)>, // 已用 / 总量，GB
    pub mem: f32,
    pub mem_used: f32,
    pub mem_total: f32,
    pub rx: f32,
    pub tx: f32,
    pub at: f64,
}

impl Default for Snapshot {
    fn default() -> Self {
        Self {
            cpu: 0.0,
            gpu: None,
            gpu_mem: None,
            mem: 0.0,
            mem_used: 0.0,
            mem_total: 0.0,
            rx: 0.0,
            tx: 0.0,
            at: -1.0,
        }
    }
}

#[derive(Default)]
struct History {
    at: f64,
    cpu: VecDeque<f32>,
    gpu: VecDeque<f32>,
    mem: VecDeque<f32>,
    net: VecDeque<f32>,
}

/// 给迷你曲线用的定长序列。
pub struct Series {
    cpu: Vec<f32>,
    gpu: Vec<f32>,
    mem: Vec<f32>,
    net: Vec<f32>,
}

/// 性能监视器：后台采样 + 界面侧历史曲线。
#[derive(Clone)]
pub struct Monitor {
    shared: Arc<Mutex<Snapshot>>,
    history: Arc<Mutex<History>>,
}

impl Monitor {
    pub fn new() -> Self {
        let shared = Arc::new(Mutex::new(Snapshot::default()));
        #[cfg(not(test))]
        {
            let sink = Arc::clone(&shared);
            let _ = std::thread::Builder::new()
                .name("daozai-perf".into())
                .spawn(move || sample_loop(sink));
        }
        Self {
            shared,
            history: Arc::new(Mutex::new(History::default())),
        }
    }

    pub fn snapshot(&self) -> Snapshot {
        self.shared.lock().map(|s| s.clone()).unwrap_or_default()
    }

    /// 仅供渲染预览测试：构造一个带固定读数的监视器，让小圆显示彩色进度弧。
    #[cfg(test)]
    pub fn from_snapshot(snap: Snapshot) -> Self {
        Self {
            shared: Arc::new(Mutex::new(snap)),
            history: Arc::new(Mutex::new(History::default())),
        }
    }

    /// 每次快照更新时推进一次历史；页面关闭期间不推进，重新打开从当前值继续。
    pub fn series(&self, snap: &Snapshot) -> Series {
        let mut h = self.history.lock().map(|h| h).unwrap_or_else(|e| e.into_inner());
        if snap.at != h.at {
            h.at = snap.at;
            h.cpu.push_back(snap.cpu);
            h.gpu.push_back(snap.gpu.unwrap_or(0.0));
            h.mem.push_back(snap.mem);
            h.net.push_back(((snap.rx + snap.tx) / 1024.0 / 1024.0).min(100.0));
            while h.cpu.len() > HISTORY { h.cpu.pop_front(); }
            while h.gpu.len() > HISTORY { h.gpu.pop_front(); }
            while h.mem.len() > HISTORY { h.mem.pop_front(); }
            while h.net.len() > HISTORY { h.net.pop_front(); }
        }
        Series {
            cpu: h.cpu.iter().copied().collect(),
            gpu: h.gpu.iter().copied().collect(),
            mem: h.mem.iter().copied().collect(),
            net: h.net.iter().copied().collect(),
        }
    }
}

fn sample_loop(shared: Arc<Mutex<Snapshot>>) {
    use sysinfo::{Networks, System};

    let mut sys = System::new();
    let mut nets = Networks::new_with_refreshed_list();
    let total_net = |nets: &Networks| -> (u64, u64) {
        let mut rx = 0u64;
        let mut tx = 0u64;
        for (_, data) in nets.iter() {
            rx = rx.saturating_add(data.total_received());
            tx = tx.saturating_add(data.total_transmitted());
        }
        (rx, tx)
    };
    let mut prev_net = total_net(&nets);
    let mut prev = Instant::now();
    let started = prev;
    let mut tick = 0u32;
    let mut gpu = (None, None);

    loop {
        std::thread::sleep(Duration::from_secs(1));
        sys.refresh_cpu_usage();
        sys.refresh_memory();
        nets.refresh(true);

        let now = Instant::now();
        let secs = now.duration_since(prev).as_secs_f32().max(0.001);
        prev = now;
        let current = total_net(&nets);
        let rx = current.0.saturating_sub(prev_net.0) as f32 / secs;
        let tx = current.1.saturating_sub(prev_net.1) as f32 / secs;
        prev_net = current;

        tick = tick.wrapping_add(1);
        if tick % 3 == 1 {
            gpu = query_gpu();
        }

        let total = sys.total_memory() as f32;
        let used = sys.used_memory() as f32;
        let snap = Snapshot {
            cpu: sys.global_cpu_usage(),
            gpu: gpu.0,
            gpu_mem: gpu.1,
            mem: if total > 0.0 { used / total * 100.0 } else { 0.0 },
            mem_used: used / 1024.0 / 1024.0 / 1024.0,
            mem_total: total / 1024.0 / 1024.0 / 1024.0,
            rx,
            tx,
            at: now.duration_since(started).as_secs_f64(),
        };
        if let Ok(mut sink) = shared.lock() {
            *sink = snap;
        }
    }
}

/// GPU 占用采集入口：先试 NVIDIA 官方工具（顺带拿显存），
/// 失败则用 Windows 原生 GPU Engine 性能计数器，兼容 AMD / Intel 等所有 WDDM 显卡。
/// 两条路都不通时返回空，G 小圆显示「—」，不影响其余三项。
fn query_gpu() -> (Option<f32>, Option<(f32, f32)>) {
    let nvidia = query_nvidia();
    if nvidia.0.is_some() {
        return nvidia;
    }
    #[cfg(windows)]
    {
        let util = query_gpu_counter();
        if util.is_some() {
            return (util, None);
        }
    }
    (None, None)
}

/// 用 PDH 读取 `\GPU Engine(*)\Utilization Percentage` 的 3D 引擎实例并求和。
/// 这是 Windows 任务管理器 GPU 曲线的数据源，N / A / Intel 显卡统一适用。
/// 计数是「每秒变化率」，必须采集两次才有有效值，所以首次调用会积一次样本。
#[cfg(windows)]
fn query_gpu_counter() -> Option<f32> {
    use windows::Win32::System::Performance::{
        PdhAddEnglishCounterW, PdhCloseQuery, PdhCollectQueryData,
        PdhGetFormattedCounterArrayW, PdhOpenQueryW, PDH_FMT_COUNTERVALUE_ITEM_W,
        PDH_FMT_DOUBLE, PDH_MORE_DATA,
    };
    use windows::core::PCWSTR;

    const PATH: &str = "\\GPU Engine(*)\\Utilization Percentage";

    unsafe {
        let mut query: isize = 0;
        if PdhOpenQueryW(PCWSTR::null(), 0, &mut query) != 0 {
            return None;
        }

        let path: Vec<u16> = PATH.encode_utf16().chain(std::iter::once(0)).collect();
        let mut counter: isize = 0;
        let added = PdhAddEnglishCounterW(query, PCWSTR(path.as_ptr()), 0, &mut counter) == 0;
        if !added {
            let _ = PdhCloseQuery(query);
            return None;
        }

        // 两次采集之间留出一小段时间，否则rate型计数器读数为 0。
        let _ = PdhCollectQueryData(query);
        std::thread::sleep(Duration::from_millis(120));
        if PdhCollectQueryData(query) != 0 {
            let _ = PdhCloseQuery(query);
            return None;
        }

        let mut size = 0u32;
        let mut count = 0u32;
        let rc = PdhGetFormattedCounterArrayW(
            counter, PDH_FMT_DOUBLE, &mut size, &mut count, None,
        );
        if rc != PDH_MORE_DATA || size == 0 {
            let _ = PdhCloseQuery(query);
            return None;
        }
        // size 含字符串数据、不一定是结构体的整数倍，必须向上取整，
        // 否则 PDH 按 size 写入时会越过 vec 末尾，造成堆损坏。
        let slots = size as usize / std::mem::size_of::<PDH_FMT_COUNTERVALUE_ITEM_W>() + 1;
        let mut items = vec![PDH_FMT_COUNTERVALUE_ITEM_W::default(); slots];
        let rc = PdhGetFormattedCounterArrayW(
            counter, PDH_FMT_DOUBLE, &mut size, &mut count, Some(items.as_mut_ptr()),
        );
        if rc != 0 || count == 0 {
            let _ = PdhCloseQuery(query);
            return None;
        }

        // 只统计 3D 引擎实例：这是任务管理器「GPU 利用率」的主口径，
        // 排除 Copy / VideoDecode 等专用引擎，避免把媒体解码算成核心占用。
        // 注意 szName 指向 PDH 内部缓冲区，必须在关闭 query 之前读完。
        let mut total = 0.0f64;
        for item in items.iter().take(count as usize) {
            if item.FmtValue.CStatus != 0 || item.szName.0.is_null() {
                continue;
            }
            let name_ptr = item.szName.0;
            let mut len = 0usize;
            while *name_ptr.add(len) != 0 && len < 256 {
                len += 1;
            }
            let name = String::from_utf16_lossy(std::slice::from_raw_parts(name_ptr, len));
            if name.to_ascii_lowercase().contains("engtype_3d") {
                total += item.FmtValue.Anonymous.doubleValue.max(0.0);
            }
        }
        let _ = PdhCloseQuery(query);
        if total < 0.0 || !total.is_finite() {
            return None;
        }
        Some((total.min(100.0)) as f32)
    }
}

/// 优先读取 NVIDIA 显卡；没有 nvidia-smi 时显示「—」，不影响其余三项。
fn query_nvidia() -> (Option<f32>, Option<(f32, f32)>) {
    let mut cmd = std::process::Command::new("nvidia-smi");
    cmd.args([
        "--query-gpu=utilization.gpu,memory.used,memory.total",
        "--format=csv,noheader,nounits",
    ]);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    let out = match cmd.output() {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).to_string(),
        _ => return (None, None),
    };
    let parts: Vec<&str> = out
        .lines()
        .next()
        .unwrap_or("")
        .split(',')
        .map(str::trim)
        .collect();
    if parts.len() < 3 {
        return (None, None);
    }
    let util = parts[0].parse::<f32>().ok().filter(|v| *v >= 0.0);
    let used = parts[1].parse::<f32>().ok();
    let total = parts[2].parse::<f32>().ok();
    let mem = match (used, total) {
        (Some(u), Some(t)) if t > 0.0 => Some((u / 1024.0, t / 1024.0)),
        _ => None,
    };
    (util, mem)
}

fn ring_value(c: &Card, cx: f32, cy: f32, r: f32, width: f32, frac: f32, color: Color32) {
    // 暗色底环 + 亮色进度弧，圆头收尾，和专注页的进度环画法一致。
    c.ring(cx, cy, r, width, Color32::from_rgb(46, 46, 50));
    let frac = frac.clamp(0.0, 1.0);
    if frac <= 0.001 { return; }
    let a1 = -FRAC_PI_2 + TAU * frac;
    c.arc(cx, cy, r, -FRAC_PI_2, a1, width, color);
    c.circle(cx + r * a1.cos(), cy + r * a1.sin(), width * 0.5, color);
    c.circle(cx, cy - r, width * 0.5, color);
}

impl App {
    /// 同心性能环：外圈 CPU、中圈内存、内圈网络，GPU 占用时最内再套一圈。
    /// 收起态与展开的音乐卡共用，保证风格统一。
    pub(crate) fn paint_perf_rings(&self, c: &Card, cx: f32, cy: f32, diam: f32) {
        let snap = self.perf.snapshot();
        let s = diam / RING_DIAM;
        let width = 2.6 * s;
        ring_value(c, cx, cy, 9.2 * s, width, snap.cpu / 100.0, theme::BLUE);
        ring_value(c, cx, cy, 6.3 * s, width, snap.mem / 100.0, theme::YELLOW);
        let net = ((snap.rx + snap.tx) / 1024.0 / 1024.0 / 8.0).min(1.0);
        ring_value(c, cx, cy, 3.4 * s, width, net, theme::CYAN);
        // 中心点：检测到 GPU 时按占用混色，检测不到时用灰色小点。
        let gpu = snap.gpu.map(|v| {
            paint::mix(Color32::from_rgb(70, 70, 74), theme::SOFT_GREEN, (v / 100.0).clamp(0.0, 1.0))
        }).unwrap_or(Color32::from_rgb(70, 70, 74));
        c.circle(cx, cy, 1.5 * s, gpu);
    }

    /// 四个独立性能小圆：CPU / 内存 / 网络 / GPU 各一个，圆心带缩写字母。
    /// `cx`/`cy` 是整体中心，`cols` 为每行个数（4=横排一列，2=两行两列）。
    pub(crate) fn paint_perf_pips(&self, c: &Card, cx: f32, cy: f32, diam: f32, gap: f32, cols: usize) {
        let snap = self.perf.snapshot();
        let s = diam / PIPS_DIAM;
        let ring_w = 2.2 * s;
        let r = (diam - ring_w) / 2.0;
        let net = ((snap.rx + snap.tx) / 1024.0 / 1024.0 / 8.0).clamp(0.0, 1.0);
        let gpu_frac = snap.gpu.map(|v| (v / 100.0).clamp(0.0, 1.0));
        let pips = [
            ("C", snap.cpu / 100.0, theme::BLUE, true),
            ("M", snap.mem / 100.0, theme::YELLOW, true),
            ("N", net, theme::CYAN, true),
            ("G", gpu_frac.unwrap_or(0.0), theme::SOFT_GREEN, gpu_frac.is_some()),
        ];
        let step = diam + gap;
        let rows = pips.len().div_ceil(cols);
        for (i, (label, frac, color, detected)) in pips.iter().enumerate() {
            let (col, row) = (i % cols, i / cols);
            let px = cx + (col as f32 - (cols as f32 - 1.0) / 2.0) * step;
            let py = cy + (row as f32 - (rows as f32 - 1.0) / 2.0) * step;
            let ink = if *detected { *color } else { Color32::from_rgb(92, 92, 96) };
            ring_value(c, px, py, r, ring_w, *frac, ink);
            c.centered_text(px, py, label, 7.5 * s, W::Bold, ink, H::Center);
        }
    }

    /// 性能环的悬停说明：四项读数集中一处，避免在小岛上堆数字。
    /// G 小圆读的是 GPU 核心占用率，显存占用在完整性能页的 GPU 卡里单独一行。
    pub(crate) fn perf_ring_tip(&self) -> String {
        let s = self.perf.snapshot();
        let gpu = s.gpu.map(|v| format!("{v:.0}%")).unwrap_or_else(|| "—".into());
        format!("性能 · CPU {:.0}% · 内存 {:.0}% · GPU 核心 {gpu}\n↓{} ↑{} · 点击查看完整性能页",
            s.cpu, s.mem, fmt_speed(s.rx), fmt_speed(s.tx))
    }
}

impl App {
    /// 性能页：四张紧凑指标卡 + 迷你曲线，沿用岛仔现有的圆角、字号和配色。
    pub(crate) fn page_perf(&mut self, c: &Card) {
        let snap = self.perf.snapshot();
        let series = self.perf.series(&snap);

        c.text(15.0, 21.0, "性能", theme::size::TITLE, W::Bold, theme::WHITE, H::Left);
        c.circle(59.0, 17.3, 2.6, theme::SOFT_GREEN);
        c.text(65.0, 21.0, "实时 · 1 秒", theme::size::TINY, W::Regular, theme::GRAY, H::Left);

        let cpu_value = format!("{:.0}%", snap.cpu);
        let gpu_value = snap.gpu.map(|v| format!("{v:.0}%")).unwrap_or_else(|| "—".into());
        let gpu_sub = match snap.gpu_mem {
            Some((u, t)) => format!("显存 {u:.1}/{t:.1} GB"),
            None if snap.gpu.is_some() => "通用检测 · 显存 —".to_string(),
            None => "未检测到显卡".to_string(),
        };
        let mem_value = format!("{:.0}%", snap.mem);
        let mem_sub = format!("{:.1}/{:.1} GB", snap.mem_used, snap.mem_total);
        let net_value = format!("↓{} ↑{}", fmt_speed(snap.rx), fmt_speed(snap.tx));

        draw_card(c, 13.0, 29.0, 190.0, 44.0, "CPU", &cpu_value, "总占用", theme::BLUE, &series.cpu, 100.0);
        draw_card(c, 216.0, 29.0, 190.0, 44.0, "GPU 核心", &gpu_value, &gpu_sub, theme::SOFT_GREEN, &series.gpu, 100.0);
        draw_card(c, 13.0, 79.0, 190.0, 44.0, "内存", &mem_value, &mem_sub, theme::YELLOW, &series.mem, 100.0);
        draw_card(c, 216.0, 79.0, 190.0, 44.0, "网络", &net_value, "实时下行 / 上行", theme::CYAN, &series.net, 12.0);
    }
}

fn fmt_speed(bytes: f32) -> String {
    if bytes >= 1024.0 * 1024.0 {
        format!("{:.1} MB/s", bytes / 1024.0 / 1024.0)
    } else if bytes >= 1024.0 {
        format!("{:.0} KB/s", bytes / 1024.0)
    } else {
        format!("{:.0} B/s", bytes)
    }
}

#[allow(clippy::too_many_arguments)]
fn draw_card(
    c: &Card,
    x: f32,
    y: f32,
    w: f32,
    h: f32,
    label: &str,
    value: &str,
    sub: &str,
    accent: Color32,
    history: &[f32],
    max: f32,
) {
    c.fill(x, y, w, h, 10.0, Color32::from_rgb(30, 30, 33));
    c.stroke(x, y, w, h, 10.0, 1.0, paint::alpha(theme::BTN_HOVER, 0.9));
    c.fill(x, y + 7.0, 2.5, h - 14.0, 1.25, accent);

    c.text(x + 12.0, y + 14.0, label, theme::size::TINY, W::Semibold, accent, H::Left);
    c.text(x + w - 10.0, y + 14.0, sub, theme::size::TINY, W::Regular, theme::DIM, H::Right);
    let value_size = if value.len() > 9 { 12.5 } else { 17.0 };
    c.text(x + 12.0, y + 34.0, value, value_size, W::Bold, theme::WHITE, H::Left);

    spark(c, x + w - 82.0, y + 8.0, 72.0, 17.0, history, accent, max);
}

fn spark(c: &Card, x: f32, y: f32, w: f32, h: f32, values: &[f32], color: Color32, max: f32) {
    c.fill(x, y, w, h, 4.0, Color32::from_rgb(22, 22, 24));
    if values.len() < 2 {
        return;
    }
    let points: Vec<(f32, f32)> = values
        .iter()
        .enumerate()
        .map(|(i, v)| {
            let px = x + 2.0 + i as f32 / (values.len() - 1) as f32 * (w - 4.0);
            let py = y + h - 2.0 - (*v / max).clamp(0.0, 1.0) * (h - 4.0);
            (px, py)
        })
        .collect();
    let mut area = points.clone();
    area.push((x + w - 2.0, y + h - 2.0));
    area.push((x + 2.0, y + h - 2.0));
    c.poly(&area, paint::alpha(color, 0.18));
    c.path(&points, 1.4, color);
    if let Some(&(px, py)) = points.last() {
        c.circle(px, py, 1.8, theme::WHITE);
    }
}

#[cfg(all(test, windows))]
mod gpu_counter_tests {
    use super::query_gpu_counter;

    #[test]
    fn windows_gpu_engine_counter_returns_reading() {
        let reading = query_gpu_counter();
        assert!(reading.is_some(), "GPU Engine 计数器应可用（含 AMD/Intel/NVIDIA）");
        let value = reading.unwrap();
        assert!((0.0..=100.0).contains(&value), "占用率应在 0..100，实际 {value}");
        eprintln!("PDH GPU Engine reading = {value}%");
    }
}
