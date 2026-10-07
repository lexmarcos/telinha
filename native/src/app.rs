//! The bubble: UI state, springs and the wiring to the engine, the tray and
//! Discord. Drawing lives in ui/*.slint (the design system is in DESIGN.md);
//! here we only decide what to show and how it moves.
//!
//! Animations are springs (ui/spring.rs), not Slint easing curves: a spring
//! starts from where the thing is and keeps its velocity when the target changes
//! mid-way, so opening and closing quickly never jumps or "hits a wall".

slint::include_modules!();

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use slint::winit_030::WinitWindowAccessor;
use slint::{ComponentHandle, Model, ModelRc, SharedString, VecModel};

use crate::config::{Config, Priority, Resolution};
use crate::engine::{self, Command, EncoderInfo, Event, Viewer};
use crate::ui::motion;
use crate::ui::spring::{Spring, reduced_motion};

/// Measurements from ui/tokens.slint the app needs to move the window.
const PANEL_W: f32 = 264.0;
const PANEL_GAP: f32 = 10.0;
const SHADOW_ROOM: f32 = 18.0;
const BUBBLE: f32 = 64.0;
/// How far the window shifts left when the panel opens on that side.
const LEFT_SHIFT: f32 = PANEL_W + PANEL_GAP;
const FPS_OPTIONS: [u32; 2] = [30, 60];
const DOTS_MAX: usize = 5;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Panel {
    Menu = 1,
    Quality = 2,
    Join = 3,
    Discord = 4,
    /// A short message beside the bubble that closes on its own.
    Toast = 5,
}

impl Panel {
    fn from_index(i: i32) -> Self {
        match i {
            2 => Self::Quality,
            3 => Self::Join,
            4 => Self::Discord,
            5 => Self::Toast,
            _ => Self::Menu,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
enum Session {
    Idle,
    Connecting,
    Channel { code: String, invite: String, live: bool },
}

struct Controller {
    ui: Bolha,
    config: Config,
    session: Session,
    viewers: Vec<Viewer>,
    encoder: Option<EncoderInfo>,
    screen: Option<(u32, u32)>,
    monitor: Option<(f32, f32)>,
    notice: Option<(String, NoteTone)>,
    /// Title, text and tone of the toast (panel 5).
    toast: (String, String, NoteTone),
    toast_timer: slint::Timer,
    /// The current stream only lets people in the Discord call watch (the bot
    /// announces it); otherwise the invite is copied when it starts.
    live_gated: bool,
    /// A newer version on the server, and its download (0..1, negative when idle).
    update: Option<crate::update::Release>,
    update_progress: f32,
    join_text: String,
    joining: bool,

    panel: Option<Panel>,
    side_left: bool,
    /// The panel just opened: its height is applied directly, without animating,
    /// while it is still almost transparent.
    fresh_panel: bool,
    /// When the panel switched. Slint only creates a panel's items on its
    /// first draw, so the measurement in the first moments comes out short.
    switched_at: Option<Instant>,
    open: Spring,
    press: Spring,
    tally: Spring,
    /// Segmented-control indicators: resolution, fps, priority, audio and who
    /// can watch.
    thumbs: [Spring; 5],
    panel_h: Spring,
    fade: Spring,
    /// Height the window reserves for the panel: during a switch, the larger of
    /// before and after (the window resizes twice, not every frame).
    hold_h: f32,
    dragging: bool,
    /// When the connecting spinner started turning.
    spin_from: Instant,
    timer: slint::Timer,

    engine: tokio::sync::mpsc::Sender<Command>,
    tray: Option<crate::tray::Guard>,
    /// The X11 clipboard needs an owner while the copied text
    /// is still there.
    clipboard: Option<arboard::Clipboard>,
}

thread_local! {
    static CTRL: RefCell<Option<Controller>> = const { RefCell::new(None) };
}

/// Operates on the controller (on the UI thread).
fn with(f: impl FnOnce(&mut Controller)) {
    CTRL.with(|c| {
        if let Some(c) = c.borrow_mut().as_mut() {
            f(c);
        }
    });
}

/// Sends work from another thread (engine, tray, Discord) to the UI.
fn post(f: impl FnOnce(&mut Controller) + Send + 'static) {
    let _ = slint::invoke_from_event_loop(move || with(f));
}

pub fn run() {
    crate::ui::spring::detect_reduced_motion();
    let ui = Bolha::new().expect("criar a janela da bolha");
    ui.global::<Motion>().set_reduced(reduced_motion());

    let engine = engine::spawn(|e| post(move |c| c.on_engine(e)));
    let tray = crate::tray::init(Arc::new(|e| post(move |c| c.on_tray(e))));

    let config = Config::load();
    let mut c = Controller {
        ui: ui.clone_strong(),
        session: Session::Idle,
        viewers: Vec::new(),
        encoder: None,
        screen: None,
        monitor: None,
        notice: None,
        toast: (String::new(), String::new(), NoteTone::Info),
        toast_timer: slint::Timer::default(),
        live_gated: false,
        update: None,
        update_progress: -1.0,
        join_text: String::new(),
        joining: false,
        panel: None,
        side_left: false,
        fresh_panel: false,
        switched_at: None,
        open: Spring::new(0.0, motion::UI),
        press: Spring::new(0.0, motion::PRESS),
        tally: Spring::new(0.0, motion::TALLY),
        thumbs: [Spring::new(0.0, motion::UI); 5],
        panel_h: Spring::new(0.0, motion::UI),
        fade: Spring::new(1.0, motion::FADE),
        hold_h: 0.0,
        dragging: false,
        spin_from: Instant::now(),
        timer: slint::Timer::default(),
        engine,
        tray,
        clipboard: None,
        config,
    };
    c.sync_thumbs(true);
    c.wire();
    c.render();
    ui.show().expect("mostrar a bolha");
    c.place();
    CTRL.with(|x| *x.borrow_mut() = Some(c));

    if let Some(dir) = std::env::var_os("TELINHA_SHOTS").map(PathBuf::from) {
        shots(dir);
    } else {
        watch_updates();
    }
    // The window hides to the tray without quitting: only "Fechar o Telinha" quits.
    slint::run_event_loop_until_quit().expect("laço da interface");
}

impl Controller {
    /* ---------------- ligações com a interface ---------------- */

    fn wire(&self) {
        let ui = &self.ui;
        ui.on_bubble_down(|| with(|c| c.bubble_down()));
        ui.on_bubble_up(|| with(|c| c.bubble_up()));
        ui.on_bubble_drag(|| with(|c| c.bubble_drag()));
        ui.on_bubble_click(|| with(|c| c.bubble_click()));
        ui.on_escape(|| with(|c| c.close_panel()));

        let st = ui.global::<AppState>();
        st.on_open(|i| with(|c| c.open_panel(Panel::from_index(i))));
        st.on_create(|| {
            with(|c| {
                let (name, server) = (c.config.name.clone(), c.config.server.clone());
                c.send(Command::Create { name, server });
                c.close_panel();
            })
        });
        st.on_start_live(|| {
            with(|c| {
                let quality = c.config.quality;
                let discord = if c.config.call_only { c.config.discord_session.clone() } else { None };
                c.live_gated = discord.is_some();
                c.send(Command::StartLive { quality, discord });
                c.close_panel();
            })
        });
        st.on_stop_live(|| with(|c| c.stop_live()));
        st.on_copy_invite(|| {
            with(|c| {
                if c.copy_invite() {
                    c.tell("Convite copiado", "", NoteTone::Info);
                } else {
                    c.tell("", "Não consegui copiar o convite.", NoteTone::Error);
                }
            })
        });
        st.on_dismiss(|| with(|c| c.close_panel()));
        st.on_update(|| with(|c| c.start_update()));
        st.on_hide(|| with(|c| c.hide()));
        st.on_leave(|| {
            with(|c| {
                c.send(Command::Leave);
                c.close_panel();
            })
        });
        st.on_quit(|| with(|c| c.quit()));
        st.on_join_edited(|t| {
            with(|c| {
                c.join_text = t.trim().to_owned();
                c.render();
            })
        });
        st.on_name_edited(|t| with(|c| c.config.name = t.to_string()));
        st.on_join_submit(|| with(|c| c.join_submit()));
        st.on_set_resolution(|i| {
            with(|c| {
                if let Some(r) = c.resolution_options().get(i as usize) {
                    c.config.quality.resolution = *r;
                    c.quality_changed();
                }
            })
        });
        st.on_set_fps(|i| {
            with(|c| {
                c.config.quality.fps = FPS_OPTIONS[(i as usize).min(FPS_OPTIONS.len() - 1)];
                c.quality_changed();
            })
        });
        st.on_set_priority(|i| {
            with(|c| {
                c.config.quality.priority = if i == 1 { Priority::Nitidez } else { Priority::Fluidez };
                c.quality_changed();
            })
        });
        st.on_set_audio(|i| {
            with(|c| {
                c.config.quality.system_audio = i == 1;
                c.quality_changed();
            })
        });
        st.on_set_call(|i| {
            with(|c| {
                c.config.call_only = i == 1;
                c.config.save();
                c.sync_thumbs(false);
                c.render();
            })
        });
        st.on_discord_login(|| with(|c| c.discord_login()));
        st.on_discord_logout(|| {
            with(|c| {
                c.config.discord_session = None;
                c.config.discord_name = None;
                c.config.save();
                c.render();
            })
        });
    }

    /* ---------------- a bolha ---------------- */

    fn bubble_down(&mut self) {
        // Respond on press, not on release.
        self.dragging = false;
        self.press.set_target(1.0);
        self.animate();
    }

    fn bubble_up(&mut self) {
        self.press.set_target(0.0);
        self.animate();
    }

    fn bubble_drag(&mut self) {
        if self.dragging {
            return;
        }
        self.dragging = true;
        self.press.set_target(0.0);
        self.animate();
        // The window manager drags: follows the mouse 1:1, with no lag.
        self.ui.window().with_winit_window(|w| {
            let _ = w.drag_window();
        });
    }

    fn bubble_click(&mut self) {
        if std::mem::take(&mut self.dragging) {
            return;
        }
        if self.panel == Some(Panel::Toast) && self.open.target() > 0.0 {
            self.open_panel(Panel::Menu);
        } else if self.panel.is_some() && self.open.target() > 0.0 {
            self.close_panel();
        } else {
            self.open_panel(Panel::Menu);
        }
    }

    /* ---------------- painel ---------------- */

    fn open_panel(&mut self, kind: Panel) {
        let was_closed = self.panel.is_none() || self.open.target() == 0.0;
        let switched = self.panel.is_some_and(|p| p != kind);
        self.panel = Some(kind);
        self.notice = None;
        self.open.set_target(1.0);
        if was_closed && self.open.value() <= 0.001 {
            self.fresh_panel = true;
            // The content arrives right after the material (which also forces
            // OpenGL rendering to redraw the panel; see ui/app.slint).
            self.fade.snap(0.0);
            self.fade.set_target(1.0);
            // Opens on the side with room on screen; the bubble stays put.
            let (x, y) = self.bubble_position();
            let needed = 2.0 * SHADOW_ROOM + BUBBLE.max(self.dots_width()) + LEFT_SHIFT;
            self.side_left = self.monitor.is_some_and(|(w, _)| x + needed > w);
            if self.side_left {
                self.set_window_position(x - LEFT_SHIFT, y);
            }
        } else if switched {
            // Panel switch: the new content appears while the height follows.
            self.fade.snap(0.0);
            self.fade.set_target(1.0);
            self.switched_at = Some(Instant::now());
        }
        self.render();
        self.animate();
    }

    /// Tells the person something: inside the open panel, or in a toast
    /// beside the bubble when no panel is open (so it is not missed).
    fn tell(&mut self, title: &str, text: &str, tone: NoteTone) {
        let panel_open = self.panel.is_some_and(|p| p != Panel::Toast) && self.open.target() > 0.0;
        if panel_open {
            let line = match (title.is_empty(), text.is_empty()) {
                (true, _) => text.to_owned(),
                (_, true) => format!("{title}."),
                _ => format!("{title}. {text}"),
            };
            self.notice = Some((line, tone));
            self.render();
            return;
        }
        self.toast = (title.to_owned(), text.to_owned(), tone);
        self.open_panel(Panel::Toast);
        // Problems stay longer than good news.
        let secs = if tone == NoteTone::Info { 4 } else { 8 };
        self.toast_timer.start(slint::TimerMode::SingleShot, Duration::from_secs(secs), || {
            with(|c| {
                if c.panel == Some(Panel::Toast) {
                    c.close_panel();
                }
            })
        });
    }

    fn close_panel(&mut self) {
        if self.panel.is_none() {
            return;
        }
        self.open.set_target(0.0);
        self.animate();
    }

    /// The panel finished closing: the window goes back to the bubble size.
    fn finish_close(&mut self) {
        let (x, y) = self.bubble_position();
        self.panel = None;
        self.hold_h = 0.0;
        if std::mem::take(&mut self.side_left) {
            self.set_window_position(x, y);
        }
        self.render();
    }

    /// Measures the panel content (Slint computes its natural height) and
    /// animates the surface to it.
    fn measure_panel(&mut self) {
        if self.panel.is_none() {
            return;
        }
        let h = self.ui.get_content_h();
        if self.fresh_panel {
            self.panel_h.snap(h);
            self.hold_h = h;
            // Stops being "just opened" only once the panel is visible.
            if self.open.value() > 0.3 {
                self.fresh_panel = false;
            }
        } else if self.switched_at.is_some_and(|t| t.elapsed() < Duration::from_millis(40)) {
            // Still building the new panel: wait for the real measurement.
            self.animate();
        } else if (h - self.panel_h.target()).abs() > 0.5 {
            self.panel_h.set_target(h);
            self.hold_h = self.hold_h.max(h).max(self.panel_h.value());
            self.animate();
        }
        self.ui.set_hold_h(self.hold_h);
        self.ui.set_panel_h(self.panel_h.value());
    }

    /* ---------------- molas ---------------- */

    /// The bubble spinner turns while connecting (still with reduced motion).
    fn spinning(&self) -> bool {
        self.session == Session::Connecting && !reduced_motion()
    }

    fn animating(&self) -> bool {
        self.spinning()
            || self.fresh_panel
            || self.open.is_moving()
            || self.press.is_moving()
            || self.tally.is_moving()
            || self.panel_h.is_moving()
            || self.fade.is_moving()
            || self.thumbs.iter().any(Spring::is_moving)
    }

    /// Starts the spring timer (only runs while something is moving).
    fn animate(&mut self) {
        if !self.timer.running() {
            self.timer.start(slint::TimerMode::Repeated, Duration::from_millis(8), || with(|c| c.frame()));
        }
    }

    fn frame(&mut self) {
        let now = Instant::now();
        for s in [&mut self.open, &mut self.press, &mut self.tally, &mut self.panel_h, &mut self.fade] {
            s.tick(now);
        }
        for s in &mut self.thumbs {
            s.tick(now);
        }
        if self.panel.is_some() && self.open.target() == 0.0 && !self.open.is_moving() {
            self.finish_close();
        }
        self.measure_panel();
        // Panel switch done: the window shrinks to the new height.
        if !self.panel_h.is_moving() && self.hold_h > self.panel_h.target() + 0.5 && self.open.target() > 0.0 {
            self.hold_h = self.panel_h.target();
        }
        self.push_motion();
        if !self.animating() {
            self.timer.stop();
        }
    }

    fn push_motion(&self) {
        let ui = &self.ui;
        ui.set_open(self.open.value());
        ui.set_press(self.press.value());
        ui.set_tally(self.tally.value());
        ui.set_panel_h(self.panel_h.value());
        ui.set_hold_h(self.hold_h);
        ui.set_content_fade(self.fade.value());
        // One turn every 1.4 s.
        let turns = if self.spinning() { self.spin_from.elapsed().as_secs_f32() / 1.4 } else { 0.0 };
        ui.set_spin(turns.fract() * 360.0);
        let thumbs: Vec<f32> = self.thumbs.iter().map(Spring::value).collect();
        ui.global::<AppState>().set_thumbs(ModelRc::new(VecModel::from(thumbs)));
    }

    /* ---------------- desenho ---------------- */

    /// Pushes the whole state to the UI.
    fn render(&mut self) {
        let ui = &self.ui;
        let st = ui.global::<AppState>();
        let (session, code, live) = match &self.session {
            Session::Idle => (0, String::new(), false),
            Session::Connecting => (1, String::new(), false),
            Session::Channel { code, live, .. } => (2, code.clone(), *live),
        };
        ui.set_window_title(
            match &self.session {
                Session::Channel { code, live: true, .. } => format!("Telinha · transmitindo no canal {code}"),
                Session::Channel { code, .. } => format!("Telinha · canal {code}"),
                _ => "Telinha".into(),
            }
            .into(),
        );
        st.set_session(session);
        st.set_code(code.clone().into());
        st.set_live(live);
        st.set_subtitle(self.subtitle().into());

        let palette = ui.global::<Palette>().get_avatars();
        let dots: Vec<DotData> = self
            .viewers
            .iter()
            .take(DOTS_MAX)
            .map(|v| DotData {
                initials: initials(&v.name).into(),
                tint: palette.row_data(name_hash(&v.name) % palette.row_count().max(1)).unwrap_or_default(),
                supported: v.supported,
            })
            .collect();
        st.set_dots(ModelRc::new(VecModel::from(dots)));
        st.set_dots_extra(self.viewers.len().saturating_sub(DOTS_MAX) as i32);
        let (notice, tone) = self.notice.clone().unwrap_or((String::new(), NoteTone::Info));
        st.set_notice(notice.into());
        st.set_notice_tone(tone);
        st.set_update_version(self.update.as_ref().map(|r| r.version.clone()).unwrap_or_default().into());
        st.set_update_progress(self.update_progress);
        let (title, text, tone) = &self.toast;
        st.set_toast_title(title.into());
        st.set_toast_text(text.into());
        st.set_toast_tone(*tone);

        // Quality
        let q = self.config.quality;
        let opts = self.resolution_options();
        let labels: Vec<SharedString> = opts.iter().map(|r| r.height().map_or("Original".into(), |h| format!("{h}p").into())).collect();
        st.set_res_options(ModelRc::new(VecModel::from(labels)));
        st.set_res_sel(opts.iter().position(|r| *r == q.resolution).unwrap_or(opts.len() - 1) as i32);
        st.set_fps_sel(FPS_OPTIONS.iter().position(|f| *f == q.fps).unwrap_or(1) as i32);
        st.set_prio_sel(i32::from(q.priority == Priority::Nitidez));
        st.set_audio_sel(i32::from(q.system_audio));
        let native = self.native();
        let (ow, oh) = q.output_size(native);
        st.set_screen_note(format!("Sua tela: {}×{}. Saindo em {ow}×{oh}.", native.0, native.1).into());
        st.set_prio_note(
            if q.priority == Priority::Fluidez { "Mantém o movimento suave. Bom pra jogos e vídeos." } else { "Mantém a imagem nítida. Bom pra texto e código." }.into(),
        );
        let (enc, enc_tone) = match &self.encoder {
            Some(e) if e.hardware => (format!("Codificando na placa de vídeo ({}).", e.name), NoteTone::Info),
            Some(e) => (format!("Codificando no processador ({}). Acima de 1080p60 ele pode não dar conta.", e.name), NoteTone::Warn),
            None => (String::new(), NoteTone::Info),
        };
        st.set_encoder_note(enc.into());
        st.set_encoder_tone(enc_tone);
        let mbps = q.bitrate(native) as f64 / 1e6;
        let br = |v: f64| format!("{v:.1}").replace('.', ",");
        let upload = match self.viewers.len() {
            n if n > 1 => format!("Até {} Mb/s de upload por pessoa. Com {n} pessoas assistindo, até {} Mb/s.", br(mbps), br(mbps * n as f64)),
            _ => format!("Até {} Mb/s de upload por pessoa assistindo.", br(mbps)),
        };
        st.set_upload_note(upload.into());

        // Join
        if st.get_join_text().as_str() != self.join_text {
            st.set_join_text(self.join_text.clone().into());
        }
        st.set_join_digits(!self.join_text.is_empty() && self.join_text.chars().all(|c| c.is_ascii_digit()));
        if st.get_name().as_str() != self.config.name {
            st.set_name(self.config.name.clone().into());
        }
        st.set_joining(self.joining);

        // Discord
        st.set_discord_logged(self.config.discord_session.is_some());
        st.set_discord_name(self.config.discord_name.clone().unwrap_or_default().into());
        st.set_call_sel(i32::from(self.config.call_only));

        ui.set_panel_kind(self.panel.map_or(0, |p| p as i32));
        ui.set_panel_shown(self.panel.is_some());
        ui.set_side_left(self.side_left);
        self.measure_panel();
        self.push_motion();
    }

    fn subtitle(&self) -> String {
        match &self.session {
            Session::Idle => "Compartilhe a tela com os amigos".into(),
            Session::Connecting => "Conectando…".into(),
            Session::Channel { code, .. } => format!("Canal {code}. {}", self.viewers_line()),
        }
    }

    /// Who is watching, by name when there are few (more specific than a count).
    fn viewers_line(&self) -> String {
        let first: Vec<&str> = self.viewers.iter().map(|v| v.name.split_whitespace().next().unwrap_or("Alguém")).collect();
        match first.as_slice() {
            [] => "Ninguém assistindo ainda".into(),
            [a] => format!("{a} assistindo"),
            [a, b] => format!("{a} e {b} assistindo"),
            [a, b, c] => format!("{a}, {b} e {c} assistindo"),
            many => format!("{} pessoas assistindo", many.len()),
        }
    }

    /* ---------------- janela ---------------- */

    /// Initial position and screen size (to pick the panel side).
    fn place(&mut self) {
        self.monitor = self
            .ui
            .window()
            .with_winit_window(|w| {
                w.current_monitor().map(|m| {
                    let s = m.size().to_logical::<f64>(m.scale_factor());
                    (s.width as f32, s.height as f32)
                })
            })
            .flatten();
        let (x, y) = match (self.config.position, self.monitor) {
            (Some(p), _) => p,
            // First time: at the top, near the right.
            (None, Some((w, _))) => (w - BUBBLE - 2.0 * SHADOW_ROOM - 48.0, 120.0),
            (None, None) => (200.0, 120.0),
        };
        self.set_window_position(x, y);
        self.sync_thumbs(true);
        self.render();
    }

    fn window_position(&self) -> (f32, f32) {
        let p = self.ui.window().position();
        let s = self.ui.window().scale_factor();
        (p.x as f32 / s, p.y as f32 / s)
    }

    fn set_window_position(&self, x: f32, y: f32) {
        self.ui.window().set_position(slint::LogicalPosition::new(x, y));
    }

    /// Where the bubble is (with the panel open on the left, the window starts before it).
    fn bubble_position(&self) -> (f32, f32) {
        let (x, y) = self.window_position();
        if self.panel.is_some() && self.side_left { (x + LEFT_SHIFT, y) } else { (x, y) }
    }

    fn dots_width(&self) -> f32 {
        let n = self.viewers.len().min(DOTS_MAX) + usize::from(self.viewers.len() > DOTS_MAX);
        if n == 0 { 0.0 } else { 20.0 + (n as f32 - 1.0) * 13.0 }
    }

    fn hide(&mut self) {
        self.config.position = Some(self.bubble_position());
        self.config.save();
        // Hide at once: whoever asks to hide doesn't want to watch the panel close.
        self.open.snap(0.0);
        self.finish_close();
        let _ = self.ui.hide();
    }

    fn show(&mut self) {
        let _ = self.ui.show();
        if let Some((x, y)) = self.config.position {
            self.set_window_position(x, y);
        }
    }

    fn quit(&mut self) {
        self.config.position = Some(self.bubble_position());
        self.config.save();
        let _ = slint::quit_event_loop();
    }

    /* ---------------- atualização ---------------- */

    fn update_found(&mut self, r: crate::update::Release) {
        let news = self.update.as_ref().is_none_or(|u| u.version != r.version);
        self.update = Some(r.clone());
        if news {
            self.tell(&format!("Versão {} disponível", r.version), "Clique na bolha para atualizar.", NoteTone::Info);
        }
        self.render();
    }

    fn start_update(&mut self) {
        let Some(r) = self.update.clone() else { return };
        if self.update_progress >= 0.0 {
            return; // already downloading
        }
        if matches!(self.session, Session::Channel { live: true, .. }) {
            self.tell("", "Pare a transmissão antes de atualizar.", NoteTone::Warn);
            return;
        }
        if !crate::update::can_install(&r) {
            // This copy cannot replace itself: the download page can.
            if let Some(page) = crate::update::page() {
                let _ = crate::engine::gate::open_browser(&page);
            }
            self.tell("", "Baixe a nova versão na página que abriu no navegador.", NoteTone::Info);
            return;
        }
        self.update_progress = 0.0;
        self.render();
        tokio::spawn(async move {
            // One UI update per percent, not per downloaded chunk.
            let shown = std::sync::atomic::AtomicU32::new(0);
            let progress = move |p: f32| {
                let pct = (p.clamp(0.0, 1.0) * 100.0) as u32;
                if shown.swap(pct, std::sync::atomic::Ordering::Relaxed) != pct {
                    post(move |c| {
                        c.update_progress = pct as f32 / 100.0;
                        c.render();
                    });
                }
            };
            let r = crate::update::install(r, progress).await;
            post(move |c| match r {
                // The new version is starting: this one gets out of the way.
                Ok(()) => c.quit(),
                Err(e) => {
                    tracing::warn!("update: {e}");
                    c.update_progress = -1.0;
                    c.tell("Não deu para atualizar", &e, NoteTone::Error);
                    c.render();
                }
            });
        });
    }

    /* ---------------- ações ---------------- */

    fn send(&self, cmd: Command) {
        let _ = self.engine.try_send(cmd);
    }

    fn stop_live(&mut self) {
        self.send(Command::StopLive);
        self.close_panel();
    }

    /// Puts the channel invite on the clipboard. `false` if it could not.
    fn copy_invite(&mut self) -> bool {
        let Session::Channel { invite, .. } = &self.session else { return false };
        let invite = invite.clone();
        if std::env::var_os("TELINHA_MOCK").is_some() {
            return true; // screenshots and tests do not touch the real clipboard
        }
        if self.clipboard.is_none() {
            self.clipboard = arboard::Clipboard::new().map_err(|e| tracing::warn!("clipboard: {e}")).ok();
        }
        self.clipboard.as_mut().is_some_and(|c| c.set_text(invite).is_ok())
    }

    fn join_submit(&mut self) {
        if self.join_text.is_empty() {
            self.notice = Some(("Digite o número do canal ou cole o link de convite.".into(), NoteTone::Error));
            self.render();
            return;
        }
        self.config.save();
        let (input, name, server) = (self.join_text.clone(), self.config.name.clone(), self.config.server.clone());
        self.send(Command::Join { input, name, server });
    }

    fn quality_changed(&mut self) {
        self.sync_thumbs(false);
        self.config.save();
        self.send(Command::SetQuality { quality: self.config.quality });
        self.render();
    }

    fn discord_login(&mut self) {
        let Some(server) = self.config.server.clone() else {
            self.notice = Some(("Entre num canal uma vez antes, para o app saber qual é o servidor.".into(), NoteTone::Warn));
            self.render();
            return;
        };
        self.notice = Some(("Termine o login no navegador.".into(), NoteTone::Info));
        self.render();
        tokio::spawn(async move {
            let r = crate::engine::gate::login(server).await;
            post(move |c| {
                match r {
                    Ok((session, name)) => {
                        c.config.discord_session = Some(session);
                        c.config.discord_name = Some(name);
                        c.config.save();
                        c.notice = None;
                    }
                    Err(e) => c.notice = Some((e, NoteTone::Error)),
                }
                c.render();
            });
        });
    }

    fn on_tray(&mut self, e: crate::tray::TrayEvent) {
        match e {
            crate::tray::TrayEvent::Show => self.show(),
            crate::tray::TrayEvent::StopLive => self.stop_live(),
            crate::tray::TrayEvent::Quit => self.quit(),
        }
    }

    fn on_engine(&mut self, e: Event) {
        match e {
            Event::Connecting => {
                self.session = Session::Connecting;
                self.spin_from = Instant::now();
                self.animate();
            }
            Event::Joined { code, invite, server } => {
                self.session = Session::Channel { code, invite, live: false };
                self.joining = false;
                self.config.server = Some(server);
                self.config.save();
                self.close_panel();
            }
            Event::Failed(msg) => {
                tracing::warn!("failed: {msg}");
                if self.session == Session::Connecting {
                    self.session = Session::Idle;
                }
                self.joining = false;
                self.tell("", &msg, NoteTone::Error);
            }
            Event::Viewers(v) => self.viewers = v,
            Event::Live(on) => {
                let started = on && matches!(self.session, Session::Channel { live: false, .. });
                if let Session::Channel { live, .. } = &mut self.session {
                    *live = on;
                }
                // Without the Discord bot announcing it, the invite is how
                // friends get in: it is ready to paste as soon as the stream starts.
                if started && !self.live_gated && self.copy_invite() {
                    self.tell("Convite copiado", "É só colar no chat da galera.", NoteTone::Info);
                }
                self.tally.set_target(if on { 1.0 } else { 0.0 });
                self.animate();
                if let Some(t) = &self.tray {
                    t.set_live(on);
                }
            }
            Event::Encoder(info) => self.encoder = Some(info),
            Event::Screen { width, height } => {
                self.screen = Some((width, height));
                self.sync_thumbs(true);
            }
            Event::Notice(msg) => self.tell("", &msg, NoteTone::Warn),
            Event::Info(msg) => self.tell("", &msg, NoteTone::Info),
            Event::Left => {
                self.session = Session::Idle;
                self.viewers.clear();
                self.encoder = None;
                self.tally.set_target(0.0);
                self.animate();
            }
        }
        self.render();
    }

    /* ---------------- qualidade ---------------- */

    fn native(&self) -> (u32, u32) {
        self.screen.or_else(|| self.monitor.map(|(w, h)| (w as u32, h as u32))).unwrap_or((1920, 1080))
    }

    fn resolution_options(&self) -> Vec<Resolution> {
        let (_, h) = self.native();
        let mut opts: Vec<Resolution> =
            [Resolution::P720, Resolution::P1080, Resolution::P1440].into_iter().filter(|r| r.height().is_some_and(|rh| rh < h)).collect();
        opts.push(Resolution::Original);
        opts
    }

    fn sync_thumbs(&mut self, snap: bool) {
        let q = self.config.quality;
        let opts = self.resolution_options();
        let res = opts.iter().position(|r| *r == q.resolution).unwrap_or(opts.len() - 1);
        let fps = FPS_OPTIONS.iter().position(|f| *f == q.fps).unwrap_or(1);
        let values = [res, fps, usize::from(q.priority == Priority::Nitidez), usize::from(q.system_audio), usize::from(self.config.call_only)];
        for (s, v) in self.thumbs.iter_mut().zip(values) {
            if snap { s.snap(v as f32) } else { s.set_target(v as f32) }
        }
        if !snap {
            self.animate();
        }
    }
}

fn initials(name: &str) -> String {
    let parts: Vec<&str> = name.split_whitespace().collect();
    let mut s: String = parts.first().and_then(|p| p.chars().next()).unwrap_or('?').to_uppercase().collect();
    if parts.len() > 1 {
        if let Some(l) = parts.last().and_then(|p| p.chars().next()) {
            s.extend(l.to_uppercase());
        }
    }
    s
}

fn name_hash(name: &str) -> usize {
    name.chars().fold(0u32, |h, c| h.wrapping_mul(31).wrapping_add(c as u32)) as usize
}

/// Looks for a new version a little after opening and then every 6 hours, and
/// says so when the app has just been updated.
fn watch_updates() {
    if std::env::var_os("TELINHA_ATUALIZADO").is_some() {
        slint::Timer::single_shot(Duration::from_millis(1200), || {
            with(|c| c.tell("Telinha atualizado", &format!("Agora na versão {}.", crate::update::CURRENT), NoteTone::Info));
        });
    }
    tokio::spawn(async {
        tokio::time::sleep(Duration::from_secs(8)).await;
        loop {
            match crate::update::check().await {
                Ok(Some(r)) => post(move |c| c.update_found(r)),
                Ok(None) => {}
                Err(e) => tracing::warn!("update check: {e}"),
            }
            tokio::time::sleep(Duration::from_secs(6 * 3600)).await;
        }
    });
}

/* ---------------- roteiro de fotos (TELINHA_SHOTS=pasta) ---------------- */

/// Walks through the main states with a fake engine and saves a screenshot of
/// each, to review the visuals without clicking.
fn shots(dir: PathBuf) {
    let _ = std::fs::create_dir_all(&dir);
    type Step = (&'static str, u64, fn(&mut Controller));
    let steps: Vec<Step> = vec![
        ("bolha", 500, |_| {}),
        ("menu", 700, |c| c.open_panel(Panel::Menu)),
        ("entrar", 700, |c| {
            c.join_text = "4821".into();
            c.open_panel(Panel::Join);
        }),
        // Two frames of the spinner (the fake engine connects in 900 ms).
        ("conectando", 200, |c| c.join_submit()),
        ("conectando-2", 250, |_| {}),
        ("no-canal", 1400, |_| {}),
        ("menu-canal", 700, |c| c.open_panel(Panel::Menu)),
        ("ao-vivo", 1200, |c| {
            let quality = c.config.quality;
            c.send(Command::StartLive { quality, discord: None });
            c.close_panel();
        }),
        // The invite toast (no Discord) closes on its own.
        ("aviso-sumiu", 4200, |_| {}),
        ("menu-ao-vivo", 700, |c| c.open_panel(Panel::Menu)),
        ("qualidade", 800, |c| c.open_panel(Panel::Quality)),
        ("qualidade-nitidez", 700, |c| {
            c.config.quality.priority = Priority::Nitidez;
            c.quality_changed();
        }),
        ("discord", 800, |c| c.open_panel(Panel::Discord)),
        ("atualizacao", 900, |c| {
            c.close_panel();
            c.update_found(crate::update::Release::sample("0.3.0"));
        }),
        ("menu-atualizacao", 800, |c| c.open_panel(Panel::Menu)),
        ("meio-da-troca", 90, |c| c.open_panel(Panel::Menu)),
        ("fechado", 900, |c| c.close_panel()),
        ("abrindo", 70, |c| c.open_panel(Panel::Menu)),
    ];
    let steps = Rc::new(steps);
    fn next(dir: Rc<PathBuf>, steps: Rc<Vec<Step>>, i: usize) {
        let Some(&(name, wait, act)) = steps.get(i) else {
            let _ = slint::quit_event_loop();
            return;
        };
        with(act);
        slint::Timer::single_shot(Duration::from_millis(wait), move || {
            with(|c| {
                if let Ok(img) = c.ui.window().take_snapshot() {
                    let (w, h) = (img.width(), img.height());
                    if let Some(png) = image::RgbaImage::from_raw(w, h, img.as_bytes().to_vec()) {
                        let _ = png.save(dir.join(format!("{i:02}-{name}.png")));
                    }
                }
            });
            next(dir, steps, i + 1);
        });
    }
    let dir = Rc::new(dir);
    slint::Timer::single_shot(Duration::from_millis(300), move || next(dir, steps, 0));
}
