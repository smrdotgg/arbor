//! Getting the window's GPU ready early: started first thing in `main`.
//!
//! Windows and Linux: Rune's `warm_gpu` makes the window's device and renderer on a thread while
//! the event loop, fonts and window are made, and the window takes them. (macOS does the same
//! through `rune::prewarm_gpu`, called from `main`.)
//!
//! Windows release builds also list the DX12 adapters first and hold that list while Rune makes
//! its device. wgpu's DX12 backend makes a D3D12 device on every adapter to list it, and Rune
//! lists them twice before making its own device: once to ask whether any DX12 adapter exists
//! (dropping the list, and with it those devices), then again to choose one. A D3D12 device is a
//! singleton per adapter and process, so while a list is held each later `D3D12CreateDevice`
//! gets the live device back instead of making it again: on the owner's laptop (an NVIDIA and
//! an AMD GPU) 655 → 411 ms to the first frame (BENCH.md, 2026-11-13).
//!
//! The list goes as soon as Rune's device exists (`rune::warm_gpu_with`'s callback, after both
//! of Rune's listings, whether or not a window ever opens), and failing that at the first frame
//! on screen or when Arbor quits ([`release`]). Once released it can't be held again: a listing
//! that finishes after a release drops what it made ([`Hold`]). The devices are dropped on a
//! thread of their own, never under the lock or on the thread that released them.
//!
//! Not in debug builds: wgpu's debug defaults turn on the D3D12 debug layer, and a debug layer
//! enabled after devices exist removes them (Microsoft's `D3D12GetDebugInterface` docs), so a
//! held list would break Rune's own instance there. Release builds make both instances with the
//! same flags (wgpu's release defaults, less indirect-call validation, as Rune does).

/// Starts getting the GPU ready. Not for headless runs, which make no window (`main` decides).
pub fn start() {
    #[cfg(all(windows, not(debug_assertions)))]
    {
        let spawned = std::thread::Builder::new().name("arbor-gpu".into()).spawn(|| {
            hold::list();
            // After Rune's device exists (both of its listings are done): the list goes.
            rune::warm_gpu_with(wgpu::Features::empty(), |_| release());
        });
        if spawned.is_err() {
            release();
            rune::warm_gpu(wgpu::Features::empty());
        }
    }
    #[cfg(any(all(windows, debug_assertions), all(not(windows), not(target_os = "macos"))))]
    rune::warm_gpu(wgpu::Features::empty());
}

/// What `start` holds goes now (Rune's device exists, the first frame is on screen, or Arbor is
/// quitting), and nothing is held after. Any thread; more than once is fine.
pub fn release() {
    #[cfg(all(windows, not(debug_assertions)))]
    hold::release();
}

/// When the adapters were listed (Windows release builds), for `ARBOR_TIMING`.
pub fn listed_at() -> Option<std::time::Instant> {
    #[cfg(all(windows, not(debug_assertions)))]
    return hold::LISTED.get().copied();
    #[cfg(not(all(windows, not(debug_assertions))))]
    None
}

/// Something held until released, once: after [`Hold::release`] nothing can be held again.
/// Whatever comes out of it is the caller's to drop, outside the lock. Used by Windows release
/// builds (and tested everywhere).
#[cfg_attr(not(all(windows, not(debug_assertions))), allow(dead_code))]
pub struct Hold<T> {
    state: std::sync::Mutex<State<T>>,
}

#[cfg_attr(not(all(windows, not(debug_assertions))), allow(dead_code))]
enum State<T> {
    Empty,
    Held(T),
    Released,
}

#[cfg_attr(not(all(windows, not(debug_assertions))), allow(dead_code))]
impl<T> Hold<T> {
    pub const fn new() -> Self {
        Hold { state: std::sync::Mutex::new(State::Empty) }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State<T>> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Whether it was released already (a listing needn't start).
    pub fn is_released(&self) -> bool {
        matches!(*self.lock(), State::Released)
    }

    /// Holds `value`, or gives it back when released already (or holding something).
    pub fn publish(&self, value: T) -> Result<(), T> {
        let mut state = self.lock();
        match *state {
            State::Empty => {
                *state = State::Held(value);
                Ok(())
            }
            State::Held(_) | State::Released => Err(value),
        }
    }

    /// Released from now on; what was held comes back to be dropped.
    pub fn release(&self) -> Option<T> {
        // The guard goes with this statement, before the value leaves.
        let previous = std::mem::replace(&mut *self.lock(), State::Released);
        match previous {
            State::Held(value) => Some(value),
            State::Empty | State::Released => None,
        }
    }
}

#[cfg(all(windows, not(debug_assertions)))]
mod hold {
    use std::future::Future;
    use std::pin::pin;
    use std::sync::OnceLock;
    use std::task::{Context, Poll, Waker};
    use std::time::Instant;

    use super::Hold;

    /// The adapters `list` made, each holding its D3D12 device.
    static HELD: Hold<Vec<wgpu::Adapter>> = Hold::new();
    pub(super) static LISTED: OnceLock<Instant> = OnceLock::new();

    /// wgpu's native futures are ready when first polled.
    fn block_on<F: Future>(f: F) -> F::Output {
        let mut f = pin!(f);
        let mut cx = Context::from_waker(Waker::noop());
        loop {
            if let Poll::Ready(v) = f.as_mut().poll(&mut cx) {
                return v;
            }
            std::thread::yield_now();
        }
    }

    /// Drops the adapters (and their devices) on a thread of their own.
    fn drop_later(adapters: Vec<wgpu::Adapter>) {
        let spawned = std::thread::Builder::new().name("arbor-gpu-drop".into()).spawn(move || drop(adapters));
        // No thread: dropped here (the `Err` holds them), still outside the lock.
        drop(spawned);
    }

    pub(super) fn list() {
        if HELD.is_released() {
            return;
        }
        // Rune's own instance flags (render.rs `new_wgpu_instance`): wgpu's release defaults,
        // without indirect-call validation.
        let mut desc = wgpu::InstanceDescriptor::new_without_display_handle();
        desc.backends = wgpu::Backends::DX12;
        desc.flags.remove(wgpu::InstanceFlags::VALIDATION_INDIRECT_CALL);
        desc.backend_options.dx12.presentation_system = wgpu::Dx12SwapchainKind::DxgiFromVisual;
        // FXC ships with Windows (nothing is compiled here); the default looks for a
        // dxcompiler.dll on PATH (Rune's own note, render.rs).
        desc.backend_options.dx12.shader_compiler = wgpu::Dx12Compiler::Fxc;
        let instance = wgpu::Instance::new(desc);
        let adapters = block_on(instance.enumerate_adapters(wgpu::Backends::DX12));
        match HELD.publish(adapters) {
            Ok(()) => {
                let _ = LISTED.set(Instant::now());
            }
            // Released while listing: nobody would release these.
            Err(late) => drop_later(late),
        }
    }

    pub(super) fn release() {
        if let Some(adapters) = HELD.release() {
            drop_later(adapters);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Hold;

    /// A listing that finishes after the release can't hold its adapters (they'd never go).
    #[test]
    fn a_release_before_the_listing_lands_refuses_it() {
        let hold: Hold<Vec<u8>> = Hold::new();
        assert_eq!(hold.release(), None);
        assert!(hold.is_released());
        assert_eq!(hold.publish(vec![1, 2]), Err(vec![1, 2]), "late: given back to be dropped");
        assert_eq!(hold.release(), None, "nothing held after a release");
    }

    /// Held until the first release, which hands it back once.
    #[test]
    fn what_is_held_comes_back_once() {
        let hold: Hold<Vec<u8>> = Hold::new();
        assert_eq!(hold.publish(vec![7]), Ok(()));
        assert!(!hold.is_released());
        assert_eq!(hold.release(), Some(vec![7]));
        assert_eq!(hold.release(), None);
        assert_eq!(hold.publish(vec![8]), Err(vec![8]));
    }

    /// The value comes out of the lock: dropping it can't happen under the mutex.
    #[test]
    fn release_hands_the_value_out_of_the_lock() {
        struct Probe<'a>(&'a Hold<Probe<'a>>);
        impl Drop for Probe<'_> {
            fn drop(&mut self) {
                // Would deadlock (or poison) if dropped while `release` held the lock.
                assert!(self.0.is_released());
            }
        }
        let hold: &'static Hold<Probe<'static>> = Box::leak(Box::new(Hold::new()));
        assert!(hold.publish(Probe(hold)).is_ok());
        drop(hold.release());
    }
}
