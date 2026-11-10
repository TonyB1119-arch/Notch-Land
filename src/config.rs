//! User configuration: `%APPDATA%\DynamicNotch\config.toml`.
//!
//! Every field has a default, so partial files are fine.  The file is watched
//! and hot-reloaded on save.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::util::app_dir;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
#[derive(Default)]
pub struct Config {
    pub general: General,
    pub appearance: Appearance,
    pub hotkeys: Hotkeys,
    pub modules: Modules,
    pub media: MediaCfg,
    pub timer: TimerCfg,
    pub clipboard: ClipboardCfg,
    pub downloads: DownloadsCfg,
    pub battery: BatteryCfg,
    pub ai: AiCfg,
    pub locks: LocksCfg,
    pub shelf: ShelfCfg,
    pub ports: PortsCfg,
    pub voice: VoiceCfg,
    pub osd: OsdCfg,
    pub bluetooth: BluetoothCfg,
    pub home: HomeCfg,
    pub weather: WeatherCfg,
    pub gemini: GeminiCfg,
    pub greeting: GreetingCfg,
    pub update: UpdateCfg,
    pub notch: NotchCfg,
    pub tabs: TabsCfg,
    pub glass: GlassCfg,
    pub dock: DockCfg,
    pub launcher: LauncherCfg,
    pub combos: CombosCfg,
    pub radial: RadialCfg,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct General {
    /// "primary", "active" (follows the focused window) or a 1-based monitor number.
    pub monitor: String,
    pub expand_on_hover: bool,
    /// Short, quiet tick on hover and click.
    pub haptics: bool,
    /// Alcove: music and finished peeks leave small bubbles beside the notch.
    pub bubbles: bool,
    /// A very quiet blip as a bubble pops out and again as it goes.
    pub bubble_sounds: bool,
    /// Alcove: volume and brightness show as one big round dial (false: the line). Battery and
    /// Bluetooth are always dials.
    pub hud_ring: bool,
    /// Round dials (volume, brightness, battery, Bluetooth) fill clockwise (false: counter-clockwise).
    pub hud_clockwise: bool,
    pub hover_delay_ms: u32,
    pub collapse_delay_ms: u32,
    pub hide_in_fullscreen: bool,
    /// "notch" (always visible), "pill" (thin line when idle) or "hidden" (only on activity/hover).
    pub idle_style: String,
    pub start_with_windows: bool,
}

impl Default for General {
    fn default() -> Self {
        Self {
            monitor: "primary".into(),
            expand_on_hover: false,
            haptics: true,
            bubbles: true,
            bubble_sounds: false,
            hud_ring: true,
            hud_clockwise: false,
            hover_delay_ms: 140,
            collapse_delay_ms: 60,
            hide_in_fullscreen: true,
            idle_style: "notch".into(),
            start_with_windows: false,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Appearance {
    pub notch_width: f32,
    pub notch_height: f32,
    pub expanded_width: f32,
    /// Background opacity when collapsed (1.0 = solid black like a real notch).
    pub collapsed_opacity: f32,
    /// Background opacity when expanded (lower = more glass).
    pub expanded_opacity: f32,
    /// Blur what is behind the expanded notch.
    pub blur: bool,
    pub shadow: bool,
    /// 0 = no overshoot, 1 = very bouncy.
    pub bounce: f32,
    /// Animation speed multiplier (2.0 = twice as fast).
    pub animation_speed: f32,
    /// Page switch length (Alcove): 1.0 = normal, 2.0 = twice as long.
    pub page_time: f32,
    /// Open notch closing length (Alcove): 1.0 = normal, 2.0 = twice as long.
    pub close_time: f32,
    /// Notch opening length (Alcove): 1.0 = normal, 2.0 = twice as long.
    pub open_time: f32,
    /// "auto" (from album art) or "#RRGGBB".
    pub accent: String,
    /// Overall look: "beams" (glowing beams everywhere), "calm" (beams only on volume, brightness
    /// and battery; glow bars, pills and dot rings elsewhere), "minimal" (thin plain lines and
    /// quiet, bounce-free motion) or "nothing" (dot-matrix glyph lights, red accent).
    pub look: String,
    /// false = blue beams with a hint of white (default), true = white beams with a hint of blue.
    pub white_beams: bool,
    /// Home clock: "roll" (digits slide up and are replaced from below) or "flip" (flip-clock tiles).
    pub clock_style: String,
    /// Draw the content once and only move/fade the picture while the notch opens, closes or
    /// switches pages (much less work per frame).
    pub motion_cache: bool,
    /// Alcove: the bottom edge of the open notch softens into a light blur.
    pub bottom_fade: bool,
    /// Alcove: the open notch's edge bends what is behind it, like thick glass.
    pub glass_look: bool,
    /// Alcove: while media is playing and the notch is open, the glass edge keeps up with what is
    /// behind the notch (the screen is looked at again a few times a second). Turn off to save CPU.
    pub glass_live: bool,
    /// Lighter animations: the glass edge is rebuilt less often (and coarser) while the notch moves,
    /// and the slow last stretch of a move uses fewer frames. Looks the same once the notch rests.
    /// Turn off for the full-rate animation.
    pub lite_motion: bool,
    /// Icons are drawn in white or grey whatever colour a page gives them (no coloured tints).
    pub mono_icons: bool,
    /// Page switches: false = the page slides and fades (default), true = the old page blurs out
    /// and the new one blurs in, in place.
    pub page_blur: bool,
    /// Most frames a second the notch draws while it moves (30 - 240). 60 is smooth and light; raise it on a fast display for
    /// even smoother motion at a higher CPU cost.
    pub max_fps: f32,
    /// Extra UI scale on top of the monitor DPI.
    pub scale: f32,
}

impl Default for Appearance {
    fn default() -> Self {
        Self {
            notch_width: 188.0,
            notch_height: 32.0,
            expanded_width: 720.0,
            collapsed_opacity: 1.0,
            expanded_opacity: 1.0,
            blur: true,
            shadow: true,
            bounce: 0.3,
            animation_speed: 1.0,
            page_time: 1.0,
            close_time: 1.0,
            open_time: 1.0,
            accent: "auto".into(),
            look: "tuna".into(),
            white_beams: false,
            clock_style: "roll".into(),
            motion_cache: true,
            bottom_fade: true,
            glass_look: true,
            glass_live: true,
            lite_motion: true,
            mono_icons: true,
            page_blur: false,
            max_fps: 60.0,
            scale: 1.0,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Hotkeys {
    pub toggle: String,
    pub ai: String,
    pub timer: String,
    pub clipboard: String,
    pub play_pause: String,
    /// Voice dictation: tap to start/stop, or hold to talk and release to finish.
    pub dictate: String,
    /// The quick launcher: apps, files, sums and web addresses.
    pub launcher: String,
    /// The launcher as a history of what you copied (text, links, images, files). Caught by a
    /// low-level hook, so Win+C works even though Windows wants it.
    pub launcher_clip: String,
    /// The launcher as a one-question chat with Gemini (uses the key from Settings → Gemini).
    pub launcher_ai: String,
    /// The launcher as a Run box: type a command, Enter runs it, the Down arrow lists the recent ones. Caught
    /// by a low-level hook, so Win+R works even though Windows wants it.
    pub launcher_run: String,
    /// The window switcher (replaces Alt+Tab): a row of the open windows with a picture of each; Tab or the
    /// arrow keys choose, Enter or letting go of Alt opens. Shift added to these keys goes backwards.
    pub launcher_switch: String,
    /// Maximise the window in front (press again to put it back). Caught by a low-level hook, so Win+F works
    /// even though Windows wants it.
    pub window_max: String,
    /// Close the window in front.
    pub window_close: String,
    /// The radial menu of your apps around the pointer. Caught by a low-level hook, so Win+M works even
    /// though Windows wants it.
    pub radial: String,
}

impl Default for Hotkeys {
    fn default() -> Self {
        Self {
            toggle: "Ctrl+Alt+N".into(),
            ai: "Ctrl+Alt+Space".into(),
            timer: "Ctrl+Alt+T".into(),
            clipboard: "Ctrl+Alt+V".into(),
            play_pause: String::new(),
            dictate: "Ctrl+Alt+D".into(),
            launcher: "Alt+Space".into(),
            launcher_clip: "Win+C".into(),
            launcher_ai: "Win+G".into(),
            launcher_run: "Win+R".into(),
            launcher_switch: "Alt+Tab".into(),
            window_max: "Win+F".into(),
            window_close: "Win+Q".into(),
            radial: "Win+M".into(),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Modules {
    pub media: bool,
    pub timer: bool,
    pub downloads: bool,
    pub privacy: bool,
    pub clipboard: bool,
    pub battery: bool,
    pub ai: bool,
    pub voice: bool,
    pub shelf: bool,
    pub ports: bool,
    pub locks: bool,
    pub osd: bool,
    pub bluetooth: bool,
    pub stats: bool,
    pub weather: bool,
    pub wifi: bool,
    pub gemini: bool,
    pub notes: bool,
    pub greeting: bool,
    pub control: bool,
    pub notifications: bool,
    pub mirror: bool,
    pub devices: bool,
}

impl Default for Modules {
    fn default() -> Self {
        Self {
            media: true,
            timer: true,
            downloads: true,
            privacy: true,
            clipboard: true,
            battery: true,
            ai: true,
            voice: true,
            shelf: true,
            ports: true,
            locks: true,
            osd: true,
            bluetooth: true,
            stats: true,
            weather: true,
            wifi: true,
            gemini: true,
            notes: true,
            greeting: true,
            control: true,
            notifications: true,
            mirror: true,
            devices: true,
        }
    }
}

impl Modules {
    pub fn is_enabled(&self, id: &str) -> bool {
        match id {
            "media" => self.media,
            "timer" => self.timer,
            "downloads" => self.downloads,
            "privacy" => self.privacy,
            "clipboard" => self.clipboard,
            "battery" => self.battery,
            "ai" => self.ai,
            "voice" => self.voice,
            "shelf" => self.shelf,
            "ports" => self.ports,
            "locks" => self.locks,
            "osd" => self.osd,
            "bluetooth" => self.bluetooth,
            "stats" => self.stats,
            "weather" => self.weather,
            "wifi" => self.wifi,
            "gemini" => self.gemini,
            "notes" => self.notes,
            "greeting" => self.greeting,
            "control" => self.control,
            "notifications" => self.notifications,
            "mirror" => self.mirror,
            "devices" => self.devices,
            _ => true,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct MediaCfg {
    pub visualizer: bool,
    /// Show a peek when the track changes.
    pub peek_on_track_change: bool,
    /// Keep the live activity this many seconds after playback pauses.
    pub linger_after_pause_secs: u32,
}

impl Default for MediaCfg {
    fn default() -> Self {
        Self { visualizer: true, peek_on_track_change: true, linger_after_pause_secs: 20 }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct TimerCfg {
    pub presets_min: Vec<u32>,
    pub sound: bool,
    /// Gain applied to the "done" chime (1.0 = as recorded).
    pub sound_volume: f32,
}

impl Default for TimerCfg {
    fn default() -> Self {
        Self { presets_min: vec![1, 3, 5, 10, 15, 25, 45], sound: true, sound_volume: 1.6 }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct ClipboardCfg {
    pub history: usize,
    pub flash_on_copy: bool,
}

impl Default for ClipboardCfg {
    fn default() -> Self {
        Self { history: 8, flash_on_copy: true }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct DownloadsCfg {
    /// Extra folders to watch (the user's Downloads folder is always watched).
    pub folders: Vec<String>,
    pub peek_on_complete: bool,
}

impl Default for DownloadsCfg {
    fn default() -> Self {
        Self { folders: Vec::new(), peek_on_complete: true }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct BatteryCfg {
    pub low_threshold: u8,
    pub peek_on_plug: bool,
}

impl Default for BatteryCfg {
    fn default() -> Self {
        Self { low_threshold: 20, peek_on_plug: true }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct AiCfg {
    /// Any OpenRouter model id. The API key is never stored here: it is read
    /// from OPENROUTER_API_KEY (environment or a git-ignored `.env` file).
    pub model: String,
    /// OpenAI-compatible chat completions endpoint.
    pub endpoint: String,
    pub max_tokens: u32,
    pub system_prompt: String,
}

impl Default for AiCfg {
    fn default() -> Self {
        Self {
            model: "google/gemini-3.1-flash-lite".into(),
            endpoint: "https://openrouter.ai/api/v1/chat/completions".into(),
            max_tokens: 4096,
            system_prompt: "You are a quick assistant living in a small desktop notch. \
                Answer concisely: prefer a few short sentences or a compact list. Plain text, no markdown headings."
                .into(),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct LocksCfg {
    /// Peek when Caps Lock / Num Lock / Scroll Lock is toggled.
    pub caps: bool,
    pub num: bool,
    pub scroll: bool,
}

impl Default for LocksCfg {
    fn default() -> Self {
        Self { caps: true, num: true, scroll: false }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct ShelfCfg {
    /// Keep parked files across restarts (only the paths are stored).
    pub remember: bool,
}

impl Default for ShelfCfg {
    fn default() -> Self {
        Self { remember: true }
    }
}

/// The radial menu (hotkey: `[hotkeys] radial`): two rings of eight apps around the pointer.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct RadialCfg {
    pub enabled: bool,
    /// Hide the Windows taskbar while Dynamic Notch runs (it comes back when you quit or turn this off).
    pub hide_taskbar: bool,
    /// Tuna's dark look: graphite slices with a dark blue selection.
    pub tuna: bool,
    /// How much the glass blurs what is behind it (0 = not at all, 3 = a lot).
    pub blur: f32,
    /// How see-through the glass is (0 = a solid tint, 1 = clear glass).
    pub glass: f32,
    /// 16 slots, "name|what to open" (an empty text is an empty slot): the first 8 are the inner ring and
    /// the last 8 the outer ring, both clockwise from the top.
    pub apps: Vec<String>,
}

impl Default for RadialCfg {
    fn default() -> Self {
        let mut apps: Vec<String> = [
            "File Explorer|explorer.exe",
            "Edge|%ProgramFiles(x86)%\\Microsoft\\Edge\\Application\\msedge.exe",
            "Notepad|notepad.exe",
            "Calculator|calc.exe",
            "PowerShell|powershell.exe",
            "Settings|ms-settings:",
            "Task Manager|taskmgr.exe",
            "Snipping Tool|snippingtool.exe",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        apps.resize(16, String::new());
        Self { enabled: true, hide_taskbar: false, tuna: false, blur: 1.6, glass: 0.65, apps }
    }
}

impl RadialCfg {
    /// Always exactly 16 slots.
    fn fix(&mut self) {
        self.blur = self.blur.clamp(0.0, 3.0);
        self.glass = self.glass.clamp(0.0, 1.0);
        self.apps.truncate(16);
        self.apps.resize(16, String::new());
    }
}

/// Combos: your own hotkeys that open things.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct CombosCfg {
    /// "Win+B|browser": the keys, a bar, and what to open (an app, file, folder, link or `browser`).
    pub items: Vec<String>,
}

/// The quick launcher (hotkey: `[hotkeys] launcher`).
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct LauncherCfg {
    /// Also find files and folders (Desktop, Documents, Downloads, Pictures, Videos, Music).
    pub files: bool,
    /// More folders to search for files.
    pub folders: Vec<String>,
    /// How much the picture behind the window is blurred (0 = not at all, 1 = a lot).
    pub blur: f32,
    /// How dark the glass is (higher = darker).
    pub dark: f32,
    /// A small dark-grey wash on the dark look (0 = none).
    pub tint: f32,
    /// Opening, closing, the list dropping down and the underline all move.
    pub animate: bool,
    /// Light frosted glass with dark writing (false = dark glass with white writing).
    pub light: bool,
    /// How big the launcher is (1.0 = the original size).
    pub size: f32,
    /// Where a web search goes; what you typed is added at the end.
    pub search_url: String,
    /// Liquid glass: the edge bends what is behind it and catches the light (off = a plain solid card).
    pub glass: bool,
    /// The blur behind the launcher is renewed while it is open, so a video behind it keeps moving.
    pub live: bool,
    /// One wide box until the box with the things to do with a match is asked for (off = always two boxes).
    pub single_box: bool,
    /// The single box is a square in the middle of a narrower card (off = one wide box).
    pub square_box: bool,
    /// The Alcove look only: the launcher hotkeys open the launcher inside the notch (false = Tuna's launcher).
    pub alcove: bool,
    /// Handed over by the app, not stored here: the Gemini settings, whether media is playing, and
    /// the dock's rim (the launcher's rim follows it).
    #[serde(skip)]
    pub gemini: GeminiCfg,
    #[serde(skip)]
    pub media_playing: bool,
    #[serde(skip)]
    pub rim_width: f32,
    #[serde(skip)]
    pub rim_bright: f32,
}

impl Default for LauncherCfg {
    fn default() -> Self {
        Self {
            files: true,
            folders: Vec::new(),
            blur: 0.7,
            dark: 0.55,
            tint: 0.12,
            animate: true,
            light: false,
            size: 0.8,
            glass: true,
            live: true,
            single_box: true,
            square_box: false,
            alcove: true,
            search_url: "https://www.google.com/search?q=".to_string(),
            gemini: GeminiCfg::default(),
            media_playing: false,
            rim_width: 1.2,
            rim_bright: 0.35,
        }
    }
}

impl LauncherCfg {
    fn clamped(&mut self) {
        self.blur = self.blur.clamp(0.0, 1.0);
        self.dark = self.dark.clamp(0.2, 0.95);
        self.tint = self.tint.clamp(0.0, 0.6);
        self.size = self.size.clamp(0.5, 1.5);
        self.folders.truncate(20);
        let u = self.search_url.trim().to_string();
        // only web addresses: what is typed in the launcher is handed to the browser
        self.search_url =
            if u.starts_with("https://") || u.starts_with("http://") { u } else { Self::default().search_url };
    }
}

/// The dock at the bottom of the screen.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct DockCfg {
    pub enabled: bool,
    /// Icon size in logical pixels.
    pub icon_size: f32,
    /// How much an icon grows under the pointer (1.0 = not at all).
    pub magnify: f32,
    /// Also show the apps that are open but not pinned.
    pub show_running: bool,
    /// Gap between the dock and the bottom of the work area.
    pub bottom_margin: f32,
    /// "path", "Name|path" or "Name|path|icon.png".
    pub items: Vec<String>,
    /// Slide the dock away until the pointer touches the bottom edge.
    pub auto_hide: bool,
    /// Hide the Windows taskbar while the dock runs.
    pub hide_taskbar: bool,
    /// How dark the glass is: a black wash over the blur (0.1 = clear glass, 1.0 = nearly black).
    pub opacity: f32,
    /// Programs that never show up in the dock.
    pub hide_apps: Vec<String>,
    /// The white rim around the dock: thickness in pixels (0 = none) and brightness (0 - 1).
    pub rim_width: f32,
    pub rim_bright: f32,
    /// The bottom of the dock curves outward and flows into the screen edge, like the notch does at the top.
    pub melt: bool,
    /// The liquid glass edge, the same glass as the notch (see Settings → Glass).
    pub glass: bool,
    /// How frosted the glass is: Windows' blur behind the dock (0 = clear glass, 1 = full blur).
    pub blur: f32,
    /// The dock's edge bends the screen behind it like thick glass (the look is Settings -> Glass). It copies the
    /// screen once, so the dock is hidden from screenshots while this is on.
    pub refract: bool,
}

impl Default for DockCfg {
    fn default() -> Self {
        Self {
            enabled: true,
            icon_size: 56.0,
            magnify: 1.6,
            show_running: true,
            bottom_margin: 6.0,
            items: vec![r"Explorer|%WINDIR%\explorer.exe".to_string()],
            auto_hide: false,
            hide_taskbar: false,
            opacity: 0.30,
            hide_apps: vec!["raycast.exe".to_string()],
            rim_width: 1.2,
            rim_bright: 0.35,
            melt: false,
            glass: true,
            blur: 0.25,
            refract: true,
        }
    }
}

impl DockCfg {
    fn clamped(&mut self) {
        self.icon_size = self.icon_size.clamp(32.0, 96.0);
        self.magnify = self.magnify.clamp(1.0, 2.2);
        self.bottom_margin = self.bottom_margin.clamp(0.0, 80.0);
        self.opacity = self.opacity.clamp(0.1, 1.0);
        self.blur = self.blur.clamp(0.0, 1.0);
        self.hide_apps.truncate(60);
        self.rim_width = self.rim_width.clamp(0.0, 4.0);
        self.rim_bright = self.rim_bright.clamp(0.0, 1.0);
        self.items.truncate(40);
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct PortsCfg {
    /// Peek when a dev server starts listening.
    pub peek_on_start: bool,
    /// Always show these ports, whatever process owns them.
    pub extra_ports: Vec<u16>,
    /// Never show these ports.
    pub ignore_ports: Vec<u16>,
    /// Extra process names that count as dev servers (e.g. "myserver.exe").
    pub extra_processes: Vec<String>,
}

impl Default for PortsCfg {
    fn default() -> Self {
        Self { peek_on_start: true, extra_ports: Vec::new(), ignore_ports: Vec::new(), extra_processes: Vec::new() }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct VoiceCfg {
    /// Groq speech-to-text model. The API key is never stored here: it is read
    /// from GROQ_API_KEY (environment or a git-ignored `.env` file).
    pub model: String,
    /// OpenAI-compatible API base URL.
    pub endpoint: String,
    /// "" = detect automatically, or an ISO code like "en" / "de".
    pub language: String,
    /// Names and terms the transcription should spell correctly.
    pub vocabulary: Vec<String>,
    /// LLM pass after transcription: "light" (fillers, punctuation),
    /// "medium" (also light rephrasing) or "off".
    pub cleanup: String,
    pub cleanup_model: String,
    /// Paste into the focused app; otherwise the text is only copied.
    pub auto_paste: bool,
    /// Recording stops automatically after this long.
    pub max_seconds: u32,
}

impl Default for VoiceCfg {
    fn default() -> Self {
        Self {
            model: "whisper-large-v3-turbo".into(),
            endpoint: "https://api.groq.com/openai/v1".into(),
            language: String::new(),
            vocabulary: Vec::new(),
            cleanup: "light".into(),
            cleanup_model: "llama-3.1-8b-instant".into(),
            auto_paste: true,
            max_seconds: 300,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct OsdCfg {
    /// Peek when the output volume or mute state changes.
    pub volume: bool,
    /// Handle the volume keys ourselves so Windows' own flyout stays hidden
    /// (falls back to the Windows flyout while the notch is hidden or open).
    pub replace_system_flyout: bool,
    /// Volume change per key press, in percent.
    pub volume_step: u32,
    /// Peek when the built-in display's brightness changes.
    pub brightness: bool,
}

impl Default for OsdCfg {
    fn default() -> Self {
        Self { volume: true, replace_system_flyout: true, volume_step: 2, brightness: true }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct BluetoothCfg {
    pub peek_on_connect: bool,
    pub peek_on_disconnect: bool,
    /// Warn once when a connected device's battery drops to this percentage.
    pub low_battery: u8,
}

impl Default for BluetoothCfg {
    fn default() -> Self {
        Self { peek_on_connect: true, peek_on_disconnect: true, low_battery: 15 }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct HomeCfg {
    /// Up to four widgets for the right-hand column of the home page, top to bottom.
    /// Ids: weather, stats, timer, control (Wi-Fi & Bluetooth), notes, clipboard, downloads, shelf, ports.
    pub widgets: Vec<String>,
}

impl Default for HomeCfg {
    fn default() -> Self {
        Self { widgets: ["weather", "stats", "timer", "control"].iter().map(|s| s.to_string()).collect() }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct WeatherCfg {
    /// City to show, e.g. "Berlin" or "Springfield, IL". Empty = not set up yet.
    pub city: String,
    /// "metric" (°C, km/h) or "imperial" (°F, mph).
    pub units: String,
    /// Minimum minutes between automatic refreshes.
    pub refresh_minutes: u32,
}

impl Default for WeatherCfg {
    fn default() -> Self {
        Self { city: String::new(), units: "metric".into(), refresh_minutes: 20 }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct GeminiCfg {
    /// The API key is never stored here: paste it in Settings → Gemini, or put
    /// GEMINI_API_KEY in a .env file.
    pub model: String,
    pub endpoint: String,
    pub max_tokens: u32,
    pub system_prompt: String,
}

impl Default for GeminiCfg {
    fn default() -> Self {
        Self {
            model: "gemini-2.5-flash".into(),
            endpoint: "https://generativelanguage.googleapis.com/v1beta".into(),
            max_tokens: 2048,
            system_prompt: "You are a helpful assistant living in a small desktop notch. \
                Keep answers short and clear. Plain text, no markdown headings."
                .into(),
        }
    }
}

/// Where "Check for updates" (Settings → General) looks for new releases.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct UpdateCfg {
    /// GitHub `owner/name` whose newest release is offered.
    pub repo: String,
}

impl Default for UpdateCfg {
    fn default() -> Self {
        Self { repo: "TonyB1119-arch/Notch".into() }
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct GreetingCfg {
    /// Name to say hello to ("Hello Sam"). Empty = just "Hello".
    pub name: String,
    /// Name of an installed handwriting font for the hello (e.g. "Sacramento"). Empty = Yellowtail when installed, else the built-in pen drawing.
    pub font: String,
}

impl GreetingCfg {
    /// The font to write the hello with: the configured one, or Yellowtail when nothing is set
    /// (used only if it is installed; otherwise the built-in pen drawing is used).
    pub fn font_name(&self) -> &str {
        let f = self.font.trim();
        if f.is_empty() {
            "Yellowtail"
        } else {
            f
        }
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct NotchCfg {
    /// Up to three small widgets on the closed notch: clock, date, battery, weather, wifi, bluetooth, notes.
    pub widgets: Vec<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct TabsCfg {
    /// Order of the tabs in the header (module ids). Home always comes first;
    /// tabs not listed follow in the default order. Change it in Settings → Tabs.
    pub order: Vec<String>,
}

/// The look of the Alcove glass edge (Settings → Glass). Sizes are in logical pixels.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct GlassCfg {
    /// How far in from the edge the glass reaches.
    pub edge_width: f32,
    /// How far out the glass looks at its strongest (the warp).
    pub pull: f32,
    /// How sharply the bend is concentrated at the very edge (1 = even, 3 = only right at the edge).
    pub bend: f32,
    /// How solid the glass is at the edge (0.3 - 1).
    pub strength: f32,
    /// Extra warp in the rounded corners (0 - 1).
    pub corner: f32,
    /// Colour fringing: red, green and blue look slightly different distances out.
    pub fringe: f32,
    /// How far up the sides the glass is drawn before it fades out.
    pub reach: f32,
    /// How softly the glass starts at the outer edge.
    pub softness: f32,
    /// Glass across the middle of the notch, wherever it is even a little see-through (0 = off).
    pub middle: f32,
    /// How much the middle glass bends what is behind it (0 = clear, 1 = like the edge).
    pub middle_warp: f32,
    /// How much the middle glass softens what is behind it, in logical pixels (0 = sharp).
    pub middle_blur: f32,
    /// How see-through the bottom edge of the notch gets (0 = not at all).
    pub fade_amount: f32,
    /// How high up the bottom fade begins (1 = the usual height).
    pub fade_start: f32,
    /// How quickly the glass fades in (higher = quicker).
    pub glow_in: f32,
    /// Live glass: milliseconds between looks at the screen while media plays.
    pub live_ms: f32,
    /// Live glass: seconds for the edge to melt from one picture into the next.
    pub melt: f32,
    /// While the notch moves, the glass gets its finest detail once this fraction of the move is left.
    pub sharpen_at: f32,
    /// How far in from the edge the glass is worked out pixel for pixel, in edge widths (4 = everywhere).
    pub sharp_zone: f32,
    /// Stretch the glass edge like iOS Liquid Glass: the picture under the glass is pulled out toward
    /// the edge (it can fold over right at the rim) instead of the picture outside being pulled in.
    pub stretch: bool,
    /// Bend the picture the way real glass does (Snell's law on a rounded edge: strongest right at the
    /// rim, then easing away). Off: the older smooth curve. The bend curve setting sets the glass's thickness.
    pub snell: bool,
    /// Snell's law: the glass's index of refraction (1 = air, 1.5 = glass, 2.4 = diamond).
    pub snell_ior: f32,
    /// Snell's law: how thick the rounded edge is, as a multiple of its width (thicker bends more, further in).
    pub snell_thick: f32,
    /// Snell's law: how square the edge's profile is (2 = quarter circle, higher = flatter on top, sharper at the rim).
    pub snell_round: f32,
    /// Where the rim light comes from, in degrees (0 = right, 90 = below, 180 = left, 225 = upper left).
    pub light_angle: f32,
    /// How much the edge highlight follows that light (0 = the same all the way round, 1 = only the lit
    /// side and a softer glint on the far side, like Apple's glass).
    pub light_dir: f32,
    /// How wide the bright line along the edge is, in logical pixels.
    pub light_width: f32,
    /// Alcove: a press makes the control glow from within, from the pointer outwards (off = no glow).
    pub glow: bool,
    /// How bright that glow is (0 - 1).
    pub glow_amount: f32,
    /// A thin bright line along the very edge of the glass, like the light catching it (0 - 1).
    pub edge_light: f32,
    /// A light frost over the glass (0 - 1).
    pub tint: f32,
    /// Stretch mode: how far the stretch leans sideways toward the ends of the bottom edge (radians).
    pub fan: f32,
    /// Stretch mode: how long the stretch may be, relative to the longest one that never folds over.
    pub stretch_len: f32,
    /// How much the glass dims when the picture behind it is bright, so white text stays clear (0 - 1).
    pub readable: f32,
}

impl Default for GlassCfg {
    fn default() -> Self {
        Self {
            edge_width: 76.0,
            pull: 22.0,
            bend: 1.5,
            strength: 0.88,
            corner: 0.3,
            fringe: 0.05,
            reach: 90.0,
            softness: 4.0,
            middle: 0.85,
            middle_warp: 0.0,
            middle_blur: 0.0,
            fade_amount: 0.8,
            fade_start: 1.7,
            glow_in: 9.0,
            live_ms: 150.0,
            melt: 0.12,
            sharpen_at: 0.12,
            sharp_zone: 4.0,
            stretch: false,
            snell: true,
            snell_ior: 1.5,
            snell_thick: 1.5,
            snell_round: 3.0,
            light_angle: 225.0,
            light_dir: 0.7,
            light_width: 1.8,
            glow: true,
            glow_amount: 0.5,
            edge_light: 0.35,
            tint: 0.0,
            fan: 0.7,
            stretch_len: 1.0,
            readable: 0.4,
        }
    }
}

impl GlassCfg {
    fn clamped(&mut self) {
        self.edge_width = self.edge_width.clamp(20.0, 160.0);
        self.pull = self.pull.clamp(0.0, 60.0);
        self.bend = self.bend.clamp(0.8, 3.0);
        self.strength = self.strength.clamp(0.3, 1.0);
        self.corner = self.corner.clamp(0.0, 1.0);
        self.fringe = self.fringe.clamp(0.0, 0.2);
        self.reach = self.reach.clamp(30.0, 200.0);
        self.softness = self.softness.clamp(0.5, 12.0);
        self.middle = self.middle.clamp(0.0, 1.0);
        self.middle_warp = self.middle_warp.clamp(0.0, 1.0);
        self.middle_blur = self.middle_blur.clamp(0.0, 6.0);
        self.fade_amount = self.fade_amount.clamp(0.0, 1.0);
        self.fade_start = self.fade_start.clamp(0.5, 3.0);
        self.glow_in = self.glow_in.clamp(2.0, 30.0);
        self.live_ms = self.live_ms.clamp(80.0, 500.0);
        self.melt = self.melt.clamp(0.03, 0.5);
        self.sharpen_at = self.sharpen_at.clamp(0.02, 0.5);
        self.sharp_zone = self.sharp_zone.clamp(0.3, 4.0);
        self.edge_light = self.edge_light.clamp(0.0, 1.0);
        self.tint = self.tint.clamp(0.0, 1.0);
        self.fan = self.fan.clamp(0.0, 1.2);
        self.stretch_len = self.stretch_len.clamp(0.5, 1.6);
        self.readable = self.readable.clamp(0.0, 1.0);
    }
}

pub fn config_path() -> PathBuf {
    app_dir().join("config.toml")
}

// returned once per load and unpacked right away; boxing would buy nothing
#[allow(clippy::large_enum_variant)]
pub enum LoadResult {
    Ok(Config),
    /// File had errors; defaults (or last good config) should be kept.
    Invalid(String),
}

pub fn load() -> LoadResult {
    let path = config_path();
    match std::fs::read_to_string(&path) {
        Ok(text) => match toml::from_str::<Config>(&text) {
            Ok(c) => LoadResult::Ok(c.sanitized()),
            Err(e) => LoadResult::Invalid(e.message().to_string()),
        },
        Err(_) => {
            let _ = std::fs::write(&path, DEFAULT_TEMPLATE);
            LoadResult::Ok(Config::default())
        }
    }
}

/// Persist a single `[section] key = value` change while keeping the user's
/// comments and formatting intact.
pub fn set_value(section: &str, key: &str, value: &str) {
    let path = config_path();
    let text = std::fs::read_to_string(&path).unwrap_or_else(|_| DEFAULT_TEMPLATE.to_string());
    let mut out = Vec::new();
    let mut in_section = false;
    let mut done = false;
    let mut section_seen = false;
    let mut skipping = false;
    for line in text.lines() {
        let t = line.trim();
        if skipping {
            skipping = !line.contains(']');
            continue;
        }
        if t.starts_with('[') && t.ends_with(']') {
            if in_section && !done {
                out.push(format!("{key} = {value}"));
                done = true;
            }
            in_section = &t[1..t.len() - 1] == section;
            section_seen |= in_section;
        } else if in_section && !done {
            let k = t.split('=').next().unwrap_or("").trim();
            if k == key && !t.starts_with('#') {
                out.push(format!("{key} = {value}"));
                done = true;
                let rest = t.splitn(2, '=').nth(1).unwrap_or("").trim();
                skipping = rest.starts_with('[') && !rest.contains(']');
                continue;
            }
        }
        out.push(line.to_string());
    }
    if !done {
        if !section_seen {
            out.push(String::new());
            out.push(format!("[{section}]"));
        }
        out.push(format!("{key} = {value}"));
    }
    let _ = std::fs::write(&path, out.join("\n") + "\n");
}

impl Config {
    fn sanitized(mut self) -> Self {
        let a = &mut self.appearance;
        a.notch_width = a.notch_width.clamp(80.0, 600.0);
        a.notch_height = a.notch_height.clamp(18.0, 64.0);
        a.expanded_width = a.expanded_width.clamp(420.0, 1000.0);
        a.collapsed_opacity = a.collapsed_opacity.clamp(0.2, 1.0);
        a.expanded_opacity = a.expanded_opacity.clamp(0.2, 1.0);
        a.bounce = a.bounce.clamp(0.0, 1.0);
        a.animation_speed = a.animation_speed.clamp(0.25, 4.0);
        a.page_time = a.page_time.clamp(0.4, 2.5);
        a.close_time = a.close_time.clamp(0.4, 2.5);
        a.open_time = a.open_time.clamp(0.4, 2.5);
        a.scale = a.scale.clamp(0.5, 2.5);
        a.max_fps = a.max_fps.clamp(30.0, 240.0);
        // the soft blur along the bottom edge was taken out
        a.bottom_fade = false;
        self.clipboard.history = self.clipboard.history.clamp(1, 30);
        self.voice.max_seconds = self.voice.max_seconds.clamp(5, 780);
        self.osd.volume_step = self.osd.volume_step.clamp(1, 25);
        self.bluetooth.low_battery = self.bluetooth.low_battery.min(100);
        self.weather.refresh_minutes = self.weather.refresh_minutes.clamp(5, 360);
        self.glass.clamped();
        self.dock.clamped();
        self.launcher.clamped();
        self.combos.items.truncate(24);
        self.radial.fix();
        self.home.widgets.truncate(4);
        self.notch.widgets.truncate(3);
        self
    }
}

pub const DEFAULT_TEMPLATE: &str = r##"# Dynamic Notch configuration
# Saved changes are applied immediately (hot reload).

[general]
# "primary", "active" (follow the focused window) or a monitor number (1, 2, ...)
monitor = "primary"
# hovering only grows the notch slightly; a click opens it (set true to open on hover)
expand_on_hover = false
# short, quiet tick on hover and click
haptics = true
# Alcove: music and finished peeks leave small bubbles beside the notch
bubbles = true
# a very quiet blip as a bubble pops out and again as it goes
bubble_sounds = false
# Alcove: volume and brightness as one big round dial (false: the line)
hud_ring = true
# round dials fill clockwise (false: counter-clockwise)
hud_clockwise = false
hover_delay_ms = 140
collapse_delay_ms = 60
hide_in_fullscreen = true
# "notch" = always visible, "pill" = slim line when idle, "hidden" = only on activity/hover
idle_style = "notch"
start_with_windows = false

[appearance]
notch_width = 188
notch_height = 32
expanded_width = 720
collapsed_opacity = 1.0
expanded_opacity = 1.0
blur = true
shadow = true
# 0 = no overshoot ... 1 = very bouncy
bounce = 0.3
animation_speed = 1.0
# Alcove: how long a page switch, the notch closing and the notch opening take (1.0 = normal, higher = slower)
page_time = 1.0
close_time = 1.0
open_time = 1.0
# "tuna" (liquid glass, capsule gauges, control center, the launcher's deep blue), "alcove" (island layouts,
# clock and weather home, page dots) or "nothing" (dot-matrix glyph lights, red accent)
look = "tuna"
# false = blue beams with a hint of white, true = white beams with a hint of blue
white_beams = false
# "roll" (digits slide up, the next one rises in) or "flip" (flip-clock tiles)
clock_style = "roll"
# Draw content once and only move/fade it while opening, closing and switching pages
motion_cache = true
# Alcove: the bottom edge of the open notch softens into a light blur
bottom_fade = true
# Alcove: the notch's edge bends what is behind it (false = plain glass)
glass_look = true
# Alcove: while media plays and the notch is open, the glass edge keeps up with what is behind it
# (a little extra CPU; during that time the notch is hidden from screenshots and screen recordings)
glass_live = true
# Lighter animations: less CPU while the notch moves (the glass edge is refreshed less often, the slow
# end of a move uses fewer frames). false = the full-rate animation
lite_motion = true
# Monochrome icons: every icon is white or grey, no coloured tints or halos. false = coloured icons
mono_icons = true
# Page switches: false = the page slides and fades, true = the old page blurs out and the new one blurs in
page_blur = false
# Most frames a second while the notch moves (30 - 240). 60 is smooth and light; raise it on a fast display
# for smoother motion at a higher CPU cost.
max_fps = 60
# "auto" picks the accent from album art, or use "#RRGGBB"
accent = "auto"
scale = 1.0

[hotkeys]
# Modifiers: Ctrl, Alt, Shift, Win. Keys: A-Z, 0-9, F1-F24, Space, Enter, Tab, Esc,
# Up/Down/Left/Right, Home, End, PageUp, PageDown, Insert, Delete, `, -, =, [, ], ;, ', ",", ., /
# Leave empty to disable.
toggle = "Ctrl+Alt+N"
ai = "Ctrl+Alt+Space"
timer = "Ctrl+Alt+T"
clipboard = "Ctrl+Alt+V"
play_pause = ""
# Voice dictation: tap to start/stop, or hold to talk and release to finish.
dictate = "Ctrl+Alt+D"
# The quick launcher (apps, files, sums, web addresses). Leave empty to turn it off.
launcher = "Alt+Space"
# The launcher as a history of what you copied (text, links, images, files). Leave empty to turn it off.
launcher_clip = "Win+C"
# The launcher as a one-question chat with Gemini. Leave empty to turn it off.
launcher_ai = "Win+G"
# The launcher as a Run box (type a command; the Down arrow lists the recent ones). Leave empty to turn it off.
launcher_run = "Win+R"
# The window switcher in place of Alt+Tab: a row of your open windows, each with a picture. Tab or the arrow keys
# choose, Enter (or letting go of Alt after Tab) opens it. Leave empty to keep the Windows Alt+Tab.
launcher_switch = "Alt+Tab"
# Maximise the window in front (press again to put it back). Leave empty to turn it off.
window_max = "Win+F"
# Close the window in front. Leave empty to turn it off.
window_close = "Win+Q"
# The radial menu of your apps around the pointer (see [radial]). Leave empty to turn it off.
radial = "Win+M"

[modules]
media = true
timer = true
downloads = true
privacy = true
clipboard = true
battery = true
ai = true
voice = true
shelf = true
ports = true
locks = true
osd = true
stats = true
weather = true
control = true            # Wi-Fi & Bluetooth in one tab
notifications = true      # Windows notifications in the notch
mirror = true             # Mirror tab (the camera only runs while you press its button)
devices = true            # peeks when a USB drive or a display is connected
gemini = true
notes = true
# greet you when you unlock the PC
greeting = true

[media]
visualizer = true
peek_on_track_change = true
linger_after_pause_secs = 20

[timer]
presets_min = [1, 3, 5, 10, 15, 25, 45]
sound = true
# loudness of the "done" chime; 1.0 = system default, capped before it would clip
sound_volume = 1.6

[clipboard]
history = 8
flash_on_copy = true

[downloads]
# Extra folders to watch; your Downloads folder is always watched.
folders = []
peek_on_complete = true

[battery]
low_threshold = 20
peek_on_plug = true

[ai]
# The OpenRouter API key is NOT stored here. Put it in a .env file:
#   OPENROUTER_API_KEY=sk-or-...
# (searched next to the exe, in the project folder and in %APPDATA%\DynamicNotch)
# Any OpenRouter model id. Cheap & fast default; alternatives:
# "deepseek/deepseek-v4.1-flash" (cheapest), "anthropic/claude-haiku-4.5", "anthropic/claude-sonnet-5.5"
model = "google/gemini-3.1-flash-lite"
endpoint = "https://openrouter.ai/api/v1/chat/completions"
max_tokens = 4096
system_prompt = "You are a quick assistant living in a small desktop notch. Answer concisely: prefer a few short sentences or a compact list. Plain text, no markdown headings."

[voice]
# Speech to text with Groq. The API key is NOT stored here. Put it in a .env file:
#   GROQ_API_KEY=gsk_...
# (same places as OPENROUTER_API_KEY). Get one at https://console.groq.com/keys
model = "whisper-large-v3-turbo"
endpoint = "https://api.groq.com/openai/v1"
# "" = detect the language automatically, or e.g. "en", "de"
language = ""
# Names and terms to spell correctly, e.g. ["Groq", "DynamicNotch"]
vocabulary = []
# Clean-up pass after transcription: "light" (fillers, punctuation), "medium" (also light rephrasing) or "off"
cleanup = "light"
cleanup_model = "llama-3.1-8b-instant"
# Paste into the focused app; when false the text is only copied
auto_paste = true
max_seconds = 300

[locks]
# Peek when a lock key is toggled
caps = true
num = true
scroll = false

[shelf]
# Keep parked files across restarts (only the paths are stored)
remember = true

[ports]
# Peek when a dev server starts listening
peek_on_start = true
# Always show / never show these ports
extra_ports = []
ignore_ports = []
# Extra process names that count as dev servers, e.g. ["myserver.exe"]
extra_processes = []

[osd]
# Volume / brightness peeks
volume = true
# Take over the volume keys so the Windows flyout stays hidden
# (the Windows flyout still shows while the notch is hidden or open)
replace_system_flyout = true
# Percent per volume key press
volume_step = 2
# Built-in display only; Windows may still show its own brightness flyout
brightness = true

[bluetooth]
peek_on_connect = true
peek_on_disconnect = true
# Warn once when a connected device's battery drops to this percentage (0 = off)
low_battery = 15

[home]
# Widgets on the right of the home page (max 4, top to bottom). You can also pick them in Settings → Home.
# weather, stats, timer, control (Wi-Fi & Bluetooth), notes, clipboard, downloads, shelf, ports
widgets = ["weather", "stats", "timer", "control"]

[weather]
# Weather comes from open-meteo.com (no account or key). Example: "Berlin" or "Springfield, IL"
city = ""
# "metric" (°C, km/h) or "imperial" (°F, mph)
units = "metric"
refresh_minutes = 20

[gemini]
# Your Gemini key is NOT stored here: paste it in Settings → Gemini, or add
#   GEMINI_API_KEY=...
# to a .env file. Get one at https://aistudio.google.com/apikey
model = "gemini-2.5-flash"
endpoint = "https://generativelanguage.googleapis.com/v1beta"
max_tokens = 2048
system_prompt = "You are a helpful assistant living in a small desktop notch. Keep answers short and clear. Plain text, no markdown headings."

[update]
# "Check for updates" in Settings → General looks at the newest release of this GitHub repo (owner/name).
# Nothing is checked unless you press that button.
repo = "TonyB1119-arch/Notch"

[greeting]
# On unlock the notch scans, then writes "Hello" in handwriting (switch it off with greeting = false under [modules]).
# Name to add after it ("Hello Sam"); empty = just "Hello". Also in Settings → General.
name = ""
# Optional: the name of an installed handwriting font to write the hello with (e.g. "Sacramento", "Yellowtail").
# Empty = Yellowtail if it is installed, otherwise the built-in pen drawing. Also in Settings → General ("Script Regular" needs that font installed).
font = ""

[dock]
# A glass dock at the bottom of the screen: your pinned apps, then the apps that are open.
# To change an icon, put a .png named after the item (Chrome.png) or its exe (chrome.png)
# in %APPDATA%\DynamicNotch\dock-icons
enabled = true
icon_size = 56.0
magnify = 1.6
show_running = true
bottom_margin = 6.0
# Slide away until the pointer touches the bottom of the screen
auto_hide = false
# Hide the Windows taskbar while the dock is running (it comes back when the app closes)
hide_taskbar = false
# How dark the glass is: a black wash (0.1 = none, 1.0 = nearly black)
opacity = 0.30
# How frosted the glass is: Windows' blur behind the dock (0 = clear glass, 1 = full blur)
blur = 0.25
# The dock's edge bends the screen behind it like thick glass (Settings -> Glass sets the look). It looks at the
# screen once and is hidden from screenshots while on.
refract = true
# Programs that never appear in the dock
hide_apps = ['raycast.exe']
# The white rim around the dock: thickness (pixels, 0 = none) and brightness (0 - 1)
rim_width = 1.2
rim_bright = 0.35
# Let the bottom of the dock curve outward and melt into the screen edge (true / false)
melt = false
# The liquid glass edge, the same glass as the notch
glass = true
# Each item is "path", "Name|path" or "Name|path|icon.png". %VARS% work in paths.
items = ['Explorer|%WINDIR%\explorer.exe']

[launcher]
# A quick search (hotkey above): apps, files, sums like 12*(3+4), the weather ("cast"), web addresses
# and web search. Press Down to see the list of matches, Tab to pick what to do with the match.
# Also find files and folders in Desktop, Documents, Downloads, Pictures, Videos and Music
files = true
# More folders to search, for example folders = ['D:\Projects']
folders = []
# How much the picture behind the window is blurred (0 = not at all, 1 = a lot)
blur = 0.7
# How dark the glass is (0.2 = light, 0.95 = almost black)
dark = 0.55
# A small dark grey tint on the dark look (0 = none, 0.6 = strong)
tint = 0.12
# Opening, closing, the list dropping down and the underline move
animate = true
# Light frosted glass with dark writing (false = dark glass with white writing)
light = false
# Liquid glass: the edge bends what is behind it and catches the light (false = a plain solid card)
glass = true
# Live blur: the glass blurs with Windows' own live backdrop, so anything moving behind it (a playing video)
# keeps moving (false = one picture from when it opened).
live = true
# One wide box until the box with the things to do with a match is asked for (Right arrow, Ctrl+K);
# false = always two boxes
single_box = true
# The single box is a square (false = one wide box); it still splits into two boxes when asked for
square_box = false
# The Alcove look only: the launcher hotkeys open the launcher inside the notch (false = Tuna's launcher)
alcove = true
# How big the launcher is (0.5 = half, 1.0 = the original size)
size = 0.8
# Where a web search goes; what you typed is added at the end
search_url = "https://www.google.com/search?q="

[combos]
# Your own hotkeys that open things, easiest to change in Settings (Combos tab).
# Each one is 'keys|what to open'. "browser" means your default browser.
# For example: items = ['Win+B|browser', 'Ctrl+Alt+N|notepad.exe']
items = []

[radial]
# The round menu of your apps (hotkey: [hotkeys] radial), easiest to change in Settings (Radial tab).
enabled = true
# Hide the Windows taskbar while Dynamic Notch runs (it comes back when you quit or turn this off).
hide_taskbar = false
# Tuna's dark look for the menu: graphite slices with a dark blue selection (Settings -> Radial).
tuna = false
# How much the glass blurs what is behind it (0 = not at all, 3 = a lot).
blur = 1.6
# How see-through the glass is (0 = a solid tint, 1 = clear glass).
glass = 0.65
# 16 slots: the first 8 are the inner ring, the last 8 the outer ring, both clockwise from the top.
# Each one is 'name|what to open'; an empty text '' is an empty slot.
apps = ['File Explorer|explorer.exe', 'Edge|%ProgramFiles(x86)%\Microsoft\Edge\Application\msedge.exe', 'Notepad|notepad.exe', 'Calculator|calc.exe', 'PowerShell|powershell.exe', 'Settings|ms-settings:', 'Task Manager|taskmgr.exe', 'Snipping Tool|snippingtool.exe', '', '', '', '', '', '', '', '']

[glass]
# The Alcove glass edge (Settings → Glass). Sizes are in logical pixels; delete a line to get its default.
edge_width = 76.0
pull = 22.0
bend = 1.5
strength = 0.88
corner = 0.3
fringe = 0.05
reach = 90.0
softness = 4.0
middle = 0.85
middle_warp = 0.0
middle_blur = 0.0
fade_amount = 0.8
fade_start = 1.7
glow_in = 9.0
live_ms = 150.0
melt = 0.12
sharpen_at = 0.12
sharp_zone = 4.0
# true = the glass edge stretches the picture under it toward the edge (iOS Liquid Glass look);
# false = it pulls in the picture from outside the notch (the original look)
stretch = false
# a thin bright line along the very edge of the glass (0 - 1)
edge_light = 0.35
# a light frost over the glass (0 - 1)
tint = 0.0
# stretch mode: how far the stretch leans sideways toward the ends of the bottom edge (0 - 1.2)
fan = 0.7
# stretch mode: how long the stretch may be (1 = the longest that never folds over, up to 1.6)
stretch_len = 1.0
# the glass dims a little when what is behind it is bright, so white text stays clear (0 - 1)
readable = 0.4

[tabs]
# Order of the header tabs (Settings → Tabs). Empty = default order.
# ids: timer, stats, voice, clipboard, downloads, shelf, ports, ai, gemini, weather, control, notes, notifications, mirror
order = []

[notch]
# Up to 3 small widgets on the closed notch (Settings → Notch):
# clock, date, battery, weather, wifi, bluetooth, notes
widgets = []

"##;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn template_parses_to_defaults() {
        let c: Config = toml::from_str(DEFAULT_TEMPLATE).expect("template must parse");
        let d = Config::default();
        assert_eq!(c.general.monitor, d.general.monitor);
        assert_eq!(c.appearance.notch_width, d.appearance.notch_width);
        assert_eq!(c.hotkeys.ai, d.hotkeys.ai);
        assert_eq!(c.ai.model, d.ai.model);
        assert_eq!(c.timer.presets_min, d.timer.presets_min);
        assert_eq!(c.hotkeys.dictate, d.hotkeys.dictate);
        assert_eq!(c.hotkeys.launcher, d.hotkeys.launcher);
        assert_eq!(c.hotkeys.launcher_clip, d.hotkeys.launcher_clip);
        assert_eq!(c.hotkeys.launcher_ai, d.hotkeys.launcher_ai);
        assert_eq!(c.hotkeys.launcher_run, d.hotkeys.launcher_run);
        assert_eq!(c.hotkeys.launcher_switch, d.hotkeys.launcher_switch);
        assert_eq!(c.hotkeys.window_max, d.hotkeys.window_max);
        assert_eq!(c.hotkeys.window_close, d.hotkeys.window_close);
        assert_eq!(c.hotkeys.radial, d.hotkeys.radial);
        assert_eq!(c.radial.apps, d.radial.apps);
        assert_eq!(c.voice.model, d.voice.model);
        assert_eq!(c.voice.cleanup, d.voice.cleanup);
        assert_eq!(c.locks.caps, d.locks.caps);
        assert_eq!(c.ports.peek_on_start, d.ports.peek_on_start);
        assert!(c.modules.voice && c.modules.shelf && c.modules.ports && c.modules.locks);
        assert!(c.modules.osd && c.modules.bluetooth && c.modules.stats);
        assert_eq!(c.osd.volume_step, d.osd.volume_step);
        assert_eq!(c.osd.replace_system_flyout, d.osd.replace_system_flyout);
        assert_eq!(c.bluetooth.low_battery, d.bluetooth.low_battery);
        assert_eq!(c.home.widgets, d.home.widgets);
        assert_eq!(c.weather.units, d.weather.units);
        assert_eq!(c.gemini.model, d.gemini.model);
        assert!(
            c.modules.weather
                && c.modules.control
                && c.modules.notifications
                && c.modules.gemini
                && c.modules.notes
                && c.modules.greeting
        );
        assert_eq!(c.greeting.name, d.greeting.name);
        assert_eq!(c.notch.widgets, d.notch.widgets);
        assert_eq!(c.tabs.order, d.tabs.order);
    }

    #[test]
    fn partial_config() {
        let c: Config = toml::from_str("[appearance]\nblur = false\n").unwrap();
        assert!(!c.appearance.blur);
        assert_eq!(c.general.monitor, "primary");
    }
}
