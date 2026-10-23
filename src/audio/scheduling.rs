//! Scoped latency scheduling for PCM work and explicit interactive barriers.
pub(super) struct LatencyScope {
    #[cfg(target_os = "macos")]
    native: *mut std::ffi::c_void,
}

impl LatencyScope {
    pub(super) fn enter() -> Self {
        Self {
            #[cfg(target_os = "macos")]
            // QoS overrides preserve the thread's original requested class.
            native: unsafe {
                pthread_override_qos_class_start_np(libc::pthread_self(), 0x21, 0)
            },
        }
    }
}

#[cfg(target_os = "macos")]
unsafe extern "C" {
    fn pthread_set_qos_class_self_np(
        qos: u32,
        relative_priority: std::ffi::c_int,
    ) -> std::ffi::c_int;
    fn mach_wait_until(deadline: u64) -> std::ffi::c_int;
    fn pthread_override_qos_class_start_np(
        thread: libc::pthread_t,
        qos: u32,
        relative_priority: std::ffi::c_int,
    ) -> *mut std::ffi::c_void;
    fn pthread_override_qos_class_end_np(override_token: *mut std::ffi::c_void) -> std::ffi::c_int;
}

pub(super) fn configure_audio_thread() {
    #[cfg(target_os = "macos")]
    {
        // A permanently owned audio worker declares its requested QoS directly;
        // temporary callers instead use the scoped dependency override above.
        let result = unsafe { pthread_set_qos_class_self_np(0x21, 0) };
        if result != 0 {
            eprintln!(
                "Cannot configure audio thread QoS: {}",
                std::io::Error::from_raw_os_error(result)
            );
        }
    }
}

pub(super) fn wait_render_deadline(remaining: std::time::Duration) {
    #[cfg(target_os = "macos")]
    {
        static TIMEBASE: std::sync::OnceLock<Option<(u32, u32)>> = std::sync::OnceLock::new();
        if let Some(&(numer, denom)) = TIMEBASE
            .get_or_init(|| {
                let mut info = libc::mach_timebase_info { numer: 0, denom: 0 };
                (unsafe { libc::mach_timebase_info(&mut info) } == 0
                    && info.numer != 0
                    && info.denom != 0)
                    .then_some((info.numer, info.denom))
            })
            .as_ref()
        {
            let ticks = remaining
                .as_nanos()
                .saturating_mul(u128::from(denom))
                .div_ceil(u128::from(numer))
                .min(u128::from(u64::MAX)) as u64;
            let deadline = unsafe { libc::mach_absolute_time() }.saturating_add(ticks);
            // Mach absolute deadlines avoid the coalescing of condition-variable
            // timeouts. Shutdown and commands are checked again within one batch.
            let result = unsafe { mach_wait_until(deadline) };
            if result == 0 || result == 14 {
                return;
            } // KERN_ABORTED is a wakeup.
        }
    }
    std::thread::park_timeout(remaining);
}

impl Drop for LatencyScope {
    fn drop(&mut self) {
        #[cfg(target_os = "macos")]
        if !self.native.is_null() {
            // The opaque token is ended exactly once when its work completes.
            unsafe { pthread_override_qos_class_end_np(self.native) };
        }
    }
}
