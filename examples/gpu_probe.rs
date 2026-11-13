//! Cold-start probe 2: where a Rune window's GPU setup spends its time, phase by phase, with
//! Rune's own calls (`rune::render::Gpu`, `Renderer`) and with plain wgpu (the version and
//! features Rune builds) per backend and per adapter. A console program: each phase prints
//! `<phase> <ms> [detail]` as it ends. Run one mode per process, so each starts cold:
//!
//! ```text
//! gpu_probe rune       Rune's window path: Gpu::instance (on Windows, since Rune #625, DXGI's
//!                      adapter list, with no device made), a hidden window and its surface,
//!                      Gpu::try_new (the adapter pick from DXGI's descriptions and the one
//!                      device), Renderer::new (the precompiled DXBC uber shader where it
//!                      applies), Renderer::new_classic (the Classic uber shader macOS starts
//!                      with; elsewhere Rune starts with Windfoil), Renderer::new again
//!                      compiling the WGSL (RUNE_DISABLE_DXBC),
//!                      then configuring the surface and presenting one cleared frame.
//! gpu_probe rune-held  The same after listing the DX12 adapters through wgpu and holding them,
//!                      as Arbor did before Rune picked from DXGI's list (wgpu's listing makes a
//!                      D3D12 device on every adapter): what that workaround would cost now.
//! gpu_probe dx12-fxc   wgpu DX12 with FXC (Rune's setting): the instance, listing the adapters
//!                      (twice: wgpu makes a device on each to list it), then on every adapter a
//!                      device, Renderer::new with the DXBC and with the WGSL, and a first submit.
//! gpu_probe dx12-dxc   The same with DXC (dxcompiler.dll beside the exe).
//! gpu_probe vulkan     The same on Vulkan (WGSL to SPIR-V through naga, then the driver). Linux
//!                      only now: Rune no longer builds wgpu's Vulkan backend for Windows.
//! ```
//!
//! `cargo build --profile dist --example gpu_probe`; BENCH.md has what it found. Windows and
//! Linux (Arbor depends on wgpu directly only there).

#![cfg_attr(target_os = "macos", allow(dead_code, unused_imports))]

#[cfg(target_os = "macos")]
fn main() {
    eprintln!("gpu_probe runs on Windows and Linux");
}

#[cfg(not(target_os = "macos"))]
mod probe {

    use std::future::Future;
    use std::pin::pin;
    use std::task::{Context, Poll, Waker};
    use std::time::Instant;

    use rune::render::{Gpu, Renderer};

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

    /// Runs `f`, printing how long it took under `phase`, with `detail` from its result.
    fn timed<T>(phase: &str, f: impl FnOnce() -> T, detail: impl FnOnce(&T) -> String) -> T {
        let t = Instant::now();
        let v = f();
        let ms = t.elapsed().as_secs_f64() * 1000.0;
        let d = detail(&v);
        println!("{phase} {ms:.2}{}{d}", if d.is_empty() { "" } else { " " });
        v
    }

    fn quiet<T>(_: &T) -> String {
        String::new()
    }

    /// Rune's own device request (render.rs `Gpu::device_on`): its optional features (the
    /// passthrough one lets the DXBC uber shader load) and limits.
    fn device_on(adapter: &wgpu::Adapter) -> Result<(wgpu::Device, wgpu::Queue), String> {
        let optional = wgpu::Features::TIMESTAMP_QUERY | wgpu::Features::PASSTHROUGH_SHADERS;
        let required_features = adapter.features() & optional;
        let al = adapter.limits();
        let base = if al.max_inter_stage_shader_variables < wgpu::Limits::default().max_inter_stage_shader_variables
            || al.max_color_attachments < wgpu::Limits::default().max_color_attachments
            || al.max_compute_workgroup_storage_size < wgpu::Limits::default().max_compute_workgroup_storage_size
        {
            wgpu::Limits::downlevel_defaults()
        } else {
            wgpu::Limits::default()
        };
        let required_limits = wgpu::Limits { max_immediate_size: 0, ..base.using_resolution(al.clone()).using_alignment(al) };
        block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some("probe"),
            required_features,
            required_limits,
            experimental_features: Default::default(),
            memory_hints: wgpu::MemoryHints::MemoryUsage,
            trace: Default::default(),
        }))
        .map_err(|e| e.to_string())
    }

    /// `Renderer::new`, with Rune's precompiled DXBC allowed or not (`RUNE_DISABLE_DXBC`, read on
    /// each call).
    fn renderer(device: &wgpu::Device, wgsl: bool) -> Renderer {
        // SAFETY: the probe is single-threaded here; nothing else reads the environment meanwhile.
        unsafe {
            if wgsl {
                std::env::set_var("RUNE_DISABLE_DXBC", "1");
            } else {
                std::env::remove_var("RUNE_DISABLE_DXBC");
            }
        }
        let r = Renderer::new(device, wgpu::TextureFormat::Bgra8Unorm);
        unsafe { std::env::remove_var("RUNE_DISABLE_DXBC") };
        r
    }

    /// One cleared 8×8 frame submitted and waited for: the queue's first submit.
    fn first_submit(device: &wgpu::Device, queue: &wgpu::Queue) {
        let target = device.create_texture(&wgpu::TextureDescriptor {
            label: None,
            size: wgpu::Extent3d { width: 8, height: 8, depth_or_array_layers: 1 },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Bgra8Unorm,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            view_formats: &[],
        });
        clear_and_submit(device, queue, &target.create_view(&Default::default()));
    }

    fn clear_and_submit(device: &wgpu::Device, queue: &wgpu::Queue, view: &wgpu::TextureView) {
        let mut encoder = device.create_command_encoder(&Default::default());
        drop(encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: None,
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view,
                depth_slice: None,
                resolve_target: None,
                ops: wgpu::Operations { load: wgpu::LoadOp::Clear(wgpu::Color::BLACK), store: wgpu::StoreOp::Store },
            })],
            ..Default::default()
        }));
        queue.submit([encoder.finish()]);
        let _ = device.poll(wgpu::PollType::wait_indefinitely());
    }

    fn describe(a: &wgpu::Adapter) -> String {
        let i = a.get_info();
        format!("\"{}\" {:?} {:?} driver=\"{} {}\"", i.name, i.backend, i.device_type, i.driver, i.driver_info)
    }

    /// A plain wgpu instance on `backends`, DX12 set up as Rune sets it, with `dxc` (a path) or FXC.
    fn instance(backends: wgpu::Backends, dxc: Option<String>) -> wgpu::Instance {
        let mut d = wgpu::InstanceDescriptor::new_without_display_handle();
        d.backends = backends;
        d.flags.remove(wgpu::InstanceFlags::VALIDATION_INDIRECT_CALL);
        d.backend_options.dx12.presentation_system = wgpu::Dx12SwapchainKind::DxgiFromVisual;
        d.backend_options.dx12.shader_compiler = match dxc {
            Some(dxc_path) => wgpu::Dx12Compiler::DynamicDxc { dxc_path },
            None => wgpu::Dx12Compiler::Fxc,
        };
        wgpu::Instance::new(d)
    }

    /// The plain wgpu modes: instance, adapters, and per adapter a device and Rune's renderer.
    fn plain(backends: wgpu::Backends, dxc: Option<String>) {
        let dx12 = backends.contains(wgpu::Backends::DX12);
        let inst = timed("instance", || instance(backends, dxc), quiet);
        let adapters = timed("enumerate_adapters", || block_on(inst.enumerate_adapters(backends)), |a| format!("count={}", a.len()));
        timed("enumerate_adapters_again", || block_on(inst.enumerate_adapters(backends)).len(), |n| format!("count={n}"));
        for (i, a) in adapters.iter().enumerate() {
            println!("adapter[{i}] {}", describe(a));
        }
        for (i, a) in adapters.iter().enumerate() {
            let Ok((device, queue)) = timed(
                &format!("device[{i}]"),
                || device_on(a),
                |r| match r {
                    Ok(_) => String::new(),
                    Err(e) => format!("error={e:?}"),
                },
            ) else {
                continue;
            };
            if dx12 {
                timed(&format!("renderer_dxbc[{i}]"), || drop(renderer(&device, false)), quiet);
            }
            timed(&format!("renderer_wgsl[{i}]"), || drop(renderer(&device, true)), quiet);
            timed(&format!("first_submit[{i}]"), || first_submit(&device, &queue), quiet);
        }
    }

    /// Rune's own path for a window, in the order a window takes it.
    fn rune_path() {
        let instance = timed("rune_instance", Gpu::instance, quiet);
        let window = win::Window::new();
        let surface = window.as_ref().map(|w| timed("create_surface", || w.surface(&instance), quiet));
        let gpu = timed(
            "rune_try_new",
            || Gpu::try_new(instance, surface.as_ref()),
            |g| match g {
                Ok(g) => describe(&g.adapter),
                Err(e) => format!("error={e:?}"),
            },
        );
        let Ok(gpu) = gpu else { return };
        timed("rune_renderer_dxbc", || drop(renderer(&gpu.device, false)), quiet);
        // The Classic uber shader macOS starts with (Rune's hidden hook for checking that path):
        // what a Linux renderer would cost without Windfoil.
        timed("rune_renderer_classic", || drop(Renderer::new_classic(&gpu.device, wgpu::TextureFormat::Bgra8Unorm)), quiet);
        timed("rune_renderer_wgsl", || drop(renderer(&gpu.device, true)), quiet);
        match &surface {
            Some(surface) => {
                timed(
                    "configure_and_first_present",
                    || {
                        let config = surface.get_default_config(&gpu.adapter, 980, 620)?;
                        surface.configure(&gpu.device, &config);
                        let frame = surface.get_current_texture();
                        let wgpu::CurrentSurfaceTexture::Success(frame) = frame else { return None };
                        clear_and_submit(&gpu.device, &gpu.queue, &frame.texture.create_view(&Default::default()));
                        gpu.queue.present(frame);
                        Some(config.format)
                    },
                    |f| format!("format={f:?}"),
                );
            }
            None => {
                timed("first_submit", || first_submit(&gpu.device, &gpu.queue), quiet);
            }
        }
        // `surface` drops before `window` (declared after it).
    }

    pub fn main() {
        let start = Instant::now();
        let mode = std::env::args().nth(1).unwrap_or_else(|| "rune".into());
        println!("mode {mode}");
        match mode.as_str() {
            "rune" => rune_path(),
            // Arbor's old workaround (src/gpu.rs before Rune's DXGI pick): the DX12 adapters
            // listed first and held, so Rune's own listings got the live D3D12 devices back.
            "rune-held" => {
                let inst = instance(wgpu::Backends::DX12, None);
                let held = timed("hold_list", || block_on(inst.enumerate_adapters(wgpu::Backends::DX12)), |a| format!("count={}", a.len()));
                rune_path();
                drop(held);
            }
            "dx12-fxc" => plain(wgpu::Backends::DX12, None),
            "dx12-dxc" => {
                let dll = std::env::current_exe().ok().and_then(|p| Some(p.parent()?.join("dxcompiler.dll"))).filter(|p| p.exists());
                match dll {
                    Some(p) => plain(wgpu::Backends::DX12, Some(p.to_string_lossy().into_owned())),
                    None => println!("error no dxcompiler.dll beside the exe"),
                }
            }
            "vulkan" => plain(wgpu::Backends::VULKAN, None),
            other => println!("error unknown mode {other}"),
        }
        println!("total {:.2}", start.elapsed().as_secs_f64() * 1000.0);
    }

    /// A hidden window to make a surface on (Windows; elsewhere there's none and the Rune path
    /// runs offscreen).
    #[cfg(windows)]
    mod win {
        use std::num::NonZeroIsize;

        use windows_sys::Win32::Foundation::HWND;
        use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
        use windows_sys::Win32::UI::WindowsAndMessaging::{
            CW_USEDEFAULT, CreateWindowExW, DefWindowProcW, DestroyWindow, RegisterClassW, WNDCLASSW, WS_OVERLAPPEDWINDOW,
        };

        pub struct Window(HWND);

        impl Window {
            pub fn new() -> Option<Window> {
                let class: Vec<u16> = "gpu-probe".encode_utf16().chain(Some(0)).collect();
                // SAFETY: plain Win32 calls with live, NUL-terminated strings; the class outlives the window.
                let hwnd = unsafe {
                    let instance = GetModuleHandleW(std::ptr::null());
                    let wc = WNDCLASSW {
                        lpfnWndProc: Some(DefWindowProcW),
                        hInstance: instance,
                        lpszClassName: class.as_ptr(),
                        ..std::mem::zeroed()
                    };
                    RegisterClassW(&wc);
                    CreateWindowExW(
                        0,
                        class.as_ptr(),
                        class.as_ptr(),
                        WS_OVERLAPPEDWINDOW,
                        CW_USEDEFAULT,
                        CW_USEDEFAULT,
                        980,
                        620,
                        std::ptr::null_mut(),
                        std::ptr::null_mut(),
                        instance,
                        std::ptr::null(),
                    )
                };
                (!hwnd.is_null()).then_some(Window(hwnd))
            }

            pub fn surface(&self, instance: &wgpu::Instance) -> wgpu::Surface<'static> {
                use wgpu::rwh::{RawDisplayHandle, RawWindowHandle, Win32WindowHandle, WindowsDisplayHandle};
                let handle = Win32WindowHandle::new(NonZeroIsize::new(self.0 as isize).expect("a window"));
                // SAFETY: the window lives until `Window` drops, after the surface (`rune_path`).
                unsafe {
                    instance
                        .create_surface_unsafe(wgpu::SurfaceTargetUnsafe::RawHandle {
                            raw_display_handle: Some(RawDisplayHandle::Windows(WindowsDisplayHandle::new())),
                            raw_window_handle: RawWindowHandle::Win32(handle),
                        })
                        .expect("a surface")
                }
            }
        }

        impl Drop for Window {
            fn drop(&mut self) {
                // SAFETY: our own window, destroyed once.
                unsafe { DestroyWindow(self.0) };
            }
        }
    }

    #[cfg(not(windows))]
    mod win {
        pub struct Window;

        impl Window {
            pub fn new() -> Option<Window> {
                None
            }

            pub fn surface(&self, _: &wgpu::Instance) -> wgpu::Surface<'static> {
                unreachable!("no window off Windows")
            }
        }
    }
}

#[cfg(not(target_os = "macos"))]
fn main() {
    probe::main();
}
