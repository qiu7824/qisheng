//! Native timer diagnostics for hosted macOS runners; no application files are opened.
use std::ffi::{c_int, c_void};
use std::sync::{Condvar, Mutex, mpsc};
use std::time::{Duration, Instant};

#[repr(C)]
struct Timebase {
    numer: u32,
    denom: u32,
}
unsafe extern "C" {
    fn pthread_self() -> *mut c_void;
    fn pthread_get_qos_class_np(thread: *mut c_void, qos: *mut u32, priority: *mut c_int) -> c_int;
    fn pthread_set_qos_class_self_np(qos: u32, priority: c_int) -> c_int;
    fn mach_absolute_time() -> u64;
    fn mach_timebase_info(info: *mut Timebase) -> c_int;
    fn mach_wait_until(deadline: u64) -> c_int;
}

fn sample(name: &str, milliseconds: u64, wait: impl Fn(Duration)) {
    let mut durations = Vec::new();
    for _ in 0..8 {
        let start = Instant::now();
        wait(Duration::from_millis(milliseconds));
        durations.push(start.elapsed().as_secs_f64() * 1000.0);
    }
    println!(
        "{{\"case\":\"{name}\",\"requested_ms\":{milliseconds},\"observed_ms\":{durations:?}}}"
    );
}

fn main() {
    println!("library_backtrace={:?}", std::env::var("RUST_LIB_BACKTRACE"));
    let mut timebase = Timebase { numer: 0, denom: 0 };
    assert_eq!(unsafe { mach_timebase_info(&mut timebase) }, 0);
    assert!(timebase.numer > 0 && timebase.denom > 0);
    for (name, qos) in [("inherited", None), ("interactive", Some(0x21))] {
        if let Some(qos) = qos {
            println!("set_qos={}", unsafe {
                pthread_set_qos_class_self_np(qos, 0)
            });
        }
        let (mut current, mut priority) = (0, 0);
        let status =
            unsafe { pthread_get_qos_class_np(pthread_self(), &mut current, &mut priority) };
        println!(
            "{{\"mode\":\"{name}\",\"qos_status\":{status},\"qos\":{current},\"priority\":{priority},\"numer\":{},\"denom\":{}}}",
            timebase.numer, timebase.denom
        );
        sample("park", 2, std::thread::park_timeout);
        sample("backtrace", 0, |_| {
            std::hint::black_box(std::backtrace::Backtrace::capture());
        });
        sample("sleep", 20, std::thread::sleep);
        sample("channel", 100, |duration| {
            let (_sender, receiver) = mpsc::channel::<()>();
            assert_eq!(
                receiver.recv_timeout(duration),
                Err(mpsc::RecvTimeoutError::Timeout)
            );
        });
        sample("condvar", 100, |duration| {
            let mutex = Mutex::new(());
            let wake = Condvar::new();
            assert!(
                wake.wait_timeout(mutex.lock().unwrap(), duration)
                    .unwrap()
                    .1
                    .timed_out()
            );
        });
        sample("mach", 20, |duration| {
            let ticks = (duration.as_nanos() * u128::from(timebase.denom))
                .div_ceil(u128::from(timebase.numer)) as u64;
            assert_eq!(unsafe { mach_wait_until(mach_absolute_time() + ticks) }, 0);
        });
    }
}
