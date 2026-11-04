//! What sound a stream carries, decided in one place so it can be tested.
//!
//! The rule that matters: **a stream of a window never sends the whole computer's
//! sound.** Someone sharing one window is counting on the rest staying private (a
//! call, a notification, another tab). So when the window's sound cannot be isolated
//! the answer is silence and a warning, never the old fallbacks (the default output
//! on Windows when process loopback fails, `TELINHA_SOM_TUDO` on Linux). Those only
//! exist for a whole monitor, which shows everything anyway.

// Each system only uses its side of this; all of it is testable on any.
#![allow(dead_code)]

use std::sync::{Arc, Mutex};

use crate::capture::target::{CaptureKind, WindowSound};

/// Said when a window's sound cannot be separated from the rest.
pub const CANNOT_ISOLATE: &str = "Não deu para separar o som dessa janela. A transmissão segue sem som.";
const DISCORD_WINDOW: &str = "O som do Discord não entra na transmissão: quem está na call ouviria a própria voz. A transmissão segue sem som.";

/// An app that is playing sound now (Linux), as the person sees it in the panel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AudioApp {
    /// The identity a choice is kept under (see `identity`).
    pub id: String,
    pub label: String,
}

/// Called with the current list whenever it changes.
pub type AppList = Arc<dyn Fn(Vec<AudioApp>) + Send + Sync>;

/// Which app the person chose to hear. Starts empty: until they choose, no sound.
/// Shared with the capture, which follows it while the stream runs.
#[derive(Clone, Default)]
pub struct Selection(Arc<Mutex<Option<String>>>);

impl Selection {
    pub fn set(&self, app: Option<String>) {
        *self.0.lock().unwrap() = app;
    }

    pub fn get(&self) -> Option<String> {
        self.0.lock().unwrap().clone()
    }
}

/// How the sound is captured.
#[derive(Clone)]
pub enum Plan {
    /// A whole monitor: everything but Discord.
    AllButDiscord,
    /// One process and its children (Windows).
    ProcessTree(u32),
    /// The apps with the chosen identity (Linux), and where to report who is playing.
    App { selection: Selection, list: Option<AppList> },
}

impl Plan {
    /// The only plan that may fall back to the whole output.
    pub fn may_use_whole_output(&self) -> bool {
        matches!(self, Self::AllButDiscord)
    }
}

/// What to do with the sound of a capture.
#[derive(Debug, PartialEq, Eq)]
pub enum Decision {
    /// "Sem som" in the quality panel: above everything else.
    Off,
    AllButDiscord,
    ProcessTree(u32),
    AskApp,
    /// No sound, and the person has to be told why.
    Silent(&'static str),
}

pub fn decide(sound_on: bool, kind: &CaptureKind) -> Decision {
    if !sound_on {
        return Decision::Off;
    }
    match kind {
        CaptureKind::Monitor => Decision::AllButDiscord,
        CaptureKind::Window { sound, .. } => match sound {
            WindowSound::Process(pid) => Decision::ProcessTree(*pid),
            WindowSound::AskApp => Decision::AskApp,
            WindowSound::Discord => Decision::Silent(DISCORD_WINDOW),
            WindowSound::Unknown => Decision::Silent(CANNOT_ISOLATE),
        },
    }
}

/// `TELINHA_SOM_TUDO` (Linux, old way of recording the whole output) counts only
/// where the whole output is allowed.
pub fn whole_output(asked: bool, plan: &Plan) -> bool {
    asked && plan.may_use_whole_output()
}

/// What to do when the capture cannot be isolated as planned.
#[derive(Debug, PartialEq, Eq)]
pub enum OnFailure {
    /// Record the whole output (Discord will be heard: old Windows only).
    WholeOutput,
    /// Stay silent and say so.
    Silence(&'static str),
}

pub fn on_isolation_failure(plan: &Plan) -> OnFailure {
    if plan.may_use_whole_output() { OnFailure::WholeOutput } else { OnFailure::Silence(CANNOT_ISOLATE) }
}

/* ---------------- Which app is which (Linux) ---------------- */

/// Binaries that run other people's programs: the program's name says more.
const GENERIC_BINARIES: [&str; 9] = ["electron", "python", "python3", "java", "node", "wine", "wine64-preloader", "wine-preloader", "bwrap"];

/// What a choice is kept under, and how it is shown. Not a node id or PID:
/// browsers play from a child process, and their streams end and come back with
/// every song. `get` reads a PipeWire property.
pub fn identity(get: impl Fn(&str) -> Option<String>) -> AudioApp {
    let name = get("application.name").filter(|n| !n.is_empty());
    let binary = get("application.process.binary").filter(|b| !b.is_empty());
    let fallback = || get("node.name").unwrap_or_else(|| "?".to_owned());
    let id = match (&binary, &name) {
        (Some(b), Some(n)) if GENERIC_BINARIES.contains(&b.to_lowercase().as_str()) => n.clone(),
        (Some(b), _) => b.clone(),
        (None, Some(n)) => n.clone(),
        (None, None) => fallback(),
    };
    let label = name.or(binary).unwrap_or_else(fallback);
    AudioApp { id, label }
}

/// The list the panel shows: each app once, by name, keeping a chosen app that
/// is silent right now so the choice stays visible.
pub fn app_list(playing: impl IntoIterator<Item = AudioApp>) -> Vec<AudioApp> {
    let mut list: Vec<AudioApp> = Vec::new();
    for app in playing {
        if !list.iter().any(|a| a.id == app.id) {
            list.push(app);
        }
    }
    list.sort_by(|a, b| a.label.to_lowercase().cmp(&b.label.to_lowercase()).then_with(|| a.id.cmp(&b.id)));
    list
}

#[cfg(test)]
mod tests {
    use super::*;

    fn janela(sound: WindowSound) -> CaptureKind {
        CaptureKind::Window { title: None, sound }
    }

    #[test]
    fn sem_som_vale_acima_de_tudo() {
        for kind in [CaptureKind::Monitor, janela(WindowSound::Process(7)), janela(WindowSound::AskApp), janela(WindowSound::Unknown), janela(WindowSound::Discord)] {
            assert_eq!(decide(false, &kind), Decision::Off, "{kind:?}");
        }
    }

    #[test]
    fn monitor_inteiro_continua_como_antes() {
        assert_eq!(decide(true, &CaptureKind::Monitor), Decision::AllButDiscord);
    }

    #[test]
    fn janela_nunca_decide_pelo_som_do_computador() {
        for sound in [WindowSound::Process(7), WindowSound::AskApp, WindowSound::Discord, WindowSound::Unknown] {
            assert_ne!(decide(true, &janela(sound.clone())), Decision::AllButDiscord, "{sound:?}");
        }
        assert_eq!(decide(true, &janela(WindowSound::Process(7))), Decision::ProcessTree(7));
        assert_eq!(decide(true, &janela(WindowSound::AskApp)), Decision::AskApp);
        assert_eq!(decide(true, &janela(WindowSound::Unknown)), Decision::Silent(CANNOT_ISOLATE));
        assert!(matches!(decide(true, &janela(WindowSound::Discord)), Decision::Silent(_)));
    }

    #[test]
    fn janela_que_nao_isola_fica_em_silencio() {
        // The old fallback only exists for a whole monitor.
        assert_eq!(on_isolation_failure(&Plan::AllButDiscord), OnFailure::WholeOutput);
        assert_eq!(on_isolation_failure(&Plan::ProcessTree(7)), OnFailure::Silence(CANNOT_ISOLATE));
        assert_eq!(on_isolation_failure(&Plan::App { selection: Selection::default(), list: None }), OnFailure::Silence(CANNOT_ISOLATE));
    }

    #[test]
    fn som_tudo_nao_vale_para_janela() {
        assert!(whole_output(true, &Plan::AllButDiscord));
        assert!(!whole_output(false, &Plan::AllButDiscord));
        assert!(!whole_output(true, &Plan::ProcessTree(7)));
        assert!(!whole_output(true, &Plan::App { selection: Selection::default(), list: None }));
    }

    fn app(props: &[(&str, &str)]) -> AudioApp {
        identity(|k| props.iter().find(|(p, _)| *p == k).map(|(_, v)| (*v).to_owned()))
    }

    #[test]
    fn identidade_nao_depende_de_no_nem_de_processo() {
        // Two tabs, two child processes, same app.
        let a = app(&[("application.process.binary", "firefox"), ("application.name", "Firefox"), ("application.process.id", "100"), ("node.name", "Firefox")]);
        let b = app(&[("application.process.binary", "firefox"), ("application.name", "Firefox"), ("application.process.id", "200"), ("node.name", "Firefox-2")]);
        assert_eq!(a, b);
        assert_eq!(a, AudioApp { id: "firefox".into(), label: "Firefox".into() });
    }

    #[test]
    fn identidade_sem_binario_usa_o_nome() {
        assert_eq!(app(&[("application.name", "Spotify")]).id, "Spotify");
        assert_eq!(app(&[("node.name", "alsa_playback.mpg123")]).id, "alsa_playback.mpg123");
    }

    #[test]
    fn binario_generico_nao_junta_apps_diferentes() {
        let spotify = app(&[("application.process.binary", "electron"), ("application.name", "Spotify")]);
        let slack = app(&[("application.process.binary", "electron"), ("application.name", "Slack")]);
        assert_ne!(spotify.id, slack.id);
    }

    #[test]
    fn lista_sem_repetir_e_em_ordem() {
        let a = |id: &str, label: &str| AudioApp { id: id.into(), label: label.into() };
        let list = app_list([a("mpv", "mpv"), a("firefox", "Firefox"), a("firefox", "Firefox"), a("chrome", "Chrome")]);
        assert_eq!(list.iter().map(|x| x.id.as_str()).collect::<Vec<_>>(), ["chrome", "firefox", "mpv"]);
    }

    #[test]
    fn escolha_vale_para_quem_compartilha_a_selecao() {
        let s = Selection::default();
        assert_eq!(s.get(), None, "nothing chosen: no sound");
        let other = s.clone();
        other.set(Some("firefox".into()));
        assert_eq!(s.get().as_deref(), Some("firefox"));
    }
}
