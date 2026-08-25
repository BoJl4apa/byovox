//! Everything that must live on the main thread: the winit event loop, the tray icon and
//! menu, the floating pill, and the audio cues. The pipeline thread reaches it only
//! through `ProxyIndicator`, which posts `UserEvent`s.

use std::path::{Path, PathBuf};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use tray_icon::menu::{CheckMenuItem, Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tray_icon::{Icon, TrayIcon, TrayIconBuilder};
use winit::application::ApplicationHandler;
use winit::event::WindowEvent;
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy};
use winit::window::{Window, WindowId, WindowLevel};

use crate::hotkey::{HotkeyEvent, HotkeyMode};
use crate::indicator::{Indicator, IndicatorState};
use crate::pipeline::Shared;

/// `Shared` behind a lock a pipeline panic may have poisoned.
///
/// Every reader here is the main thread inside winit's Win32 frames, or the IPC handler:
/// unwinding either through foreign frames over a flag the panicking thread had already
/// finished writing is worse than reading the state it left. The pipeline posts `Quit` when
/// it dies, so the poisoned state is short-lived by construction. Same call `ipc.rs` already
/// makes for its handler mutex.
fn shared_of(lock: &Mutex<Shared>) -> std::sync::MutexGuard<'_, Shared> {
    lock.lock().unwrap_or_else(|p| p.into_inner())
}

/// How long the Error indication is held before the UI falls back to Idle.
const ERROR_HOLD: Duration = Duration::from_secs(3);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MenuAction {
    ToggleEnabled,
    ToggleMode,
    ShowLast,
    OpenConfig,
    OpenLogs,
    RunCheck,
    Quit,
}

#[derive(Debug, Clone)]
pub enum UserEvent {
    Indicator(IndicatorState),
    Menu(MenuAction),
    Quit,
}

pub struct UiOptions {
    pub pill: bool,
    pub cue: bool,
    pub version: &'static str,
}

/// The pipeline's view of the UI: posts state changes to the main thread.
#[derive(Clone)]
pub struct ProxyIndicator(pub EventLoopProxy<UserEvent>);

impl Indicator for ProxyIndicator {
    /// Never blocks: `send_event` queues and wakes the loop. A closed loop is not an error
    /// here — the process is on its way out and the indication no longer has a consumer.
    fn set(&mut self, state: IndicatorState) {
        let _ = self.0.send_event(UserEvent::Indicator(state));
    }
}

pub fn build_event_loop() -> Result<(EventLoop<UserEvent>, EventLoopProxy<UserEvent>)> {
    let event_loop = EventLoop::<UserEvent>::with_user_event()
        .build()
        .context("event loop")?;
    let proxy = event_loop.create_proxy();
    Ok((event_loop, proxy))
}

pub fn icon_rgba(state: IndicatorState) -> Vec<u8> {
    let (r, g, b) = match state {
        IndicatorState::Idle | IndicatorState::Done => (140, 140, 140),
        IndicatorState::Recording => (220, 40, 40),
        IndicatorState::Working => (230, 160, 30),
        IndicatorState::Error => (200, 30, 90),
    };
    let mut px = vec![0u8; 32 * 32 * 4];
    for y in 0..32 {
        for x in 0..32 {
            let dx = x as f32 - 15.5;
            let dy = y as f32 - 15.5;
            if dx * dx + dy * dy <= 13.0 * 13.0 {
                let i = (y * 32 + x) * 4;
                px[i..i + 4].copy_from_slice(&[r, g, b, 255]);
            }
        }
    }
    px
}

/// The mode the tray's Mode item selects next. Two modes, so the item is a check mark on
/// "Toggle mode" rather than a submenu.
fn other_mode(current: HotkeyMode) -> HotkeyMode {
    match current {
        HotkeyMode::Hold => HotkeyMode::Toggle,
        HotkeyMode::Toggle => HotkeyMode::Hold,
    }
}

pub fn pill_text(state: IndicatorState) -> Option<&'static str> {
    match state {
        IndicatorState::Recording => Some("●  recording"),
        IndicatorState::Working => Some("…  working"),
        IndicatorState::Error => Some("✕  failed"),
        IndicatorState::Idle | IndicatorState::Done => None,
    }
}

/// The word the tray's status line uses. Done is idle to a reader: the state exists only to
/// separate the completion cue from the silent returns to Idle.
fn state_word(state: IndicatorState) -> &'static str {
    match state {
        IndicatorState::Idle | IndicatorState::Done => "idle",
        IndicatorState::Recording => "recording",
        IndicatorState::Working => "working",
        IndicatorState::Error => "error",
    }
}

struct App {
    opts: UiOptions,
    shared: Arc<Mutex<Shared>>,
    hotkey_tx: Sender<HotkeyEvent>,
    config_path: PathBuf,
    log_dir: PathBuf,
    proxy: EventLoopProxy<UserEvent>,
    tray: Option<TrayIcon>,
    items: Option<MenuItems>,
    pill: Option<Pill>,
    cue: Option<Cue>,
    error_until: Option<Instant>,
    enabled: bool,
    mode: HotkeyMode,
}

struct MenuItems {
    status: MenuItem,
    enabled: MenuItem,
    /// Checked = toggle, cleared = hold.
    mode: CheckMenuItem,
    show_last: MenuItem,
    open_config: MenuItem,
    open_logs: MenuItem,
    run_check: MenuItem,
    quit: MenuItem,
}

impl App {
    fn build_tray(&mut self) -> Result<()> {
        let menu = Menu::new();
        let items = MenuItems {
            status: MenuItem::new("byovox: idle", false, None),
            enabled: MenuItem::new(if self.enabled { "Disable" } else { "Enable" }, true, None),
            mode: CheckMenuItem::new("Toggle mode", true, self.mode == HotkeyMode::Toggle, None),
            show_last: MenuItem::new("Show last transcript", true, None),
            open_config: MenuItem::new("Open config", true, None),
            open_logs: MenuItem::new("Open logs", true, None),
            run_check: MenuItem::new("Run check", true, None),
            quit: MenuItem::new("Quit", true, None),
        };
        menu.append_items(&[
            &items.status,
            &PredefinedMenuItem::separator(),
            &items.enabled,
            &items.mode,
            &items.show_last,
            &items.open_config,
            &items.open_logs,
            &items.run_check,
            &PredefinedMenuItem::separator(),
            &items.quit,
        ])?;
        let tray = TrayIconBuilder::new()
            .with_menu(Box::new(menu))
            .with_tooltip(format!("byovox {}", self.opts.version))
            .with_icon(Icon::from_rgba(icon_rgba(IndicatorState::Idle), 32, 32)?)
            .build()?;
        // Forward menu clicks into the winit loop instead of polling a receiver. muda wants
        // a `Fn + Send + Sync` handler and an `EventLoopProxy` is `Send` but not `Sync`, so
        // the proxy rides in a mutex it is the only user of.
        let proxy = Mutex::new(self.proxy.clone());
        let ids = [
            (items.enabled.id().clone(), MenuAction::ToggleEnabled),
            (items.mode.id().clone(), MenuAction::ToggleMode),
            (items.show_last.id().clone(), MenuAction::ShowLast),
            (items.open_config.id().clone(), MenuAction::OpenConfig),
            (items.open_logs.id().clone(), MenuAction::OpenLogs),
            (items.run_check.id().clone(), MenuAction::RunCheck),
            (items.quit.id().clone(), MenuAction::Quit),
        ];
        // This runs inside the tray's window procedure: a panic here would unwind through
        // foreign frames, so a poisoned lock drops the click instead.
        MenuEvent::set_event_handler(Some(move |ev: MenuEvent| {
            if let Some((_, action)) = ids.iter().find(|(id, _)| *id == ev.id)
                && let Ok(p) = proxy.lock()
            {
                let _ = p.send_event(UserEvent::Menu(*action));
            }
        }));
        self.tray = Some(tray);
        self.items = Some(items);
        Ok(())
    }

    /// Paint one indicator state. `sound` is false for the timed fall back from Error to
    /// Idle, which must not add a second tone to the one the error already played.
    fn apply(&mut self, state: IndicatorState, sound: bool) {
        tracing::debug!(?state, sound, "indicator");
        if let (Some(tray), Some(items)) = (&self.tray, &self.items) {
            if let Ok(icon) = Icon::from_rgba(icon_rgba(state), 32, 32) {
                let _ = tray.set_icon(Some(icon));
            }
            // `last_error` is already a content-free summary; see `pipeline::summary`.
            let last_error = shared_of(&self.shared).last_error.clone();
            let label = match (state, last_error) {
                (IndicatorState::Error, Some(e)) => {
                    format!("byovox: error — {}", e.chars().take(60).collect::<String>())
                }
                (s, _) => format!("byovox: {}", state_word(s)),
            };
            items.status.set_text(label);
        }
        if let Some(pill) = &mut self.pill {
            pill.show(pill_text(state));
        }
        if sound && let Some(cue) = &mut self.cue {
            match state {
                IndicatorState::Recording => cue.play(880.0, 70),
                // Only a dictation that landed: a tap, a cancel and an empty transcript are
                // silent returns to Idle, and must not sound like a success.
                IndicatorState::Done => cue.play(660.0, 60),
                IndicatorState::Error => cue.play(220.0, 220),
                IndicatorState::Idle | IndicatorState::Working => {}
            }
        }
        self.error_until = (state == IndicatorState::Error).then(|| Instant::now() + ERROR_HOLD);
    }

    fn menu(&mut self, action: MenuAction, event_loop: &ActiveEventLoop) {
        match action {
            MenuAction::Quit => event_loop.exit(),
            MenuAction::ToggleEnabled => {
                self.enabled = !self.enabled;
                // Disabling must close the microphone now, not at the next hotkey event, so
                // the cancel goes in before the pipeline can see the flag.
                if !self.enabled {
                    let _ = self.hotkey_tx.send(HotkeyEvent::Cancel);
                }
                shared_of(&self.shared).enabled = self.enabled;
                tracing::info!(enabled = self.enabled, "dictation toggled from the tray");
                if let Some(items) = &self.items {
                    items
                        .enabled
                        .set_text(if self.enabled { "Disable" } else { "Enable" });
                }
            }
            MenuAction::ToggleMode => {
                self.mode = other_mode(self.mode);
                // A mode change mid-recording would strand the microphone — in toggle mode
                // the matching release is ignored — so the recording is cancelled first.
                // Cancel is a no-op unless the pipeline is recording, and it is obeyed in
                // either mode.
                let _ = self.hotkey_tx.send(HotkeyEvent::Cancel);
                shared_of(&self.shared).mode = self.mode;
                tracing::info!(mode = ?self.mode, "hotkey mode changed from the tray");
                // muda flips the check itself on click; it is set from our own state so the
                // mark and the pipeline cannot disagree.
                if let Some(items) = &self.items {
                    items.mode.set_checked(self.mode == HotkeyMode::Toggle);
                }
            }
            MenuAction::ShowLast => {
                let text = shared_of(&self.shared)
                    .last_transcript
                    .clone()
                    .unwrap_or_else(|| "(nothing yet)".into());
                // The dialog pumps its own modal loop. Called from here it would freeze the
                // event loop until dismissed: the tray icon would stop following the
                // pipeline, and a `byovox quit` would be acknowledged over IPC and then not
                // acted on. The box owns no window, so it runs on a thread of its own.
                std::thread::spawn(move || show_message("byovox — last transcript", &text));
            }
            MenuAction::OpenConfig => open_path(&self.config_path),
            MenuAction::OpenLogs => open_path(&self.log_dir),
            MenuAction::RunCheck => {
                if let Ok(exe) = std::env::current_exe() {
                    let _ = std::process::Command::new(exe)
                        .arg("check")
                        .arg("--pause")
                        .spawn();
                }
            }
        }
    }
}

impl ApplicationHandler<UserEvent> for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.tray.is_none() {
            if let Err(e) = self.build_tray() {
                tracing::error!(error = %e, "tray icon failed; continuing without it");
            }
            if self.opts.pill && self.pill.is_none() {
                match Pill::new(event_loop) {
                    Ok(p) => self.pill = Some(p),
                    Err(e) => {
                        tracing::warn!(error = %e, "pill window failed; continuing without it")
                    }
                }
            }
            // Nothing to fail yet: the output device is opened by the first cue, not here.
            if self.opts.cue && self.cue.is_none() {
                self.cue = Some(Cue::new());
            }
        }
    }

    fn user_event(&mut self, event_loop: &ActiveEventLoop, event: UserEvent) {
        match event {
            UserEvent::Indicator(s) => self.apply(s, true),
            UserEvent::Menu(a) => self.menu(a, event_loop),
            UserEvent::Quit => event_loop.exit(),
        }
    }

    fn window_event(&mut self, _: &ActiveEventLoop, _: WindowId, event: WindowEvent) {
        let Some(pill) = &mut self.pill else {
            return;
        };
        match event {
            WindowEvent::RedrawRequested => pill.draw(),
            // The text is laid out in physical pixels, so a move to a display with a
            // different DPI has to repaint or the pill reads at the old scale.
            WindowEvent::ScaleFactorChanged { .. } => pill.window.request_redraw(),
            _ => {}
        }
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        match self.error_until {
            Some(t) if Instant::now() >= t => {
                self.error_until = None;
                self.apply(IndicatorState::Idle, false);
                event_loop.set_control_flow(ControlFlow::Wait);
            }
            Some(t) => event_loop.set_control_flow(ControlFlow::WaitUntil(t)),
            None => event_loop.set_control_flow(ControlFlow::Wait),
        }
    }
}

/// Blocks until Quit. `hotkey_tx` is the pipeline thread's own event channel: the tray uses
/// it to cancel an in-flight recording when dictation is disabled.
pub fn run(
    event_loop: EventLoop<UserEvent>,
    opts: UiOptions,
    shared: Arc<Mutex<Shared>>,
    hotkey_tx: Sender<HotkeyEvent>,
    config_path: PathBuf,
    log_dir: PathBuf,
) -> Result<()> {
    let proxy = event_loop.create_proxy();
    // The menu's Enable/Disable label and Mode check have to start where the pipeline
    // actually is, not at an assumed `true` and an assumed hold.
    let (enabled, mode) = {
        let s = shared_of(&shared);
        (s.enabled, s.mode)
    };
    let mut app = App {
        opts,
        shared,
        hotkey_tx,
        config_path,
        log_dir,
        proxy,
        tray: None,
        items: None,
        pill: None,
        cue: None,
        error_until: None,
        enabled,
        mode,
    };
    event_loop.run_app(&mut app).context("event loop")?;
    Ok(())
}

// ---- pill ----------------------------------------------------------------------------

const FONT: &[u8] = include_bytes!("../assets/DejaVuSans.ttf");
const PILL_W: u32 = 170;
const PILL_H: u32 = 36;
/// Pill background and text, as one grey level each: the buffer is 0RGB and the pill paints
/// nothing but shades between them.
const BG: u32 = 0x0020_2020;
const FG: u32 = 0xF0;

struct Pill {
    window: std::rc::Rc<Window>,
    surface: softbuffer::Surface<std::rc::Rc<Window>, std::rc::Rc<Window>>,
    font: fontdue::Font,
    text: Option<&'static str>,
}

impl Pill {
    /// Created hidden: the first `show` positions it by the cursor before it is mapped, so
    /// it never flashes at the origin. It is also painted once here, so that first show maps
    /// a surface that already has its background in it rather than uninitialised memory.
    fn new(event_loop: &ActiveEventLoop) -> Result<Pill> {
        let attrs = Window::default_attributes()
            .with_title("byovox")
            .with_decorations(false)
            .with_resizable(false)
            .with_visible(false)
            .with_active(false)
            .with_window_level(WindowLevel::AlwaysOnTop)
            .with_inner_size(winit::dpi::LogicalSize::new(PILL_W, PILL_H));
        #[cfg(windows)]
        let attrs = {
            use winit::platform::windows::WindowAttributesExtWindows;
            attrs.with_skip_taskbar(true)
        };
        let window = std::rc::Rc::new(event_loop.create_window(attrs).context("pill window")?);
        deny_activation(&window)?;
        let ctx = softbuffer::Context::new(window.clone()).map_err(|e| anyhow::anyhow!("{e}"))?;
        let surface =
            softbuffer::Surface::new(&ctx, window.clone()).map_err(|e| anyhow::anyhow!("{e}"))?;
        let font = fontdue::Font::from_bytes(FONT, fontdue::FontSettings::default())
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        let mut pill = Pill {
            window,
            surface,
            font,
            text: None,
        };
        pill.draw();
        Ok(pill)
    }

    fn show(&mut self, text: Option<&'static str>) {
        self.text = text;
        match text {
            Some(_) => {
                if let Some((x, y)) = cursor_position() {
                    self.window
                        .set_outer_position(winit::dpi::PhysicalPosition::new(x + 24, y + 24));
                }
                self.window.set_visible(true);
                self.window.request_redraw();
            }
            None => self.window.set_visible(false),
        }
        // Setting it once at creation is not enough: winit recomputes the *whole* extended
        // style from its own WindowFlags on every visibility change and writes it back, so
        // each show and each hide erases the bit. Re-asserting it here leaves the window
        // carrying it at every moment a show can happen. `set_visible` runs inline on the
        // event loop thread, which this is, so the restore is ordered after the wipe.
        if let Err(e) = deny_activation(&self.window) {
            tracing::warn!(error = %e, "pill lost its no-activation style");
        }
    }

    fn draw(&mut self) {
        let size = self.window.inner_size();
        let (w, h) = (size.width.max(1), size.height.max(1));
        if self
            .surface
            .resize(w.try_into().unwrap(), h.try_into().unwrap())
            .is_err()
        {
            return;
        }
        let Ok(mut buf) = self.surface.buffer_mut() else {
            return;
        };
        buf.fill(BG);
        if let Some(text) = self.text {
            // The window is sized in logical units but the buffer is physical pixels, so
            // every layout number here is scaled or the text shrinks as the display's DPI
            // rises.
            let scale = self.window.scale_factor() as f32;
            let mut pen_x = 12.0 * scale;
            let baseline = (24.0 * scale) as i32;
            for ch in text.chars() {
                let (metrics, bitmap) = self.font.rasterize(ch, 16.0 * scale);
                for (i, a) in bitmap.iter().enumerate() {
                    let px = pen_x as i32 + metrics.xmin + (i % metrics.width.max(1)) as i32;
                    let py = baseline - metrics.height as i32 - metrics.ymin
                        + (i / metrics.width.max(1)) as i32;
                    if px >= 0 && py >= 0 && (px as u32) < w && (py as u32) < h {
                        let idx = (py as u32 * w + px as u32) as usize;
                        // Source-over, not replace: glyph boxes overlap, and overwriting
                        // would let one glyph's transparent margin erase its neighbour's
                        // antialiasing back to the background.
                        let dst = buf[idx] & 0xFF;
                        let shade = dst + (*a as u32 * (FG.saturating_sub(dst)) / 255);
                        buf[idx] = (shade << 16) | (shade << 8) | shade;
                    }
                }
                pen_x += metrics.advance_width;
            }
        }
        let _ = buf.present();
    }
}

/// Keep the pill out of the activation chain, the taskbar and Alt-Tab, for good.
///
/// `WS_EX_NOACTIVATE` is what stops a **click** on the pill from taking focus. The pill appears
/// at cursor + (24, 24) at the start of every recording, so it lands under the pointer, and a
/// `WM_MOUSEACTIVATE` there would pull focus off the window the transcript is about to be typed
/// into. `ShowWindow` itself is already safe — the window is created with `active: false`, and
/// winit's `MARKER_ACTIVATE` never persists, because `apply_diff` takes `self` by value and so
/// mutates a throwaway copy — so this bit covers the activation paths `SW_SHOWNOACTIVATE` does
/// not.
///
/// `WS_EX_TOOLWINDOW` keeps the taskbar button and the Alt-Tab entry away. winit's
/// `with_skip_taskbar` is an `ITaskbarList::DeleteTab` COM call rather than a style bit, and it
/// is re-run only on the `TASKBAR_CREATED` broadcast — while `apply_diff` writes
/// `WS_EX_APPWINDOW` back on the very pass that wipes this word, so without this the button
/// returns on a hide/re-show. `WS_EX_APPWINDOW` has to come *off* in the same write:
/// it forces a top-level window onto the taskbar and takes precedence over `WS_EX_TOOLWINDOW`,
/// so setting the two bits without clearing it leaves the pill qualifying for a button by the
/// shell's own listing rule. All three are read back for the same reason.
///
/// Both bits must be re-asserted after every visibility change: `apply_diff` recomputes the
/// whole extended-style word from winit's own `WindowFlags` on every `set_visible`, and never
/// emits either of these. See `Pill::show`. On a winit bump, re-check (a) that `apply_diff`
/// still recomputes the whole ex-style word, and (b) that `execute_in_thread` still runs inline
/// on the event-loop thread — that is what orders the re-assert *after* the wipe instead of
/// racing it.
///
/// At construction a failure is fatal and `Pill::new`'s caller drops the pill, since a pill
/// that can steal focus is worse than no pill. From `show` there is no pill left to drop, so
/// that caller can only WARN and carry on.
#[cfg(windows)]
fn deny_activation(window: &Window) -> Result<()> {
    use windows::Win32::Foundation::HWND;
    use windows::Win32::UI::WindowsAndMessaging::{
        GWL_EXSTYLE, GetWindowLongPtrW, SetWindowLongPtrW, WS_EX_APPWINDOW, WS_EX_NOACTIVATE,
        WS_EX_TOOLWINDOW,
    };
    use winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};

    let handle = window.window_handle().context("pill window handle")?;
    let RawWindowHandle::Win32(h) = handle.as_raw() else {
        anyhow::bail!("pill window is not a Win32 window");
    };
    let hwnd = HWND(h.hwnd.get() as *mut std::ffi::c_void);
    let wanted = (WS_EX_NOACTIVATE.0 | WS_EX_TOOLWINDOW.0) as isize;
    let unwanted = WS_EX_APPWINDOW.0 as isize;
    // SAFETY: `hwnd` is this window's live handle and both calls only read/write its own
    // extended style word.
    let readback = unsafe {
        let ex = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
        SetWindowLongPtrW(hwnd, GWL_EXSTYLE, (ex | wanted) & !unwanted);
        GetWindowLongPtrW(hwnd, GWL_EXSTYLE)
    };
    // SetWindowLongPtrW returns the old value, which is indistinguishable from failure, so
    // the style is read back instead of trusting the return. All three bits must land: any
    // one of them taking and another not would leave a silently half-fixed pill.
    if readback & wanted != wanted || readback & unwanted != 0 {
        anyhow::bail!(
            "the pill window kept the wrong extended style (0x{readback:08X}): wanted \
             WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW, not WS_EX_APPWINDOW"
        );
    }
    Ok(())
}

#[cfg(not(windows))]
fn deny_activation(_window: &Window) -> Result<()> {
    Ok(()) // Plans 2/3: the X11/Wayland/AppKit equivalents.
}

#[cfg(windows)]
fn cursor_position() -> Option<(i32, i32)> {
    use windows::Win32::Foundation::POINT;
    use windows::Win32::UI::WindowsAndMessaging::GetCursorPos;
    let mut p = POINT::default();
    // SAFETY: GetCursorPos writes a POINT.
    unsafe { GetCursorPos(&mut p).ok()? };
    Some((p.x, p.y))
}

#[cfg(not(windows))]
fn cursor_position() -> Option<(i32, i32)> {
    None // Plans 2/3: per-platform; the pill then stays at its last position
}

// ---- cues ----------------------------------------------------------------------------

/// The audio cues.
///
/// The sink is opened by the first cue and re-opened after any failure, rather than held for
/// the loop's lifetime: an output endpoint can be absent when the daemon starts and appear
/// later (a headset, a dock, an endpoint that flaps), and a daemon that runs for weeks has to
/// pick it up. Cues are decoration, so every failure is soft — but a dead endpoint must not
/// write one WARN per dictation either, so a failure streak reports once and the next success
/// re-arms it.
struct Cue {
    sink: Option<rodio::MixerDeviceSink>,
    /// Set when a failure has already been reported, cleared by the next success.
    warned: bool,
}

impl Cue {
    fn new() -> Cue {
        Cue {
            sink: None,
            warned: false,
        }
    }

    fn play(&mut self, hz: f32, ms: u64) {
        match self.open_and_play(hz, ms) {
            Ok(()) => self.warned = false,
            Err(e) => {
                // Drop the handle so the next cue opens a fresh one: a device that came back
                // is only reachable through a new sink.
                self.sink = None;
                if !self.warned {
                    self.warned = true;
                    tracing::warn!(error = %e, "audio cue unavailable; continuing without it");
                }
            }
        }
    }

    /// Only the open is fallible: `Mixer::add` returns nothing, so a stream that dies *after*
    /// a successful open is invisible here until something else forces a re-open.
    fn open_and_play(&mut self, hz: f32, ms: u64) -> Result<()> {
        use rodio::Source;
        let sink = match &mut self.sink {
            Some(sink) => sink,
            None => {
                let mut sink =
                    rodio::DeviceSinkBuilder::open_default_sink().context("audio output")?;
                // Without rodio's `tracing` feature its drop notice is a raw `eprintln!`;
                // byovox logs its own audio failures and a tray app must not scribble on
                // stderr.
                sink.log_on_drop(false);
                self.sink.insert(sink)
            }
        };
        let src = rodio::source::SineWave::new(hz)
            .take_duration(Duration::from_millis(ms))
            .amplify(0.15);
        sink.mixer().add(src);
        Ok(())
    }
}

// ---- helpers -------------------------------------------------------------------------

fn open_path(p: &Path) {
    #[cfg(windows)]
    let _ = std::process::Command::new("explorer").arg(p).spawn();
    #[cfg(target_os = "macos")]
    let _ = std::process::Command::new("open").arg(p).spawn();
    #[cfg(all(unix, not(target_os = "macos")))]
    let _ = std::process::Command::new("xdg-open").arg(p).spawn();
}

#[cfg(windows)]
fn show_message(title: &str, text: &str) {
    use windows::Win32::UI::WindowsAndMessaging::{MB_OK, MessageBoxW};
    use windows::core::HSTRING;
    // SAFETY: MessageBoxW with owned wide strings.
    unsafe { MessageBoxW(None, &HSTRING::from(text), &HSTRING::from(title), MB_OK) };
}

#[cfg(not(windows))]
fn show_message(title: &str, _text: &str) {
    // Plan 2: notify-rust / dialog. The transcript stays out of the log until there is a
    // dialog to put it in — `byovox last` is the only way to see it on this platform.
    tracing::info!(title, "no message dialog on this platform yet");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn icon_rgba_is_32x32_and_state_coloured() {
        let idle = icon_rgba(IndicatorState::Idle);
        assert_eq!(idle.len(), 32 * 32 * 4);
        let rec = icon_rgba(IndicatorState::Recording);
        // centre pixel: red channel dominates when recording, not when idle
        let c = (16 * 32 + 16) * 4;
        assert!(rec[c] > rec[c + 1] && rec[c] > rec[c + 2]);
        assert!(idle[c] == idle[c + 1] && idle[c] == idle[c + 2]);
    }

    /// The tray's Mode item is a check mark, so the click has to land on the other mode
    /// and the mark has to follow the mode rather than muda's own auto-flip.
    #[test]
    fn the_mode_item_flips_between_hold_and_toggle() {
        assert_eq!(other_mode(HotkeyMode::Hold), HotkeyMode::Toggle);
        assert_eq!(other_mode(HotkeyMode::Toggle), HotkeyMode::Hold);
        assert_eq!(other_mode(other_mode(HotkeyMode::Hold)), HotkeyMode::Hold);
    }

    #[test]
    fn pill_text_per_state() {
        assert_eq!(pill_text(IndicatorState::Recording), Some("●  recording"));
        assert_eq!(pill_text(IndicatorState::Working), Some("…  working"));
        assert_eq!(pill_text(IndicatorState::Idle), None);
    }
}
