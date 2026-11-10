//! System stats tile: CPU, RAM and GPU load on the home page.
//!
//! Sampled once a second and only while the notch is open: the worker
//! blocks until the notch expands, samples until it collapses, then blocks
//! again. CPU comes from GetSystemTimes deltas, RAM from GlobalMemoryStatusEx
//! and GPU from the "GPU Engine" performance counters Task Manager reads
//! (the busiest engine wins, like Task Manager's GPU column).

use std::any::Any;
use std::collections::HashMap;
use std::sync::mpsc::{channel, Receiver, RecvTimeoutError, Sender};
use std::time::Duration;

use windows::core::w;
use windows::Win32::Foundation::FILETIME;
use windows::Win32::System::Performance::{
    PdhAddEnglishCounterW, PdhCloseQuery, PdhCollectQueryData, PdhGetFormattedCounterArrayW, PdhOpenQueryW,
    PDH_FMT_COUNTERVALUE_ITEM_W, PDH_FMT_DOUBLE, PDH_HCOUNTER, PDH_HQUERY, PDH_MORE_DATA,
};
use windows::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
use windows::Win32::System::Threading::GetSystemTimes;

use super::{CardSize, Cx, Module, ModuleId, SystemEvent};
use crate::bus::Bus;
use crate::gfx::{Icon, TextStyle};
use crate::ui::{gkey, Ui};
use crate::util::{palette, Color, Rect};

const ID: ModuleId = "stats";
const INTERVAL: Duration = Duration::from_secs(1);
/// First reading after opening: rates need two samples, keep the gap short.
/// Readings kept for the tick graphs on the stats page.
const HISTORY: usize = 20;
const FIRST: Duration = Duration::from_millis(250);

/// Latest CPU and memory load in percent (-1 until the first reading), for the notch header.
static LOAD_CPU: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(-1);
static LOAD_RAM: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(-1);

pub fn cpu_percent() -> Option<i32> {
    let v = LOAD_CPU.load(std::sync::atomic::Ordering::Relaxed);
    (v >= 0).then_some(v)
}

pub fn ram_percent() -> Option<i32> {
    let v = LOAD_RAM.load(std::sync::atomic::Ordering::Relaxed);
    (v >= 0).then_some(v)
}

#[derive(Clone, Copy, Debug, Default)]
struct Sample {
    cpu: f32,
    ram: f32,
    gpu: Option<f32>,
}

enum Cmd {
    Run(bool),
    Quit,
}

pub struct Stats {
    tx: Option<Sender<Cmd>>,
    sample: Option<Sample>,
    /// Recent readings per gauge (CPU, RAM, GPU) for the tick graphs.
    history: [Vec<f32>; 3],
}

impl Stats {
    pub fn new() -> Self {
        Self { tx: None, sample: None, history: Default::default() }
    }
}

impl Module for Stats {
    fn id(&self) -> ModuleId {
        ID
    }
    fn title(&self) -> &str {
        "System Stats"
    }
    fn icon(&self) -> Icon {
        Icon::Pulse
    }

    fn start(&mut self, cx: &mut Cx) {
        let (tx, rx) = channel();
        let bus = cx.bus.clone();
        let _ = std::thread::Builder::new().name("stats".into()).spawn(move || worker(rx, bus));
        self.tx = Some(tx);
    }

    fn stop(&mut self) {
        if let Some(tx) = self.tx.take() {
            let _ = tx.send(Cmd::Quit);
        }
    }

    fn on_system(&mut self, ev: &SystemEvent, _cx: &mut Cx) {
        if let (SystemEvent::Expanded(open), Some(tx)) = (ev, &self.tx) {
            let _ = tx.send(Cmd::Run(*open));
        }
    }

    fn on_message(&mut self, msg: Box<dyn Any + Send>, cx: &mut Cx) {
        if let Ok(s) = msg.downcast::<Sample>() {
            let vals = [s.cpu, s.ram, s.gpu.unwrap_or(0.0)];
            for (h, v) in self.history.iter_mut().zip(vals) {
                if h.len() >= HISTORY {
                    h.remove(0);
                }
                h.push(v);
            }
            self.sample = Some(*s);
            LOAD_CPU.store((s.cpu * 100.0).round() as i32, std::sync::atomic::Ordering::Relaxed);
            LOAD_RAM.store((s.ram * 100.0).round() as i32, std::sync::atomic::Ordering::Relaxed);
            cx.fx.redraw = true;
        }
    }

    fn card(&self) -> Option<CardSize> {
        Some(CardSize::Small)
    }

    /// The numbers live on the home page; there is no Stats tab.
    fn has_page(&self) -> bool {
        false
    }

    fn page_height(&self, _w: f32) -> f32 {
        if crate::ui::look_alcove() {
            150.0
        } else {
            158.0
        }
    }

    /// Three big tick rings with a short history graph of ticks under each.
    fn draw_page(&mut self, ui: &mut Ui, r: Rect) {
        if crate::ui::look_alcove() {
            return self.draw_alcove(ui, r);
        }
        let s = self.sample;
        let gauges: [(&str, Option<f32>); 3] =
            [("CPU", s.map(|s| s.cpu)), ("RAM", s.map(|s| s.ram)), ("GPU", s.and_then(|s| s.gpu))];
        let slot = r.w / 3.0;
        let d = 68.0;
        let top = r.y + (r.h - (d + 64.0)).max(0.0) * 0.5;
        for (i, (name, frac)) in gauges.iter().enumerate() {
            let cx = r.x + slot * (i as f32 + 0.5);
            let cy = top + d * 0.5;
            let f = ui.glide(gkey("stats-page", i), frac.unwrap_or(0.0));
            let shown = frac.map(|_| f);
            ui.ring(cx, cy, d, 6.0, frac.unwrap_or(0.0), load_color(f));
            ui.p.text(
                &pct(shown),
                Rect::new(cx - d * 0.5, cy - 11.0, d, 22.0),
                &TextStyle::new(15.0, palette::TEXT).semibold().display().center(),
            );
            ui.p.text(
                name,
                Rect::new(cx - slot * 0.5, top + d + 6.0, slot, 15.0),
                &TextStyle::new(12.0, palette::TEXT).semibold().center(),
            );
            let g = Rect::new(cx - slot * 0.36, top + d + 26.0, slot * 0.72, 18.0);
            if self.history[i].is_empty() {
                ui.tick_rule(Rect::new(g.x, g.bottom() - 2.0, g.w, 2.0), 0.10);
            } else {
                let c = load_color(f);
                // pad with empty ticks on the left so the graph keeps its width
                let mut v = vec![0.0; HISTORY - self.history[i].len()];
                v.extend_from_slice(&self.history[i]);
                ui.tick_graph(g, &v, c);
            }
        }
    }

    fn draw_card(&mut self, ui: &mut Ui, r: Rect) {
        let _ = ui.card(r, false);
        let s = self.sample;
        let cols: [(&str, Option<f32>, String); 3] = [
            ("CPU", s.map(|s| s.cpu), pct(s.map(|s| s.cpu))),
            ("RAM", s.map(|s| s.ram), pct(s.map(|s| s.ram))),
            ("GPU", s.and_then(|s| s.gpu), pct(s.and_then(|s| s.gpu))),
        ];
        let inner = r.inset_xy(12.0, 0.0);
        let cw = inner.w / 3.0;
        // ring on the left of each slot, name and value stacked beside it
        let d = (r.h - 12.0).clamp(22.0, 40.0);
        let stroke = (d * 0.13).clamp(2.2, 4.0);
        let label = TextStyle::new(10.5, palette::TEXT_FAINT).semibold();
        let value = TextStyle::new(12.5, palette::TEXT).semibold().display();
        for (i, (name, frac, text)) in cols.iter().enumerate() {
            let x = inner.x + i as f32 * cw;
            let (cx, cy) = (x + d * 0.5, r.cy());
            let f = ui.glide(gkey("stats-card", i), frac.unwrap_or(0.0));
            let text = if frac.is_some() { pct(Some(f)) } else { text.clone() };
            ui.ring(cx, cy, d, stroke, frac.unwrap_or(0.0), load_color(f));
            let tx = x + d + 7.0;
            let tw = (cw - d - 9.0).max(0.0);
            ui.p.text(name, Rect::new(tx, cy - 14.0, tw, 14.0), &label);
            ui.p.text(&text, Rect::new(tx, cy, tw, 15.0), &value);
        }
    }
}

impl Stats {
    /// Alcove: three soft tiles, each with a big number and a little bar history.
    fn draw_alcove(&mut self, ui: &mut Ui, r: Rect) {
        let s = self.sample;
        let gauges: [(&str, Icon, Option<f32>); 3] = [
            ("Processor", Icon::Cpu, s.map(|s| s.cpu)),
            ("Memory", Icon::Ram, s.map(|s| s.ram)),
            ("Graphics", Icon::Pulse, s.and_then(|s| s.gpu)),
        ];
        let gap = 12.0;
        let tw = (r.w - 2.0 * gap) / 3.0;
        for (i, (name, icon, frac)) in gauges.iter().enumerate() {
            let t = Rect::new(r.x + i as f32 * (tw + gap), r.y, tw, r.h);
            let f = ui.glide(gkey("stats-al", i), frac.unwrap_or(0.0));
            let col = if f >= 0.85 { palette::RED } else { Color::white(0.95) };
            ui.p.icon(*icon, Rect::new(t.x + 16.0, t.y + 15.0, 17.0, 17.0), Color::white(0.55));
            ui.p.text(
                name,
                Rect::new(t.x + 40.0, t.y + 12.0, tw - 50.0, 22.0),
                &TextStyle::new(13.0, palette::TEXT_DIM).semibold(),
            );
            let big = TextStyle::new(40.0, col).weight(650);
            let num = if frac.is_some() { format!("{:.0}", f * 100.0) } else { "–".to_string() };
            let (nw, _) = ui.p.measure(&num, &big);
            ui.p.text(&num, Rect::new(t.x + 16.0, t.y + 36.0, nw + 6.0, 52.0), &big);
            if frac.is_some() {
                ui.p.text(
                    "%",
                    Rect::new(t.x + 18.0 + nw, t.y + 48.0, 30.0, 28.0),
                    &TextStyle::new(17.0, palette::TEXT_DIM).semibold(),
                );
            }
            // the last readings as little rounded bars
            let area = Rect::new(t.x + 16.0, t.bottom() - 40.0, tw - 32.0, 26.0);
            let n = HISTORY;
            let bg = 3.0;
            let bw = (area.w - bg * (n as f32 - 1.0)) / n as f32;
            let hist = &self.history[i];
            for k in 0..n {
                let idx = k as isize - (n - hist.len()) as isize;
                let v = if idx >= 0 { hist[idx as usize] } else { 0.0 };
                let bh = (v.clamp(0.0, 1.0) * area.h).max(3.0);
                let x = area.x + k as f32 * (bw + bg);
                let a = if idx >= 0 { 0.35 + 0.6 * (k as f32 / n as f32) } else { 0.12 };
                ui.p.fill_rounded(Rect::new(x, area.bottom() - bh, bw, bh), bw * 0.5, col.with_a(a));
            }
        }
    }
}

fn pct(v: Option<f32>) -> String {
    v.map_or("–".into(), |v| format!("{:.0}%", (v * 100.0).clamp(0.0, 100.0)))
}

fn load_color(v: f32) -> Color {
    if v >= 0.85 {
        palette::RED
    } else {
        palette::BLUE
    }
}

// ---------------------------------------------------------------------------
// Worker
// ---------------------------------------------------------------------------

fn worker(rx: Receiver<Cmd>, bus: Bus) {
    let mut cpu = CpuMeter::default();
    // opened on first use: Windows loads its GPU counter provider then
    // (~0.2 s of CPU once per process), later samples take ~1 ms
    let mut gpu: Option<Option<GpuMeter>> = None;
    let mut wait: Option<Duration> = None;
    // last whole percentages sent: unchanged readings cost no redraw
    let mut shown: Option<[i32; 3]> = None;
    loop {
        let cmd = match wait {
            Some(d) => match rx.recv_timeout(d) {
                Ok(c) => Some(c),
                Err(RecvTimeoutError::Timeout) => None,
                Err(RecvTimeoutError::Disconnected) => return,
            },
            None => match rx.recv() {
                Ok(c) => Some(c),
                Err(_) => return,
            },
        };
        match cmd {
            Some(Cmd::Quit) => return,
            Some(Cmd::Run(false)) => wait = None,
            Some(Cmd::Run(true)) => {
                if wait.is_none() {
                    // prime the rate counters; the first real sample follows shortly
                    cpu.sample();
                    if let Some(g) = gpu.get_or_insert_with(GpuMeter::open) {
                        g.sample();
                    }
                    wait = Some(FIRST);
                }
            }
            None => {
                let mut s = Sample {
                    cpu: cpu.sample(),
                    gpu: gpu.as_mut().and_then(|g| g.as_mut()?.sample()),
                    ..Default::default()
                };
                let mut m =
                    MEMORYSTATUSEX { dwLength: std::mem::size_of::<MEMORYSTATUSEX>() as u32, ..Default::default() };
                if unsafe { GlobalMemoryStatusEx(&mut m) }.is_ok() && m.ullTotalPhys > 0 {
                    s.ram = (m.ullTotalPhys - m.ullAvailPhys) as f32 / m.ullTotalPhys as f32;
                }
                let pct = |v: f32| (v * 100.0).round() as i32;
                let now = [pct(s.cpu), pct(s.ram), s.gpu.map_or(-1, pct)];
                if shown != Some(now) {
                    shown = Some(now);
                    bus.to_module(ID, s);
                }
                wait = Some(INTERVAL);
            }
        }
    }
}

#[derive(Default)]
struct CpuMeter {
    last: Option<(u64, u64)>,
}

impl CpuMeter {
    /// Busy fraction since the previous call.
    fn sample(&mut self) -> f32 {
        let (mut idle, mut kernel, mut user) = (FILETIME::default(), FILETIME::default(), FILETIME::default());
        if unsafe { GetSystemTimes(Some(&mut idle), Some(&mut kernel), Some(&mut user)) }.is_err() {
            return 0.0;
        }
        let ft = |f: FILETIME| ((f.dwHighDateTime as u64) << 32) | f.dwLowDateTime as u64;
        // kernel time includes idle time
        let (idle, total) = (ft(idle), ft(kernel) + ft(user));
        let out = match self.last {
            Some((li, lt)) if total > lt => 1.0 - (idle.saturating_sub(li)) as f32 / (total - lt) as f32,
            _ => 0.0,
        };
        self.last = Some((idle, total));
        out.clamp(0.0, 1.0)
    }
}

struct GpuMeter {
    query: PDH_HQUERY,
    counter: PDH_HCOUNTER,
    buf: Vec<u64>,
}

impl GpuMeter {
    fn open() -> Option<Self> {
        unsafe {
            let mut query = PDH_HQUERY::default();
            if PdhOpenQueryW(None, 0, &mut query) != 0 {
                return None;
            }
            let mut counter = PDH_HCOUNTER::default();
            if PdhAddEnglishCounterW(query, w!("\\GPU Engine(*)\\Utilization Percentage"), 0, &mut counter) != 0 {
                let _ = PdhCloseQuery(query);
                crate::log!("stats: GPU counters unavailable");
                return None;
            }
            Some(Self { query, counter, buf: Vec::new() })
        }
    }

    /// Load of the busiest GPU engine, 0..1.
    fn sample(&mut self) -> Option<f32> {
        unsafe {
            if PdhCollectQueryData(self.query) != 0 {
                return None;
            }
            let (mut size, mut count) = (0u32, 0u32);
            if PdhGetFormattedCounterArrayW(self.counter, PDH_FMT_DOUBLE, &mut size, &mut count, None) != PDH_MORE_DATA
            {
                return None;
            }
            // u64 storage keeps the item array 8-byte aligned
            self.buf.resize((size as usize).div_ceil(8), 0);
            let items = self.buf.as_mut_ptr() as *mut PDH_FMT_COUNTERVALUE_ITEM_W;
            if PdhGetFormattedCounterArrayW(self.counter, PDH_FMT_DOUBLE, &mut size, &mut count, Some(items)) != 0 {
                return None;
            }
            // one instance per process and engine: sum per engine, take the busiest
            let mut engines: HashMap<String, f64> = HashMap::new();
            for it in std::slice::from_raw_parts(items, count as usize) {
                // PDH_CSTATUS_VALID_DATA / PDH_CSTATUS_NEW_DATA
                if it.FmtValue.CStatus > 1 {
                    continue;
                }
                let name = it.szName.to_string().unwrap_or_default();
                *engines.entry(engine_key(&name).to_string()).or_default() += it.FmtValue.Anonymous.doubleValue;
            }
            let busiest = engines.values().fold(0.0f64, |a, &b| a.max(b));
            Some((busiest / 100.0).clamp(0.0, 1.0) as f32)
        }
    }
}

impl Drop for GpuMeter {
    fn drop(&mut self) {
        unsafe {
            let _ = PdhCloseQuery(self.query);
        }
    }
}

/// "pid_1234_luid_0x0_0xD1F6_phys_0_eng_3_engtype_3D" → "luid_0x0_0xD1F6_phys_0_eng_3_engtype_3D".
fn engine_key(instance: &str) -> &str {
    instance.find("luid_").map_or(instance, |i| &instance[i..])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn engine_keys() {
        assert_eq!(engine_key("pid_42_luid_0x0_0x1_phys_0_eng_0_engtype_3D"), "luid_0x0_0x1_phys_0_eng_0_engtype_3D");
        assert_eq!(engine_key("other"), "other");
        assert_eq!(pct(Some(0.123)), "12%");
        assert_eq!(pct(None), "–");
    }
}
