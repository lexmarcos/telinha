//! Computer audio on Windows: WASAPI loopback. With Discord open, it uses
//! "process loopback" in exclusion mode (Windows 10 2004+): it takes all
//! audio except Discord's, so people in the call do not hear their own voice.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::SyncSender;

use wasapi::{AudioClient, DeviceEnumerator, Direction, SampleType, StreamMode, WaveFormat, initialize_mta};

use super::{CHANNELS, RATE};

/// Discord's root process (the parent of the other instances), if it is open.
fn discord_pid() -> Option<u32> {
    use sysinfo::{ProcessRefreshKind, RefreshKind, System};
    let sys = System::new_with_specifics(RefreshKind::nothing().with_processes(ProcessRefreshKind::nothing()));
    let mut ids = Vec::new();
    for (pid, p) in sys.processes() {
        let name = p.name().to_string_lossy().to_lowercase();
        if name.starts_with("discord") && name.ends_with(".exe") {
            ids.push((pid.as_u32(), p.parent().map(|x| x.as_u32())));
        }
    }
    ids.iter().find(|(_, parent)| parent.is_none_or(|pp| !ids.iter().any(|(id, _)| *id == pp))).map(|(id, _)| *id)
}

pub fn start(tx: SyncSender<Vec<f32>>, stop: Arc<AtomicBool>) -> Result<String, String> {
    let discord = discord_pid();
    let source = match discord {
        Some(_) => "todo o som do computador menos o Discord".to_owned(),
        None => "todo o som do computador".to_owned(),
    };
    std::thread::Builder::new()
        .name("telinha-wasapi".into())
        .spawn(move || {
            if let Err(e) = run(tx, stop, discord) {
                tracing::error!("WASAPI audio: {e}");
            }
        })
        .map_err(|e| e.to_string())?;
    Ok(source)
}

fn run(tx: SyncSender<Vec<f32>>, stop: Arc<AtomicBool>, exclude: Option<u32>) -> Result<(), String> {
    initialize_mta().ok().map_err(|e| format!("COM: {e:?}"))?;
    let format = WaveFormat::new(32, 32, &SampleType::Float, RATE as usize, CHANNELS, None);

    let mut client = match exclude {
        Some(pid) => match AudioClient::new_application_loopback_client(pid, false) {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!("no Discord exclusion (old Windows?): {e}");
                default_loopback()?
            }
        },
        None => default_loopback()?,
    };
    let mode = StreamMode::EventsShared { autoconvert: true, buffer_duration_hns: 100_000 };
    client.initialize_client(&format, &Direction::Capture, &mode).map_err(|e| e.to_string())?;
    let event = client.set_get_eventhandle().map_err(|e| e.to_string())?;
    let capture = client.get_audiocaptureclient().map_err(|e| e.to_string())?;
    client.start_stream().map_err(|e| e.to_string())?;

    let mut queue = std::collections::VecDeque::new();
    while !stop.load(Ordering::Relaxed) {
        capture.read_from_device_to_deque(&mut queue).map_err(|e| e.to_string())?;
        if !queue.is_empty() {
            let bytes: Vec<u8> = queue.drain(..queue.len() / 4 * 4).collect();
            let samples = bytes.chunks_exact(4).map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]])).collect();
            let _ = tx.try_send(samples);
        }
        if event.wait_for_event(200).is_err() {
            // With no audio playing the event does not fire; keep waiting.
            continue;
        }
    }
    let _ = client.stop_stream();
    Ok(())
}

/// Plain loopback: capture the default output as if it were an input.
fn default_loopback() -> Result<AudioClient, String> {
    let device = DeviceEnumerator::new().map_err(|e| e.to_string())?.get_default_device(&Direction::Render).map_err(|e| e.to_string())?;
    device.get_iaudioclient().map_err(|e| e.to_string())
}
