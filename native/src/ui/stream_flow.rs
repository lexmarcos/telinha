//! What the single "Transmitir" button does and how far it got. Kept free of the
//! UI and the engine so every decision can be tested.
//!
//! Between the click and `Live(true)` the engine may be connecting or waiting on
//! the system's capture picker, with commands queuing behind it. The flow is what
//! keeps a second click from opening a second picker, and it remembers whether the
//! channel exists only because of this click, so a failure does not leave it behind.

/// Where the person is when the flow is asked to move.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Place {
    Outside,
    Connecting,
    Channel,
    Live,
}

/// What the app has to do next.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    /// Open a channel; the flow continues when it joins.
    Create,
    /// Open the capture and start streaming.
    Capture,
    /// Nothing: a start is already under way, or there is nothing to start.
    Wait,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum StreamFlow {
    #[default]
    Idle,
    /// The app's own picker is open: nothing has started, not even a channel.
    Choosing,
    /// The channel for this stream is being opened.
    Creating,
    /// The capture was asked for. `created`: the channel was opened by this flow.
    Starting { created: bool },
    /// The start failed in a channel of its own: waiting for the engine to leave it.
    Leaving,
}

impl StreamFlow {
    /// The button was clicked where the app asks what to share. `true`: ask now.
    pub fn choose(&mut self, place: Place) -> bool {
        let asks = *self == Self::Idle && matches!(place, Place::Outside | Place::Channel);
        if asks {
            *self = Self::Choosing;
        }
        asks
    }

    /// Something was picked: `begin` takes it from here. `false` if the flow is
    /// not waiting for that anymore (the channel it was in dropped, say).
    pub fn chosen(&mut self) -> bool {
        let waiting = *self == Self::Choosing;
        if waiting {
            *self = Self::Idle;
        }
        waiting
    }

    /// The picker closed without a pick. Nothing happened, so nothing to undo.
    pub fn cancel_choice(&mut self) {
        if *self == Self::Choosing {
            *self = Self::Idle;
        }
    }

    /// The button was clicked, or the channel it asked for has just joined.
    pub fn begin(&mut self, place: Place) -> Step {
        let (next, step) = match (*self, place) {
            (Self::Idle, Place::Outside) => (Self::Creating, Step::Create),
            (Self::Idle, Place::Channel) => (Self::Starting { created: false }, Step::Capture),
            (Self::Creating, Place::Channel) => (Self::Starting { created: true }, Step::Capture),
            (current, _) => (current, Step::Wait),
        };
        *self = next;
        step
    }

    /// A channel joined: does the stream go ahead on its own?
    pub fn awaiting_channel(self) -> bool {
        self == Self::Creating
    }

    /// The stream is on air: the channel now stays, even if the stream fails later.
    pub fn on_air(&mut self) {
        *self = Self::Idle;
    }

    /// The engine reported a failure, or the system's picker was cancelled.
    /// `true`: leave the channel this flow opened.
    pub fn failed(&mut self) -> bool {
        match *self {
            Self::Starting { created: true } => {
                *self = Self::Leaving;
                true
            }
            Self::Leaving => false,
            _ => {
                *self = Self::Idle;
                false
            }
        }
    }

    pub fn left(&mut self) {
        *self = Self::Idle;
    }

    /// Whether anything that depends on the engine's queue has to wait. A channel
    /// being joined counts too, since the engine only reads commands after it.
    pub fn busy(self, place: Place) -> bool {
        self != Self::Idle || place == Place::Connecting
    }

    /// The button's text.
    pub fn label(self, place: Place) -> &'static str {
        match self {
            Self::Starting { .. } => "Iniciando a transmissão…",
            Self::Choosing => "Transmitir",
            Self::Leaving => "Saindo do canal…",
            Self::Creating => "Conectando…",
            Self::Idle if place == Place::Connecting => "Conectando…",
            Self::Idle => "Transmitir",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Walks a flow to the point where the capture was asked for in its own channel.
    fn starting_in_own_channel() -> StreamFlow {
        let mut flow = StreamFlow::default();
        assert_eq!(flow.begin(Place::Outside), Step::Create);
        assert!(flow.awaiting_channel());
        assert_eq!(flow.begin(Place::Channel), Step::Capture);
        flow
    }

    #[test]
    fn clique_fora_de_canal_cria_e_transmite_ao_entrar() {
        let mut flow = starting_in_own_channel();
        assert_eq!(flow, StreamFlow::Starting { created: true });
        assert!(!flow.awaiting_channel());
        flow.on_air();
        assert_eq!(flow, StreamFlow::Idle);
    }

    #[test]
    fn clique_em_canal_transmite_nele() {
        let mut flow = StreamFlow::default();
        assert_eq!(flow.begin(Place::Channel), Step::Capture);
        assert_eq!(flow, StreamFlow::Starting { created: false });
        assert!(!flow.awaiting_channel());
    }

    #[test]
    fn entrar_num_canal_sem_clique_nao_transmite() {
        assert!(!StreamFlow::default().awaiting_channel());
    }

    #[test]
    fn cancelar_em_sala_do_fluxo_sai_dela() {
        let mut flow = starting_in_own_channel();
        assert!(flow.failed());
        assert_eq!(flow, StreamFlow::Leaving);
        assert!(flow.busy(Place::Channel), "the channel is still closing");
        flow.left();
        assert_eq!(flow, StreamFlow::Idle);
        assert!(!flow.busy(Place::Outside));
    }

    #[test]
    fn cancelar_em_sala_existente_continua_nela() {
        let mut flow = StreamFlow::default();
        flow.begin(Place::Channel);
        assert!(!flow.failed());
        assert_eq!(flow, StreamFlow::Idle);
        assert_eq!(flow.begin(Place::Channel), Step::Capture, "can try again");
    }

    #[test]
    fn falha_ao_conectar_nao_prende_o_botao() {
        let mut flow = StreamFlow::default();
        flow.begin(Place::Outside);
        assert!(!flow.failed(), "no channel to leave");
        assert!(!flow.busy(Place::Outside));
        assert_eq!(flow.begin(Place::Outside), Step::Create, "can try again");
    }

    #[test]
    fn falha_depois_de_no_ar_nao_sai_do_canal() {
        let mut flow = starting_in_own_channel();
        flow.on_air();
        assert!(!flow.failed());
    }

    #[test]
    fn segunda_falha_nao_pede_para_sair_de_novo() {
        let mut flow = starting_in_own_channel();
        assert!(flow.failed());
        assert!(!flow.failed());
        assert_eq!(flow, StreamFlow::Leaving);
    }

    #[test]
    fn clique_repetido_nao_faz_nada() {
        // Before Connecting arrives, and while it lasts.
        let mut flow = StreamFlow::default();
        flow.begin(Place::Outside);
        assert_eq!(flow.begin(Place::Outside), Step::Wait);
        assert_eq!(flow.begin(Place::Connecting), Step::Wait);
        assert_eq!(flow, StreamFlow::Creating);
        // With the capture picker open, created by the flow or not.
        let mut own = starting_in_own_channel();
        assert_eq!(own.begin(Place::Channel), Step::Wait);
        assert_eq!(own, StreamFlow::Starting { created: true });
        let mut existing = StreamFlow::default();
        existing.begin(Place::Channel);
        assert_eq!(existing.begin(Place::Channel), Step::Wait);
        assert_eq!(existing, StreamFlow::Starting { created: false });
        // While its channel closes.
        let mut leaving = StreamFlow::Leaving;
        assert_eq!(leaving.begin(Place::Channel), Step::Wait);
        assert_eq!(leaving.begin(Place::Outside), Step::Wait);
    }

    #[test]
    fn clique_sem_nada_a_iniciar_nao_faz_nada() {
        // Someone else's channel still connecting, and a stream already on air.
        let mut flow = StreamFlow::default();
        assert_eq!(flow.begin(Place::Connecting), Step::Wait);
        assert_eq!(flow.begin(Place::Live), Step::Wait);
        assert_eq!(flow, StreamFlow::Idle);
    }

    #[test]
    fn seletor_vem_antes_de_criar_a_sala() {
        let mut flow = StreamFlow::default();
        assert!(flow.choose(Place::Outside));
        assert_eq!(flow, StreamFlow::Choosing);
        assert_eq!(flow.begin(Place::Outside), Step::Wait, "no channel while choosing");
        assert!(flow.busy(Place::Outside));
        assert!(flow.chosen());
        assert_eq!(flow.begin(Place::Outside), Step::Create);
        // When the channel joins, straight to the capture: no second question.
        assert!(!flow.choose(Place::Channel));
        assert_eq!(flow.begin(Place::Channel), Step::Capture);
        assert_eq!(flow, StreamFlow::Starting { created: true });
    }

    #[test]
    fn cancelar_o_seletor_nao_deixa_nada_para_tras() {
        for place in [Place::Outside, Place::Channel] {
            let mut flow = StreamFlow::default();
            assert!(flow.choose(place));
            flow.cancel_choice();
            assert_eq!(flow, StreamFlow::Idle);
            assert!(!flow.failed(), "nothing to leave");
            assert_eq!(flow.label(place), "Transmitir");
            assert!(flow.choose(place), "can try again");
        }
    }

    #[test]
    fn seletor_em_sala_existente_nao_a_marca_como_criada() {
        let mut flow = StreamFlow::default();
        assert!(flow.choose(Place::Channel));
        assert!(flow.chosen());
        assert_eq!(flow.begin(Place::Channel), Step::Capture);
        assert_eq!(flow, StreamFlow::Starting { created: false });
        assert!(!flow.failed(), "the channel was already there");
    }

    #[test]
    fn nao_abre_o_seletor_com_algo_em_andamento() {
        for mut flow in [StreamFlow::Creating, StreamFlow::Starting { created: true }, StreamFlow::Leaving, StreamFlow::Choosing] {
            let before = flow;
            assert!(!flow.choose(Place::Channel), "{before:?}");
            assert_eq!(flow, before);
        }
        let mut flow = StreamFlow::default();
        assert!(!flow.choose(Place::Connecting));
        assert!(!flow.choose(Place::Live));
        assert_eq!(flow, StreamFlow::Idle);
    }

    #[test]
    fn escolha_tardia_nao_inicia_nada() {
        // The channel dropped while the picker was open.
        let mut flow = StreamFlow::default();
        flow.choose(Place::Channel);
        assert!(!flow.failed());
        assert!(!flow.chosen());
        // Cancelling a choice that is not open changes nothing.
        let mut starting = StreamFlow::Starting { created: true };
        starting.cancel_choice();
        assert_eq!(starting, StreamFlow::Starting { created: true });
    }

    #[test]
    fn sair_limpa_o_fluxo() {
        let mut flow = starting_in_own_channel();
        flow.left();
        assert_eq!(flow, StreamFlow::Idle);
    }

    #[test]
    fn o_botao_diz_em_que_passo_esta() {
        let mut flow = StreamFlow::default();
        assert_eq!((flow.label(Place::Outside), flow.busy(Place::Outside)), ("Transmitir", false));
        assert_eq!((flow.label(Place::Channel), flow.busy(Place::Channel)), ("Transmitir", false));
        // Joining someone's channel, with no click on the button.
        assert_eq!((flow.label(Place::Connecting), flow.busy(Place::Connecting)), ("Conectando…", true));
        flow.begin(Place::Outside);
        assert_eq!((flow.label(Place::Outside), flow.busy(Place::Outside)), ("Conectando…", true));
        flow.begin(Place::Channel);
        assert_eq!((flow.label(Place::Channel), flow.busy(Place::Channel)), ("Iniciando a transmissão…", true));
    }
}
