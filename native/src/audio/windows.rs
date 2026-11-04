//! Computer audio on Windows: WASAPI "process loopback" in exclusion mode
//! (Windows 10 2004+): all audio except Discord's when it is open (so people
//! in the call do not hear their own voice), otherwise all audio except
//! Telinha's own chimes. Only one process tree can be left out, so with
//! Discord open the capture goes silent while a chime plays. Discord is looked
//! up again every few seconds (on a thread of its own: a full process scan
//! would stall the 10 ms capture loop), so opening or restarting it
//! mid-stream works.
//!
//! Streaming a single window, the same API runs in inclusion mode on that window's
//! process tree. Nothing else may be heard then: if that fails the stream goes on
//! silent and says so (`plan`), instead of the default output as above.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc::SyncSender;

use wasapi::{AudioClient, DeviceEnumerator, Direction, SampleType, StreamMode, WaveFormat, initialize_mta};

use super::plan::{self, OnFailure, Plan};
use super::{CHANNELS, Problem, RATE};

/// Discord's main executables. Helpers (overlay, updater) also start with
/// "discord" but are not where the call's audio plays.
const DISCORD_MAIN: [&str; 4] = ["discord.exe", "discordptb.exe", "discordcanary.exe", "discorddevelopment.exe"];

/// Discord's root process: the main executable that is the ancestor of the
/// most Discord processes (the call's audio plays in one of its children).
fn discord_pid() -> Option<u32> {
    use sysinfo::{ProcessRefreshKind, RefreshKind, System};
    let sys = System::new_with_specifics(RefreshKind::nothing().with_processes(ProcessRefreshKind::nothing()));
    let parent = |pid: sysinfo::Pid| sys.process(pid).and_then(|p| p.parent());
    let discord: Vec<(sysinfo::Pid, String)> = sys
        .processes()
        .iter()
        .map(|(pid, p)| (*pid, p.name().to_string_lossy().to_lowercase()))
        .filter(|(_, name)| name.starts_with("discord") && name.ends_with(".exe"))
        .collect();
    let descendants = |root: sysinfo::Pid| {
        discord
            .iter()
            .filter(|(pid, _)| {
                // Walks up the parents (bounded, in case of a loop).
                let mut cur = parent(*pid);
                for _ in 0..16 {
                    match cur {
                        Some(p) if p == root => return true,
                        Some(p) => cur = parent(p),
                        None => return false,
                    }
                }
                false
            })
            .count()
    };
    discord
        .iter()
        .filter(|(_, name)| DISCORD_MAIN.contains(&name.as_str()))
        .max_by_key(|(pid, _)| descendants(*pid))
        .map(|(pid, _)| pid.as_u32())
}

pub fn start(tx: SyncSender<Vec<f32>>, stop: Arc<AtomicBool>, plan: Plan, problem: Problem) -> Result<String, String> {
    let source = match &plan {
        Plan::ProcessTree(_) => "só o som da janela",
        _ => "todo o som do computador menos o Discord",
    };
    std::thread::Builder::new()
        .name("telinha-wasapi".into())
        .spawn(move || {
            let result = match &plan {
                Plan::ProcessTree(pid) => run_tree(&tx, &stop, *pid),
                Plan::AllButDiscord => run(&tx, &stop, &plan),
                Plan::App { .. } => Err("picking an app by hand only exists on Linux".to_owned()),
            };
            if let Err(e) = result {
                tracing::error!("WASAPI audio: {e}");
                // Whichever way the capture failed, a window's stream stays silent.
                if let OnFailure::Silence(note) = plan::on_isolation_failure(&plan) {
                    *problem.lock().unwrap() = Some(note.to_owned());
                }
            }
        })
        .map_err(|e| e.to_string())?;
    Ok(source.to_owned())
}

/// How often Discord is looked up again: opened, restarted or updated after
/// the stream started, it must still be left out.
const RECHECK: std::time::Duration = std::time::Duration::from_secs(5);

/// Keeps Discord's root PID (0 = not open) up to date, from its own thread.
fn watch_discord(stop: Arc<AtomicBool>) -> Arc<AtomicU32> {
    let pid = Arc::new(AtomicU32::new(discord_pid().unwrap_or(0)));
    let p = pid.clone();
    let spawned = std::thread::Builder::new().name("telinha-discord".into()).spawn(move || {
        let mut last = std::time::Instant::now();
        while !stop.load(Ordering::Relaxed) {
            std::thread::sleep(std::time::Duration::from_millis(100));
            if last.elapsed() >= RECHECK {
                last = std::time::Instant::now();
                p.store(discord_pid().unwrap_or(0), Ordering::Relaxed);
            }
        }
    });
    if let Err(e) = spawned {
        tracing::warn!("Discord lookup: {e}");
    }
    pid
}

fn run(tx: &SyncSender<Vec<f32>>, stop: &Arc<AtomicBool>, plan: &Plan) -> Result<(), String> {
    let _boost = crate::realtime::boost(crate::realtime::Task::Audio);
    initialize_mta().ok().map_err(|e| format!("COM: {e:?}"))?;
    let discord = watch_discord(stop.clone());
    while !stop.load(Ordering::Relaxed) {
        let pid = discord.load(Ordering::Relaxed);
        capture(tx, stop, plan, (pid != 0).then_some(pid), &discord)?;
    }
    Ok(())
}

/// Captures only a process and its children, until the stream stops.
fn run_tree(tx: &SyncSender<Vec<f32>>, stop: &AtomicBool, pid: u32) -> Result<(), String> {
    let _boost = crate::realtime::boost(crate::realtime::Task::Audio);
    initialize_mta().ok().map_err(|e| format!("COM: {e:?}"))?;
    // `true` is PROCESS_LOOPBACK_MODE_INCLUDE_TARGET_PROCESS_TREE (`false`, used
    // below, excludes it). There is deliberately no fallback here.
    let client = AudioClient::new_application_loopback_client(pid, true).map_err(|e| e.to_string())?;
    tracing::info!(pid, "audio: only this window's process tree");
    pump(client, tx, stop, false, || false)
}

/// Captures until the stream stops or Discord's process changes.
fn capture(tx: &SyncSender<Vec<f32>>, stop: &AtomicBool, plan: &Plan, discord: Option<u32>, now: &AtomicU32) -> Result<(), String> {
    // Whether our own chimes reach the capture (then they are silenced).
    let mut hears_us = discord.is_some();
    let pid = discord.unwrap_or_else(std::process::id);
    let client = match AudioClient::new_application_loopback_client(pid, false) {
        Ok(c) => {
            match discord {
                Some(d) => tracing::info!(discord_pid = d, "audio: everything except Discord's process tree"),
                None => tracing::info!("audio: everything (Discord is not open)"),
            }
            c
        }
        Err(e) => match plan::on_isolation_failure(plan) {
            OnFailure::WholeOutput => {
                tracing::warn!("no process exclusion (old Windows?), Discord will be heard: {e}");
                hears_us = true;
                default_loopback()?
            }
            OnFailure::Silence(_) => return Err(e.to_string()),
        },
    };
    let previous = discord.unwrap_or(0);
    pump(client, tx, stop, hears_us, || {
        let current = now.load(Ordering::Relaxed);
        if current != previous {
            tracing::info!(before = ?discord, now = current, "Discord changed; restarting the audio capture");
        }
        current != previous
    })
}

/// Reads from an activated client until the stream stops or `restart` says so.
fn pump(mut client: AudioClient, tx: &SyncSender<Vec<f32>>, stop: &AtomicBool, hears_us: bool, restart: impl Fn() -> bool) -> Result<(), String> {
    let format = WaveFormat::new(32, 32, &SampleType::Float, RATE as usize, CHANNELS, None);
    let mode = StreamMode::EventsShared { autoconvert: true, buffer_duration_hns: 100_000 };
    client.initialize_client(&format, &Direction::Capture, &mode).map_err(|e| e.to_string())?;
    let event = client.set_get_eventhandle().map_err(|e| e.to_string())?;
    let capture = client.get_audiocaptureclient().map_err(|e| e.to_string())?;
    client.start_stream().map_err(|e| e.to_string())?;

    while !stop.load(Ordering::Relaxed) {
        // Every packet that is ready: reading one per wake-up would leave the
        // rest behind as permanent delay.
        while let Some(frames) = capture.get_next_packet_size().map_err(|e| e.to_string())?.filter(|&n| n > 0) {
            let mut samples = vec![0f32; frames as usize * CHANNELS];
            // SAFETY: any 4 bytes are a valid f32, and the buffer is exactly
            // `frames` of 32-bit float stereo, the format asked for above.
            let bytes = unsafe { std::slice::from_raw_parts_mut(samples.as_mut_ptr().cast::<u8>(), samples.len() * 4) };
            let (read, info) = capture.read_from_device(bytes).map_err(|e| e.to_string())?;
            samples.truncate(read as usize * CHANNELS);
            if info.flags.silent || (hears_us && super::chime::muting()) {
                samples.fill(0.0);
            }
            if !samples.is_empty() {
                let _ = tx.try_send(samples);
            }
        }
        if restart() {
            break;
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

/// Plays a short sound (interleaved stereo at `RATE`) on the default output, in the background.
pub fn play(samples: Vec<f32>) {
    let spawned = std::thread::Builder::new().name("telinha-aviso".into()).spawn(move || {
        if let Err(e) = play_now(&samples) {
            tracing::warn!("chime: {e}");
        }
    });
    if let Err(e) = spawned {
        tracing::warn!("chime: {e}");
    }
}

fn play_now(samples: &[f32]) -> Result<(), String> {
    initialize_mta().ok().map_err(|e| format!("COM: {e:?}"))?;
    let device = DeviceEnumerator::new().map_err(|e| e.to_string())?.get_default_device(&Direction::Render).map_err(|e| e.to_string())?;
    let mut client = device.get_iaudioclient().map_err(|e| e.to_string())?;
    let format = WaveFormat::new(32, 32, &SampleType::Float, RATE as usize, CHANNELS, None);
    let mode = StreamMode::EventsShared { autoconvert: true, buffer_duration_hns: 200_000 };
    client.initialize_client(&format, &Direction::Render, &mode).map_err(|e| e.to_string())?;
    let event = client.set_get_eventhandle().map_err(|e| e.to_string())?;
    let render = client.get_audiorenderclient().map_err(|e| e.to_string())?;
    let total = client.get_buffer_size().map_err(|e| e.to_string())? as usize;

    // After the sound, silence until the buffer has played out.
    let mut pos = 0;
    let write = |pos: &mut usize, frames: usize| -> Result<(), String> {
        let bytes: Vec<u8> = (0..frames * CHANNELS).flat_map(|i| samples.get(*pos + i).copied().unwrap_or(0.0).to_le_bytes()).collect();
        *pos += frames * CHANNELS;
        render.write_to_device(frames, &bytes, None).map_err(|e| e.to_string())
    };
    write(&mut pos, client.get_available_space_in_frames().map_err(|e| e.to_string())? as usize)?;
    client.start_stream().map_err(|e| e.to_string())?;
    while pos < samples.len() + total * CHANNELS {
        if event.wait_for_event(500).is_err() {
            break;
        }
        write(&mut pos, client.get_available_space_in_frames().map_err(|e| e.to_string())? as usize)?;
    }
    let _ = client.stop_stream();
    Ok(())
}
