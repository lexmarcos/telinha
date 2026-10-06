//! Molas do design system (DESIGN.md, "Movimento"). As cores, tamanhos e
//! tempos curtos ficam em ui/tokens.slint; as molas ficam aqui porque quem as
//! anima é o app, quadro a quadro.

/// Molas no estilo Apple: amortecimento e resposta (segundos).
#[derive(Debug, Clone, Copy)]
pub struct SpringParams {
    pub damping: f32,
    pub response: f32,
}

/// Painel abrindo e fechando, troca de painel, marcador do segmentado.
pub const UI: SpringParams = SpringParams { damping: 1.0, response: 0.32 };
/// Encolher no aperto e voltar.
pub const PRESS: SpringParams = SpringParams { damping: 1.0, response: 0.12 };
/// O anel vermelho chegando quando a transmissão começa (o único quique).
pub const TALLY: SpringParams = SpringParams { damping: 0.7, response: 0.45 };
/// Conteúdo novo aparecendo quando o painel troca.
pub const FADE: SpringParams = SpringParams { damping: 1.0, response: 0.22 };
