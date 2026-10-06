//! Ícone na bandeja: a bolha some nele e volta por ele.
//!
//! Linux: StatusNotifierItem pelo D-Bus (ksni), sem GTK.
//! Windows: tray-icon, criado na thread principal (a do laço de mensagens).

use std::sync::Arc;

/// Quem recebe os cliques na bandeja (chamado fora da thread da interface).
pub type OnEvent = Arc<dyn Fn(TrayEvent) + Send + Sync>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrayEvent {
    Show,
    StopLive,
    Quit,
}

const ICON_PNG: &[u8] = include_bytes!("../assets/icons/tray.png");

fn icon_rgba() -> (Vec<u8>, u32, u32) {
    let img = image::load_from_memory(ICON_PNG).expect("ícone da bandeja").to_rgba8();
    let (w, h) = img.dimensions();
    (img.into_raw(), w, h)
}

pub use platform::{Guard, init};

#[cfg(target_os = "linux")]
mod platform {
    use super::*;
    use ksni::TrayMethods;
    use ksni::menu::{MenuItem, StandardItem};

    struct Tray {
        on: OnEvent,
        live: bool,
    }

    impl ksni::Tray for Tray {
        fn id(&self) -> String {
            "telinha".into()
        }
        fn title(&self) -> String {
            if self.live { "Telinha · transmitindo".into() } else { "Telinha".into() }
        }
        fn icon_pixmap(&self) -> Vec<ksni::Icon> {
            let (rgba, w, h) = icon_rgba();
            // ksni quer ARGB32 em ordem de rede.
            let data = rgba.chunks_exact(4).flat_map(|p| [p[3], p[0], p[1], p[2]]).collect();
            vec![ksni::Icon { width: w as i32, height: h as i32, data }]
        }
        fn activate(&mut self, _x: i32, _y: i32) {
            (self.on)(TrayEvent::Show);
        }
        fn menu(&self) -> Vec<MenuItem<Self>> {
            vec![
                StandardItem { label: "Mostrar a bolha".into(), activate: Box::new(|t: &mut Self| (t.on)(TrayEvent::Show)), ..Default::default() }.into(),
                StandardItem {
                    label: "Parar de transmitir".into(),
                    enabled: self.live,
                    activate: Box::new(|t: &mut Self| (t.on)(TrayEvent::StopLive)),
                    ..Default::default()
                }
                .into(),
                MenuItem::Separator,
                StandardItem { label: "Fechar o Telinha".into(), activate: Box::new(|t: &mut Self| (t.on)(TrayEvent::Quit)), ..Default::default() }.into(),
            ]
        }
    }

    /// O ícone vive no tokio (D-Bus); o guarda só repassa mudanças.
    pub struct Guard(Arc<tokio::sync::OnceCell<ksni::Handle<Tray>>>);

    impl Guard {
        pub fn set_live(&self, live: bool) {
            let cell = self.0.clone();
            tokio::spawn(async move {
                if let Some(h) = cell.get() {
                    h.update(|t| t.live = live).await;
                }
            });
        }
    }

    /// Precisa ser chamado dentro do tokio.
    pub fn init(on: OnEvent) -> Option<Guard> {
        let cell = Arc::new(tokio::sync::OnceCell::new());
        let c = cell.clone();
        tokio::spawn(async move {
            match (Tray { on, live: false }).spawn().await {
                Ok(handle) => {
                    let _ = c.set(handle);
                }
                Err(e) => tracing::warn!("sem bandeja do sistema: {e}"),
            }
        });
        Some(Guard(cell))
    }
}

#[cfg(target_os = "windows")]
mod platform {
    use super::*;
    use tray_icon::menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem};
    use tray_icon::{MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};

    pub struct Guard {
        _icon: TrayIcon,
        stop: MenuItem,
    }

    impl Guard {
        pub fn set_live(&self, live: bool) {
            self.stop.set_enabled(live);
        }
    }

    /// Precisa ser chamado na thread principal (a do laço de mensagens).
    pub fn init(on: OnEvent) -> Option<Guard> {
        let (rgba, w, h) = icon_rgba();
        let show = MenuItem::with_id("show", "Mostrar a bolha", true, None);
        let stop = MenuItem::with_id("stop", "Parar de transmitir", false, None);
        let quit = MenuItem::with_id("quit", "Fechar o Telinha", true, None);
        let menu = Menu::new();
        let _ = menu.append_items(&[&show, &stop, &PredefinedMenuItem::separator(), &quit]);
        let icon = TrayIconBuilder::new()
            .with_tooltip("Telinha")
            .with_icon(tray_icon::Icon::from_rgba(rgba, w, h).ok()?)
            .with_menu(Box::new(menu))
            .with_menu_on_left_click(false)
            .build()
            .map_err(|e| tracing::warn!("sem bandeja do sistema: {e}"))
            .ok()?;
        // Os receptores do tray-icon são bloqueantes: ficam numa thread própria.
        std::thread::spawn(move || {
            loop {
                if let Ok(e) = MenuEvent::receiver().recv_timeout(std::time::Duration::from_millis(100)) {
                    match e.id.as_ref() {
                        "show" => on(TrayEvent::Show),
                        "stop" => on(TrayEvent::StopLive),
                        "quit" => on(TrayEvent::Quit),
                        _ => {}
                    }
                }
                while let Ok(e) = TrayIconEvent::receiver().try_recv() {
                    if let TrayIconEvent::Click { button: MouseButton::Left, button_state: MouseButtonState::Up, .. } = e {
                        on(TrayEvent::Show);
                    }
                }
            }
        });
        Some(Guard { _icon: icon, stop })
    }
}
