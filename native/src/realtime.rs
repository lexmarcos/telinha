//! Scheduling for the latency-critical work while streaming.
//!
//! Windows: the process a notch above normal, a 1 ms system timer (FFmpeg's
//! ddagrab paces the capture with Sleep, which otherwise ticks every 15.6 ms
//! unless some other app raised it), no power throttling (Windows 11 would
//! otherwise ignore the timer request while the bubble is hidden), and MMCSS
//! for the video and audio threads, as games and audio apps do.
//!
//! Linux: nothing. PipeWire's own data threads (where our audio capture runs,
//! RT_PROCESS) are already real time through rtkit, and making the encoder
//! thread real time would let a stuck GPU call starve the desktop.

/// Work a thread does, for its MMCSS task class.
#[derive(Clone, Copy)]
pub enum Task {
    Video,
    Audio,
}

/// While alive, the process runs with streaming priority and timer.
#[must_use]
pub struct Streaming(());

/// While alive, the current thread runs with its task's priority.
#[must_use]
pub struct Boost {
    #[cfg(target_os = "windows")]
    mmcss: windows_sys::Win32::Foundation::HANDLE,
}

#[cfg(not(target_os = "windows"))]
pub fn streaming() -> Streaming {
    Streaming(())
}

#[cfg(not(target_os = "windows"))]
pub fn boost(_: Task) -> Boost {
    Boost {}
}

#[cfg(target_os = "windows")]
mod win {
    use super::{Boost, Streaming, Task};
    use windows_sys::Win32::Media::{timeBeginPeriod, timeEndPeriod};
    use windows_sys::Win32::System::Threading::{
        ABOVE_NORMAL_PRIORITY_CLASS, AvRevertMmThreadCharacteristics, AvSetMmThreadCharacteristicsW, GetCurrentProcess, GetCurrentThread,
        NORMAL_PRIORITY_CLASS, PROCESS_POWER_THROTTLING_CURRENT_VERSION, PROCESS_POWER_THROTTLING_EXECUTION_SPEED,
        PROCESS_POWER_THROTTLING_IGNORE_TIMER_RESOLUTION, PROCESS_POWER_THROTTLING_STATE, ProcessPowerThrottling, SetPriorityClass,
        SetProcessInformation, SetThreadPriority, THREAD_PRIORITY_HIGHEST,
    };

    /// `control` lists what this process decides itself; `state` 0 = not throttled.
    fn throttling(control: u32) {
        let state = PROCESS_POWER_THROTTLING_STATE { Version: PROCESS_POWER_THROTTLING_CURRENT_VERSION, ControlMask: control, StateMask: 0 };
        // Fails on Windows before 10 1709: nothing to do then.
        unsafe {
            SetProcessInformation(
                GetCurrentProcess(),
                ProcessPowerThrottling,
                &state as *const _ as *const _,
                std::mem::size_of::<PROCESS_POWER_THROTTLING_STATE>() as u32,
            )
        };
    }

    pub fn streaming() -> Streaming {
        throttling(PROCESS_POWER_THROTTLING_EXECUTION_SPEED | PROCESS_POWER_THROTTLING_IGNORE_TIMER_RESOLUTION);
        unsafe {
            SetPriorityClass(GetCurrentProcess(), ABOVE_NORMAL_PRIORITY_CLASS);
            timeBeginPeriod(1);
        }
        tracing::info!("streaming priority on (1 ms timer, above normal)");
        Streaming(())
    }

    impl Drop for Streaming {
        fn drop(&mut self) {
            unsafe {
                timeEndPeriod(1);
                SetPriorityClass(GetCurrentProcess(), NORMAL_PRIORITY_CLASS);
            }
            throttling(0);
        }
    }

    pub fn boost(task: Task) -> Boost {
        let name: Vec<u16> = match task {
            Task::Video => "Games",
            Task::Audio => "Pro Audio",
        }
        .encode_utf16()
        .chain([0])
        .collect();
        let mut index = 0u32;
        let mmcss = unsafe { AvSetMmThreadCharacteristicsW(name.as_ptr(), &mut index) };
        if mmcss.is_null() {
            // MMCSS service off: at least a higher thread priority.
            unsafe { SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_HIGHEST) };
            tracing::info!("MMCSS unavailable; thread priority raised instead");
        }
        Boost { mmcss }
    }

    impl Drop for Boost {
        fn drop(&mut self) {
            if !self.mmcss.is_null() {
                unsafe { AvRevertMmThreadCharacteristics(self.mmcss) };
            }
        }
    }
}

#[cfg(target_os = "windows")]
pub use win::{boost, streaming};
