//! Molas interrompíveis: mudar o alvo no meio do caminho continua do valor e
//! da velocidade atuais, nunca pula (DESIGN.md, "Movimento").

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use super::motion::SpringParams;

static REDUCED_MOTION: AtomicBool = AtomicBool::new(false);

pub fn reduced_motion() -> bool {
    REDUCED_MOTION.load(Ordering::Relaxed)
}

/// Lê a preferência do sistema uma vez, na abertura do app.
pub fn detect_reduced_motion() {
    let reduced = system_reduced_motion();
    REDUCED_MOTION.store(reduced, Ordering::Relaxed);
}

#[cfg(target_os = "linux")]
fn system_reduced_motion() -> bool {
    std::process::Command::new("gsettings")
        .args(["get", "org.gnome.desktop.interface", "enable-animations"])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim() == "false")
        .unwrap_or(false)
}

#[cfg(not(target_os = "linux"))]
fn system_reduced_motion() -> bool {
    false
}

#[derive(Debug, Clone, Copy)]
pub struct Spring {
    value: f32,
    velocity: f32,
    target: f32,
    params: SpringParams,
    last: Option<Instant>,
}

impl Spring {
    pub fn new(value: f32, params: SpringParams) -> Self {
        Self { value, velocity: 0.0, target: value, params, last: None }
    }

    pub fn value(&self) -> f32 {
        self.value
    }

    pub fn target(&self) -> f32 {
        self.target
    }

    pub fn set_target(&mut self, target: f32) {
        if (target - self.target).abs() < f32::EPSILON {
            return;
        }
        self.target = target;
        self.last = None;
        if reduced_motion() {
            self.value = target;
            self.velocity = 0.0;
        }
    }

    /// Pula direto pro alvo (estado inicial, sem animação).
    pub fn snap(&mut self, value: f32) {
        self.value = value;
        self.target = value;
        self.velocity = 0.0;
        self.last = None;
    }

    pub fn is_moving(&self) -> bool {
        (self.value - self.target).abs() > 0.0005 || self.velocity.abs() > 0.002
    }

    /// Avança até `now`. Integra em passos de 1 ms (estável para qualquer mola).
    pub fn tick(&mut self, now: Instant) {
        if !self.is_moving() {
            self.value = self.target;
            self.velocity = 0.0;
            self.last = None;
            return;
        }
        let Some(last) = self.last.replace(now) else { return };
        let mut dt = now.saturating_duration_since(last).as_secs_f32().min(0.064);
        let omega = std::f32::consts::TAU / self.params.response;
        let stiffness = omega * omega;
        let damping = 2.0 * self.params.damping * omega;
        while dt > 0.0 {
            let h = dt.min(0.001);
            let accel = -stiffness * (self.value - self.target) - damping * self.velocity;
            self.velocity += accel * h;
            self.value += self.velocity * h;
            dt -= h;
        }
    }
}
