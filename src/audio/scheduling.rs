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
    fn pthread_override_qos_class_start_np(
        thread: libc::pthread_t,
        qos: u32,
        relative_priority: std::ffi::c_int,
    ) -> *mut std::ffi::c_void;
    fn pthread_override_qos_class_end_np(override_token: *mut std::ffi::c_void) -> std::ffi::c_int;
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
