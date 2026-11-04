//! What the app asks before and during a stream of a single thing: which monitor
//! or window (Windows, where the app has to ask, since the system has no picker
//! of its own to offer) and which app to hear (Linux, where the system's picker
//! says it was a window but not whose). `begin_stream` and `on_engine` call in here.

use slint::{ModelRc, VecModel};

use super::*;
use crate::capture::target::{CaptureTarget, SourceList};
use crate::engine::AudioApp;

/// The app's own picker exists where the system does not provide one.
/// TELINHA_MOCK_SELETOR=1 shows it anywhere, with the fake engine's sources.
fn has_own_picker() -> bool {
    cfg!(target_os = "windows") || std::env::var_os("TELINHA_MOCK_SELETOR").is_some()
}

#[derive(Default)]
pub(super) struct Picks {
    /// What the current "Transmitir" click is going to capture.
    pub target: CaptureTarget,
    sources: SourceList,
    /// The stream is of a window (what notes about its size say).
    pub window: bool,
    /// Who can be heard: `Some` only while a window's sound is up to the person.
    apps: Option<Vec<AudioApp>>,
    selected: Option<String>,
}

impl Picks {
    /// The screenshot script's way of showing a list the system would not give here.
    pub fn set_sources_for_shot(&mut self, sources: SourceList) {
        self.sources = sources;
    }
}

impl Controller {
    /// Asks what to share, if this click still has to. `true` when that is the
    /// answer to the click for now: the stream goes on when something is picked.
    /// Before any channel exists, so that giving up leaves nothing behind.
    pub(super) fn ask_source(&mut self) -> bool {
        if !has_own_picker() || !self.flow.choose(self.stream_place()) {
            return false;
        }
        self.show_sources();
        true
    }

    /// The picker, with the list as it is now.
    pub(super) fn show_sources(&mut self) {
        self.picks.sources = crate::capture::sources();
        self.open_panel(Panel::Source);
    }

    /// Something was picked. It may be gone already, and then this is the place
    /// to say so: the person is looking at the list, and no channel opened yet.
    pub(super) fn pick_source(&mut self, index: usize) {
        let s = &self.picks.sources;
        let target = s.monitors.get(index).map(|m| m.target.clone()).or_else(|| s.windows.get(index.wrapping_sub(s.monitors.len())).map(|w| w.target.clone()));
        let Some(target) = target else { return };
        if let Err(why) = crate::capture::still_there(&target) {
            self.picks.sources = crate::capture::sources();
            self.notice = Some((why, NoteTone::Warn));
            self.render();
            return;
        }
        if !self.flow.chosen() {
            return;
        }
        self.picks.target = target;
        self.begin_stream();
    }

    /// The person left the picker without picking. Nothing was started.
    pub(super) fn cancel_source(&mut self) {
        self.flow.cancel_choice();
    }

    /// The engine says who can be heard.
    pub(super) fn audio_apps(&mut self, apps: Vec<AudioApp>, selected: Option<String>, ask: bool) {
        tracing::info!(apps = apps.len(), ?selected, ask, "who can be heard");
        self.picks.apps = Some(apps);
        self.picks.selected = selected;
        if ask {
            self.open_panel(Panel::AudioApp);
        }
    }

    pub(super) fn pick_audio_app(&mut self, index: i32) {
        let app = self.picks.apps.as_ref().and_then(|a| usize::try_from(index).ok().and_then(|i| a.get(i))).map(|a| a.id.clone());
        self.send(Command::SetAudioApp { app });
    }

    /// The stream ended: nothing left to choose.
    pub(super) fn forget_stream_picks(&mut self) {
        self.picks.apps = None;
        self.picks.selected = None;
        self.picks.window = false;
        self.picks.target = CaptureTarget::Default;
        if self.panel == Some(Panel::AudioApp) {
            self.close_panel();
        }
    }

    pub(super) fn render_picks(&self) {
        let st = self.ui.global::<AppState>();
        let row = |label: &str, chosen: bool| PickRow { label: label.into(), chosen };
        let rows = |v: Vec<PickRow>| ModelRc::new(VecModel::from(v));
        let s = &self.picks.sources;
        st.set_source_monitors(rows(s.monitors.iter().map(|m| row(&m.label, false)).collect()));
        st.set_source_windows(rows(s.windows.iter().map(|w| row(&w.title, false)).collect()));
        st.set_source_note(s.no_windows.clone().unwrap_or_default().into());
        let selected = self.picks.selected.as_deref();
        let apps = self.picks.apps.as_deref().unwrap_or_default();
        st.set_audio_apps(rows(apps.iter().map(|a| row(&a.label, selected == Some(a.id.as_str()))).collect()));
        st.set_audio_none(selected.is_none());
        st.set_audio_label(self.audio_label().into());
    }

    /// The menu line that leads back to the choice. Empty when there is none.
    fn audio_label(&self) -> String {
        let Some(apps) = &self.picks.apps else { return String::new() };
        if !self.config.quality.system_audio {
            return String::new();
        }
        match &self.picks.selected {
            None => "Escolher o som".into(),
            Some(id) => format!("Som: {}", apps.iter().find(|a| a.id == *id).map_or(id.as_str(), |a| a.label.as_str())),
        }
    }
}
