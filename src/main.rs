// mado-memory — pixel plugin for Mado sidebar
// Displays CPU usage (with sparkline history), RAM usage (with trend delta),
// and Mado process uptime.

use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Write};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::Deserialize;
use sysinfo::{Pid, System};

// ── Palette (Slate) ───────────────────────────────────────────────────────────

const BG:     [u8; 4] = [15,  23,  42,  255]; // slate-900
const FG:     [u8; 4] = [248, 250, 252, 255]; // slate-50
const LABEL:  [u8; 4] = [148, 163, 184, 255]; // slate-400
const BAR_BG: [u8; 4] = [30,  41,  59,  255]; // slate-800
const GREEN:  [u8; 4] = [52,  211, 153, 255]; // emerald-400
const AMBER:  [u8; 4] = [251, 191, 36,  255]; // amber-400
const RED:    [u8; 4] = [239, 68,  68,  255]; // red-500
const SPARK:  [u8; 4] = [99,  102, 241, 255]; // indigo-500

fn bar_color(ratio: f32) -> [u8; 4] {
    if ratio > 0.80 { RED }
    else if ratio > 0.50 { AMBER }
    else { GREEN }
}

fn delta_color(delta: i64) -> [u8; 4] {
    if delta > 5 { RED }
    else if delta > 0 { AMBER }
    else if delta < 0 { GREEN }
    else { LABEL }
}

// ── Canvas ────────────────────────────────────────────────────────────────────

struct Canvas { pixels: Vec<u8>, w: usize, h: usize }

impl Canvas {
    fn new(w: usize, h: usize) -> Self {
        let mut pixels = vec![0u8; w * h * 4];
        for px in pixels.chunks_exact_mut(4) { px.copy_from_slice(&BG); }
        Canvas { pixels, w, h }
    }

    fn blend(&mut self, x: usize, y: usize, color: [u8; 4], alpha: f32) {
        if x >= self.w || y >= self.h { return; }
        let i = (y * self.w + x) * 4;
        let ia = 1.0 - alpha;
        for c in 0..3 {
            self.pixels[i + c] =
                (self.pixels[i + c] as f32 * ia + color[c] as f32 * alpha).round() as u8;
        }
        self.pixels[i + 3] = 255;
    }

    fn fill_rect(&mut self, x: usize, y: usize, rw: usize, rh: usize, color: [u8; 4]) {
        for row in y..(y + rh).min(self.h) {
            for col in x..(x + rw).min(self.w) {
                let i = (row * self.w + col) * 4;
                self.pixels[i..i + 4].copy_from_slice(&color);
            }
        }
    }

    fn text(&mut self, font: &fontdue::Font, text: &str,
            size: f32, x: usize, y: usize, color: [u8; 4]) -> usize {
        let mut cx = x;
        for ch in text.chars() {
            let (m, bmp) = font.rasterize(ch, size);
            let gx = cx as isize + m.xmin as isize;
            let gy = y as isize - m.height as isize - m.ymin as isize;
            for (k, &cov) in bmp.iter().enumerate() {
                if cov == 0 { continue; }
                let px = gx + (k % m.width) as isize;
                let py = gy + (k / m.width) as isize;
                if px >= 0 && py >= 0 {
                    self.blend(px as usize, py as usize, color, cov as f32 / 255.0);
                }
            }
            cx += m.advance_width.round() as usize;
        }
        cx
    }

    fn measure(font: &fontdue::Font, text: &str, size: f32) -> usize {
        text.chars().map(|ch| {
            let (m, _) = font.rasterize(ch, size);
            m.advance_width.round() as usize
        }).sum()
    }

    // Column-bar sparkline: each pixel column maps to one history sample.
    fn sparkline(&mut self, history: &VecDeque<f32>, max_val: f32,
                 x: usize, y: usize, sw: usize, sh: usize) {
        self.fill_rect(x, y, sw, sh, BAR_BG);
        let n = history.len();
        if n == 0 || max_val <= 0.0 || sw == 0 { return; }
        for col in 0..sw {
            let idx = (col * n / sw).min(n - 1);
            let ratio = (history[idx] / max_val).clamp(0.0, 1.0);
            let bar_height = (ratio * sh as f32).round() as usize;
            if bar_height > 0 {
                self.fill_rect(x + col, y + sh - bar_height, 1, bar_height, SPARK);
            }
        }
    }

    fn write_frame(&self, out: &mut impl Write) {
        out.write_all(b"MADO").unwrap();
        out.write_all(&(self.w as u32).to_le_bytes()).unwrap();
        out.write_all(&(self.h as u32).to_le_bytes()).unwrap();
        out.write_all(&self.pixels).unwrap();
        out.flush().unwrap();
    }
}

// ── Font loading ──────────────────────────────────────────────────────────────

fn load_font() -> Option<fontdue::Font> {
    for path in &[
        "/System/Library/Fonts/Supplemental/Arial.ttf",
        "/Library/Fonts/Arial.ttf",
        "/System/Library/Fonts/Helvetica.ttc",
        "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf",
        "/usr/share/fonts/TTF/DejaVuSans.ttf",
        "/usr/share/fonts/dejavu-sans-fonts/DejaVuSans.ttf",
        "C:\\Windows\\Fonts\\arial.ttf",
        "C:\\Windows\\Fonts\\segoeui.ttf",
    ] {
        if let Ok(data) = std::fs::read(path) {
            if let Ok(font) = fontdue::Font::from_bytes(
                data.as_slice(), fontdue::FontSettings::default())
            {
                return Some(font);
            }
        }
    }
    None
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn format_uptime(secs: u64) -> String {
    let h = secs / 3600;
    let m = (secs % 3600) / 60;
    let s = secs % 60;
    if h > 0      { format!("{}h {}m", h, m) }
    else if m > 0 { format!("{}m {}s", m, s) }
    else          { format!("{}s", s) }
}

// ── Event ─────────────────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct Event {
    #[serde(rename = "type")]
    kind:   String,
    width:  Option<u32>,
    height: Option<u32>,
}

// ── Stats ─────────────────────────────────────────────────────────────────────

const HISTORY_LEN: usize = 60; // one sample per second

#[derive(Clone, Default)]
struct Stats {
    cpu_pct:     f32,
    ram_mb:      u64,
    ram_total:   u64,
    cpu_history: VecDeque<f32>,
    ram_history: VecDeque<u64>,
    uptime_secs: u64,
}

// ── Main ──────────────────────────────────────────────────────────────────────

fn main() {
    let font = load_font().unwrap_or_else(|| {
        eprintln!("mado-memory: no system font found");
        std::process::exit(1);
    });

    // We are a child of Mado, so our parent is Mado's PID.
    let mado_pid: Option<Pid> = {
        let mut sys = System::new();
        sys.refresh_processes(sysinfo::ProcessesToUpdate::All, true);
        sysinfo::get_current_pid().ok()
            .and_then(|me| sys.process(me))
            .and_then(|p| p.parent())
    };

    let dims:  Arc<Mutex<(u32, u32)>> = Arc::new(Mutex::new((300, 200)));
    let stats: Arc<Mutex<Stats>>      = Arc::new(Mutex::new(Stats::default()));

    // Stats refresh thread — samples every second.
    {
        let stats = Arc::clone(&stats);
        std::thread::spawn(move || {
            let mut sys = System::new();
            loop {
                sys.refresh_processes(sysinfo::ProcessesToUpdate::All, true);
                sys.refresh_memory();

                if let Some(pid) = mado_pid {
                    if let Some(proc) = sys.process(pid) {
                        let now = SystemTime::now()
                            .duration_since(UNIX_EPOCH)
                            .map(|d| d.as_secs())
                            .unwrap_or(0);

                        let mut s = stats.lock().unwrap();

                        s.cpu_pct   = proc.cpu_usage();
                        s.ram_mb    = proc.memory() / 1_048_576;
                        s.ram_total = sys.total_memory() / 1_048_576;
                        s.uptime_secs = now.saturating_sub(proc.start_time());

                        let cpu_sample = s.cpu_pct;
                        s.cpu_history.push_back(cpu_sample);
                        if s.cpu_history.len() > HISTORY_LEN { s.cpu_history.pop_front(); }

                        let ram_sample = s.ram_mb;
                        s.ram_history.push_back(ram_sample);
                        if s.ram_history.len() > HISTORY_LEN { s.ram_history.pop_front(); }
                    }
                }

                std::thread::sleep(Duration::from_secs(1));
            }
        });
    }

    // Stdin listener — handles resize events from Mado.
    {
        let dims = Arc::clone(&dims);
        std::thread::spawn(move || {
            let stdin = std::io::stdin();
            for line in BufReader::new(stdin.lock()).lines().flatten() {
                if let Ok(ev) = serde_json::from_str::<Event>(&line) {
                    if ev.kind == "resize" {
                        if let (Some(w), Some(h)) = (ev.width, ev.height) {
                            if w > 0 && h > 0 {
                                *dims.lock().unwrap() = (w, h);
                            }
                        }
                    }
                }
            }
        });
    }

    let stdout = std::io::stdout();
    let mut out = std::io::BufWriter::new(stdout.lock());

    loop {
        let (w, h) = *dims.lock().unwrap();
        let (w, h) = (w as usize, h as usize);
        let s = stats.lock().unwrap().clone();

        let mut canvas = Canvas::new(w, h);

        let pad        = (w as f32 * 0.08).round() as usize;
        let inner      = w.saturating_sub(pad * 2);
        let label_size = (w as f32 * 0.075).clamp(10.0, 13.0);
        let value_size = (w as f32 * 0.095).clamp(12.0, 16.0);
        let bar_h      = ((w as f32 * 0.035).clamp(4.0, 7.0)) as usize;
        let spark_h    = ((h as f32 * 0.42).clamp(36.0, 84.0)) as usize;

        // ── Uptime ───────────────────────────────────────────────────────────
        let uptime_y = (h as f32 * 0.12).round() as usize;
        canvas.text(&font, "\u{2191}", label_size, pad, uptime_y, LABEL); // ↑
        let up_str = format_uptime(s.uptime_secs);
        let up_vw  = Canvas::measure(&font, &up_str, label_size);
        canvas.text(&font, &up_str, label_size,
                    w.saturating_sub(pad + up_vw), uptime_y, FG);

        // ── CPU ──────────────────────────────────────────────────────────────
        let cpu_y = (h as f32 * 0.34).round() as usize;
        canvas.text(&font, "CPU", label_size, pad, cpu_y, LABEL);

        let cpu_str = format!("{:.1}%", s.cpu_pct);
        let cpu_vw  = Canvas::measure(&font, &cpu_str, value_size);
        canvas.text(&font, &cpu_str, value_size,
                    w.saturating_sub(pad + cpu_vw), cpu_y, FG);

        // CPU sparkline — scale to the observed max or 100, whichever is larger.
        let spark_top = cpu_y + 4;
        let spark_max = s.cpu_history.iter().cloned().fold(100.0_f32, f32::max);
        canvas.sparkline(&s.cpu_history, spark_max, pad, spark_top, inner, spark_h);

        // ── RAM ──────────────────────────────────────────────────────────────
        let ram_y = (h as f32 * 0.82).round() as usize;
        canvas.text(&font, "RAM", label_size, pad, ram_y, LABEL);

        // 30-second memory delta.
        let ram_delta: i64 = if s.ram_history.len() >= 2 {
            let window = s.ram_history.len().min(30);
            let old = s.ram_history[s.ram_history.len() - window];
            s.ram_mb as i64 - old as i64
        } else {
            0
        };

        // Current RAM value, right-aligned.
        let ram_str = format!("{} MB", s.ram_mb);
        let ram_vw  = Canvas::measure(&font, &ram_str, value_size);
        canvas.text(&font, &ram_str, value_size,
                    w.saturating_sub(pad + ram_vw), ram_y, FG);

        // Delta placed just left of the RAM value.
        if ram_delta != 0 {
            let sign = if ram_delta > 0 { "+" } else { "" };
            let delta_str = format!("{}{} MB", sign, ram_delta);
            let delta_vw  = Canvas::measure(&font, &delta_str, label_size);
            let delta_x   = w.saturating_sub(pad + ram_vw + delta_vw + 6);
            canvas.text(&font, &delta_str, label_size,
                        delta_x, ram_y, delta_color(ram_delta));
        }

        // RAM bar.
        let ram_bar_y = ram_y + (label_size * 0.5) as usize;
        canvas.fill_rect(pad, ram_bar_y, inner, bar_h, BAR_BG);
        let ram_ratio = if s.ram_total > 0 {
            (s.ram_mb as f32 / s.ram_total as f32).clamp(0.0, 1.0)
        } else {
            0.0
        };
        let ram_fill = (ram_ratio * inner as f32) as usize;
        if ram_fill > 0 {
            canvas.fill_rect(pad, ram_bar_y, ram_fill, bar_h, bar_color(ram_ratio));
        }

        canvas.write_frame(&mut out);
        std::thread::sleep(Duration::from_secs(1));
    }
}
