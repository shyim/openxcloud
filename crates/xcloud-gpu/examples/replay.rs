//! Phase 0 spike B: decode `fixtures/testsrc_1080p60.h264` through VideoToolbox at 60 fps
//! in a loop, import every frame into wgpu without a copy, run NV12 → RGBA + FSR 1.0 and
//! show it in an eframe window with a latency HUD. Exits after `--frames N` presented
//! frames and prints avg/p99 of every stage.
//!
//! ```text
//! cargo run --release -p xcloud-gpu --example replay -- [--present fifo|immediate] [--frames N]
//!     [--sharpness S] [--upscaler fsr|metalfx|bypass] [--no-upscale] [path/to/stream.h264]
//! ```
//!
//! `--upscaler metalfx` needs `--features metalfx` (macOS); without it the renderer logs the
//! fallback once and runs FSR.

#[cfg(target_os = "macos")]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    macos::main()
}

#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("replay: this spike needs the VideoToolbox decoder and only runs on macOS");
}

#[cfg(target_os = "macos")]
mod macos {
    use std::collections::VecDeque;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    use eframe::egui;
    use egui_wgpu::CallbackTrait;
    use xcloud_gpu::{UpscalePolicy, Upscaler, VideoRenderer, VideoTexture};
    use xcloud_media::annexb::split_access_units;
    use xcloud_media::vt::{VtConfig, VtDecoder};
    use xcloud_media::{Frame, FrameSink, VideoDecoder};

    const DEFAULT_FIXTURE: &str = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../fixtures/testsrc_1080p60.h264"
    );
    const FPS: u32 = 60;

    struct Args {
        present: wgpu::PresentMode,
        frames: usize,
        sharpness: f32,
        upscaler: Upscaler,
        path: String,
    }

    fn parse_args() -> Result<Args, String> {
        let mut args = Args {
            present: wgpu::PresentMode::Immediate,
            frames: 300,
            sharpness: xcloud_gpu::DEFAULT_SHARPNESS,
            upscaler: Upscaler::from_env(),
            path: DEFAULT_FIXTURE.to_string(),
        };
        let mut it = std::env::args().skip(1);
        while let Some(arg) = it.next() {
            match arg.as_str() {
                "--present" => {
                    let v = it.next().ok_or("--present needs fifo|immediate")?;
                    args.present = match v.as_str() {
                        "fifo" => wgpu::PresentMode::Fifo,
                        "immediate" => wgpu::PresentMode::Immediate,
                        "mailbox" => wgpu::PresentMode::Mailbox,
                        other => return Err(format!("unknown present mode {other}")),
                    };
                }
                "--frames" => {
                    let v = it.next().ok_or("--frames needs a number")?;
                    args.frames = v.parse().map_err(|e| format!("--frames: {e}"))?;
                }
                "--sharpness" => {
                    let v = it.next().ok_or("--sharpness needs a number")?;
                    args.sharpness = v.parse().map_err(|e| format!("--sharpness: {e}"))?;
                }
                "--upscaler" => {
                    let v = it.next().ok_or("--upscaler needs fsr|metalfx|bypass")?;
                    args.upscaler =
                        Upscaler::parse(&v).ok_or_else(|| format!("unknown upscaler {v}"))?;
                }
                "--no-upscale" => args.upscaler = Upscaler::Bypass,
                "-h" | "--help" => {
                    return Err("usage: replay [--present fifo|immediate] [--frames N] [--sharpness S] [--upscaler fsr|metalfx|bypass] [--no-upscale] [file.h264]".to_string());
                }
                other if other.starts_with("--") => return Err(format!("unknown flag {other}")),
                other => args.path = other.to_string(),
            }
        }
        Ok(args)
    }

    /// What the decode thread publishes: the frame and a serial so the UI can tell new
    /// frames from repaints for other reasons.
    #[derive(Clone)]
    struct Published {
        serial: u64,
        frame: Arc<Frame>,
    }

    /// Per-frame measurements, in milliseconds, collected from the render thread and the
    /// paint callback.
    #[derive(Default)]
    struct Stats {
        decode: Vec<f64>,
        prepare_cpu: Vec<f64>,
        gpu_total: Vec<f64>,
        gpu_convert: Vec<f64>,
        gpu_easu: Vec<f64>,
        gpu_rcas: Vec<f64>,
        gpu_metalfx: Vec<f64>,
        age_at_prepare: Vec<f64>,
        age_at_gpu_done: Vec<f64>,
        frame_interval: Vec<f64>,
        zero_copy_frames: u64,
        cpu_upload_frames: u64,
        prepare_errors: u64,
        last_gpu: Option<xcloud_gpu::GpuTimings>,
        last_prepare_ms: f64,
        last_age_prepare_ms: f64,
        last_age_done_ms: Option<f64>,
        last_zero_copy: bool,
        in_flight: usize,
        stalls: u64,
        zero_copy_available: bool,
        zero_copy_reason: Option<String>,
        timestamps_supported: bool,
    }

    /// Runs the renderer inside egui's `prepare` so the compute passes land in egui's
    /// command encoder, ahead of its render pass, on the same queue.
    struct VideoCallback {
        frame: Arc<Frame>,
        stats: Arc<Mutex<Stats>>,
    }

    impl CallbackTrait for VideoCallback {
        fn prepare(
            &self,
            _device: &wgpu::Device,
            _queue: &wgpu::Queue,
            _screen: &egui_wgpu::ScreenDescriptor,
            encoder: &mut wgpu::CommandEncoder,
            resources: &mut egui_wgpu::CallbackResources,
        ) -> Vec<wgpu::CommandBuffer> {
            let Some(renderer) = resources.get_mut::<VideoRenderer>() else {
                return Vec::new();
            };
            let t0 = Instant::now();
            let age = t0.saturating_duration_since(self.frame.t_decoded);
            let result = renderer.prepare(encoder, &self.frame).map(|_| ());
            let prepare_ms = t0.elapsed().as_secs_f64() * 1e3;
            if let Ok(mut s) = self.stats.lock() {
                match result {
                    Ok(()) => {
                        s.decode
                            .push(self.frame.decode_latency().as_secs_f64() * 1e3);
                        s.prepare_cpu.push(prepare_ms);
                        s.age_at_prepare.push(age.as_secs_f64() * 1e3);
                        if let Some(t) = renderer.gpu_timings() {
                            s.gpu_total.push(t.total_ms());
                            s.gpu_convert.push(t.convert_ms);
                            s.gpu_easu.push(t.easu_ms);
                            s.gpu_rcas.push(t.rcas_ms);
                            s.gpu_metalfx.push(t.metalfx_ms);
                            s.last_gpu = Some(t);
                        }
                        if let Some(d) = renderer.last_gpu_done_age() {
                            let ms = d.as_secs_f64() * 1e3;
                            s.age_at_gpu_done.push(ms);
                            s.last_age_done_ms = Some(ms);
                        }
                        if renderer.last_frame_zero_copy() {
                            s.zero_copy_frames += 1;
                        } else {
                            s.cpu_upload_frames += 1;
                        }
                        s.last_zero_copy = renderer.last_frame_zero_copy();
                        s.last_prepare_ms = prepare_ms;
                        s.last_age_prepare_ms = age.as_secs_f64() * 1e3;
                    }
                    Err(e) => {
                        s.prepare_errors += 1;
                        eprintln!("renderer.prepare failed: {e}");
                    }
                }
                s.in_flight = renderer.in_flight();
                s.stalls = renderer.stalls();
                s.zero_copy_available = renderer.importer().zero_copy_available();
                s.zero_copy_reason = renderer
                    .importer()
                    .zero_copy_disabled_reason()
                    .map(str::to_string);
                s.timestamps_supported = renderer.timestamps_supported();
            }
            Vec::new()
        }

        fn paint(
            &self,
            _info: egui::PaintCallbackInfo,
            _render_pass: &mut wgpu::RenderPass<'static>,
            _resources: &egui_wgpu::CallbackResources,
        ) {
            // The video is drawn by egui itself through the registered native texture.
        }
    }

    struct ReplayApp {
        output: triple_buffer::Output<Option<Published>>,
        texture: VideoTexture,
        stats: Arc<Mutex<Stats>>,
        stop: Arc<AtomicBool>,
        decoder_thread: Option<std::thread::JoinHandle<()>>,
        last_serial: Option<u64>,
        presented: usize,
        target_frames: usize,
        present_label: &'static str,
        present_times: VecDeque<Instant>,
        last_present: Option<Instant>,
        source_size: (u32, u32),
        output_size: (u32, u32),
        upscaled: bool,
        /// `renderer.upscaler_label()`: "FSR", "FX" or "".
        upscaler_label: &'static str,
        requested: Upscaler,
        started: Instant,
        reported: bool,
    }

    impl ReplayApp {
        fn new(
            cc: &eframe::CreationContext<'_>,
            args: &Args,
            aus: Vec<Vec<u8>>,
        ) -> Result<Self, String> {
            let render_state = cc.wgpu_render_state.as_ref().ok_or(
                "eframe did not give us a wgpu render state (is the wgpu renderer enabled?)",
            )?;
            let mut renderer = VideoRenderer::with_upscaler(
                &render_state.device,
                &render_state.queue,
                UpscalePolicy::default(),
                args.upscaler,
            );
            renderer.set_sharpness(args.sharpness);
            let stats = Arc::new(Mutex::new(Stats {
                zero_copy_available: renderer.importer().zero_copy_available(),
                zero_copy_reason: renderer
                    .importer()
                    .zero_copy_disabled_reason()
                    .map(str::to_string),
                timestamps_supported: renderer.timestamps_supported(),
                ..Stats::default()
            }));
            render_state
                .renderer
                .write()
                .callback_resources
                .insert(renderer);

            let (input, output) = triple_buffer::triple_buffer(&None);
            let stop = Arc::new(AtomicBool::new(false));
            let ctx = cc.egui_ctx.clone();
            let thread_stop = Arc::clone(&stop);
            let decoder_thread = std::thread::Builder::new()
                .name("video-decode".into())
                .spawn(move || decode_loop(aus, input, ctx, thread_stop))
                .map_err(|e| format!("spawn decoder thread: {e}"))?;

            Ok(Self {
                output,
                texture: VideoTexture::new(),
                stats,
                stop,
                decoder_thread: Some(decoder_thread),
                last_serial: None,
                presented: 0,
                target_frames: args.frames,
                present_label: match args.present {
                    wgpu::PresentMode::Fifo => "fifo",
                    wgpu::PresentMode::Mailbox => "mailbox",
                    _ => "immediate",
                },
                present_times: VecDeque::new(),
                last_present: None,
                source_size: (0, 0),
                output_size: (0, 0),
                upscaled: false,
                upscaler_label: "",
                requested: args.upscaler,
                started: Instant::now(),
                reported: false,
            })
        }

        fn fps(&self) -> f64 {
            match (self.present_times.front(), self.present_times.back()) {
                (Some(first), Some(last)) if self.present_times.len() >= 2 => {
                    let span = last.saturating_duration_since(*first).as_secs_f64();
                    if span > 0.0 {
                        (self.present_times.len() - 1) as f64 / span
                    } else {
                        0.0
                    }
                }
                _ => 0.0,
            }
        }

        fn report(&mut self) {
            if self.reported {
                return;
            }
            self.reported = true;
            let Ok(s) = self.stats.lock() else {
                return;
            };
            let wall = self.started.elapsed().as_secs_f64();
            println!();
            println!(
                "== replay: {} frames presented in {wall:.1} s ({:.1} fps), present={}, upscaler={} (requested {}), upscaled={}",
                self.presented,
                self.presented as f64 / wall.max(1e-9),
                self.present_label,
                upscaler_name(self.upscaler_label),
                self.requested.as_str(),
                self.upscaled,
            );
            println!(
                "   source {}x{} → output {}x{}",
                self.source_size.0, self.source_size.1, self.output_size.0, self.output_size.1
            );
            println!(
                "   import: zero-copy frames {} / CPU-upload frames {} / prepare errors {}; zero-copy {}",
                s.zero_copy_frames,
                s.cpu_upload_frames,
                s.prepare_errors,
                match &s.zero_copy_reason {
                    None => "available".to_string(),
                    Some(r) => format!("DISABLED: {r}"),
                }
            );
            println!(
                "   GPU timestamps: {}",
                if s.timestamps_supported {
                    "TIMESTAMP_QUERY"
                } else {
                    "unavailable"
                }
            );
            println!("   in flight at exit {}  stalls {}", s.in_flight, s.stalls);
            println!(
                "   {:<28} {:>9} {:>9} {:>9}  (ms)",
                "stage", "avg", "p99", "max"
            );
            let stage = |name: &str, v: &[f64]| {
                if v.is_empty() {
                    println!("   {name:<28} {:>9} {:>9} {:>9}", "-", "-", "-");
                    return;
                }
                let mut sorted = v.to_vec();
                sorted.sort_by(f64::total_cmp);
                let avg = sorted.iter().sum::<f64>() / sorted.len() as f64;
                let p99 = sorted[((sorted.len() - 1) as f64 * 0.99).round() as usize];
                let max = sorted[sorted.len() - 1];
                println!("   {name:<28} {avg:>9.3} {p99:>9.3} {max:>9.3}");
            };
            stage("decode (submit→callback)", &s.decode);
            stage("prepare CPU (import+encode)", &s.prepare_cpu);
            stage("GPU total", &s.gpu_total);
            stage("GPU nv12→rgba", &s.gpu_convert);
            stage("GPU FSR EASU", &s.gpu_easu);
            stage("GPU FSR RCAS", &s.gpu_rcas);
            stage("GPU MetalFX (cmd buffer)", &s.gpu_metalfx);
            stage("frame age at prepare", &s.age_at_prepare);
            stage("frame age at GPU done", &s.age_at_gpu_done);
            stage("present interval", &s.frame_interval);
        }
    }

    impl eframe::App for ReplayApp {
        fn ui(&mut self, ui: &mut egui::Ui, frame: &mut eframe::Frame) {
            let ctx = ui.ctx().clone();
            let Some(render_state) = frame.wgpu_render_state() else {
                return;
            };

            // The previous frame's encoder has been submitted by now: release what the
            // GPU finished and advance the timestamp readback.
            {
                let mut egui_renderer = render_state.renderer.write();
                if let Some(vr) = egui_renderer.callback_resources.get_mut::<VideoRenderer>() {
                    vr.after_submit();
                }
            }

            // Latest decoded frame (lock-free, latest-wins).
            let new_frame = self.output.update();
            let Some(published) = self.output.output_buffer().clone() else {
                egui::CentralPanel::default().show(ui, |ui| {
                    ui.centered_and_justified(|ui| {
                        ui.label("waiting for the first decoded frame…")
                    });
                });
                ctx.request_repaint_after(Duration::from_millis(10));
                return;
            };
            let is_new = new_frame && self.last_serial != Some(published.serial);

            // Make sure the output texture exists for this size and is known to egui before
            // the UI references it.
            let (view, generation) = {
                let mut egui_renderer = render_state.renderer.write();
                let Some(vr) = egui_renderer.callback_resources.get_mut::<VideoRenderer>() else {
                    return;
                };
                if let Err(e) = vr.ensure_targets(published.frame.width, published.frame.height) {
                    eprintln!("ensure_targets: {e}");
                    return;
                }
                self.source_size = vr.source_size().unwrap_or((0, 0));
                self.output_size = vr.output_size().unwrap_or((0, 0));
                self.upscaled = vr.is_upscaled();
                self.upscaler_label = vr.upscaler_label();
                (vr.display_view().cloned(), vr.output_generation())
            };
            if let Some(view) = view {
                let mut egui_renderer = render_state.renderer.write();
                self.texture
                    .sync(&mut egui_renderer, &render_state.device, &view, generation);
            }

            if is_new {
                self.last_serial = Some(published.serial);
                let now = Instant::now();
                if let Some(prev) = self.last_present
                    && let Ok(mut s) = self.stats.lock()
                {
                    s.frame_interval
                        .push(now.saturating_duration_since(prev).as_secs_f64() * 1e3);
                }
                self.last_present = Some(now);
                self.present_times.push_back(now);
                while self.present_times.len() > 120 {
                    self.present_times.pop_front();
                }
                self.presented += 1;
            }

            let stats_snapshot = self.stats.lock().ok().map(|s| HudLine::from_stats(&s));

            egui::CentralPanel::default()
                .frame(egui::Frame::NONE.fill(egui::Color32::BLACK))
                .show(ui, |ui| {
                    let available = ui.max_rect();
                    let video_rect =
                        xcloud_gpu::letterbox(available, self.output_size.0, self.output_size.1);
                    if is_new {
                        ui.painter().add(egui_wgpu::Callback::new_paint_callback(
                            video_rect,
                            VideoCallback {
                                frame: Arc::clone(&published.frame),
                                stats: Arc::clone(&self.stats),
                            },
                        ));
                    }
                    self.texture.draw(
                        ui.painter(),
                        available,
                        self.output_size.0,
                        self.output_size.1,
                    );

                    // HUD, like StreamHUD.qml but with the per-stage latencies.
                    let fps = self.fps();
                    egui::Area::new(egui::Id::unique("hud"))
                        .fixed_pos(available.min + egui::vec2(12.0, 12.0))
                        .show(&ctx, |ui| {
                            egui::Frame::popup(ui.style())
                                .fill(egui::Color32::from_black_alpha(190))
                                .show(ui, |ui| {
                                    ui.style_mut().override_text_style =
                                        Some(egui::TextStyle::Monospace);
                                    // "1440p → 2160p FX" like StreamHUD.qml; no suffix
                                    // when the frame is shown at source size.
                                    ui.label(
                                        format!(
                                            "{}x{} → {}x{} {}",
                                            self.source_size.0,
                                            self.source_size.1,
                                            self.output_size.0,
                                            self.output_size.1,
                                            self.upscaler_label
                                        )
                                        .trim_end(),
                                    );
                                    if let Some(h) = &stats_snapshot {
                                        ui.label(format!("import: {}", h.import));
                                        ui.label(format!(
                                            "present: {}   fps: {fps:.1}",
                                            self.present_label
                                        ));
                                        ui.label(format!(
                                            "decode: {:.2} ms   prepare(cpu): {:.2} ms",
                                            h.decode_ms, h.prepare_ms
                                        ));
                                        ui.label(format!("GPU: {}", h.gpu));
                                        ui.label(format!(
                                            "frame age @prepare: {:.2} ms   @gpu-done: {}",
                                            h.age_prepare_ms, h.age_done
                                        ));
                                        ui.label(format!(
                                            "frames {}/{}   in-flight {}   stalls {}",
                                            self.presented,
                                            self.target_frames,
                                            h.in_flight,
                                            h.stalls
                                        ));
                                    }
                                });
                        });
                });

            if self.presented >= self.target_frames {
                self.report();
                self.stop.store(true, Ordering::Relaxed);
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
        }

        fn on_exit(&mut self) {
            self.stop.store(true, Ordering::Relaxed);
            if let Some(handle) = self.decoder_thread.take() {
                let _ = handle.join();
            }
            self.report();
        }
    }

    /// Human name for a renderer label.
    fn upscaler_name(label: &str) -> &'static str {
        match label {
            "FSR" => "FSR 1.0",
            "FX" => "MetalFX spatial",
            _ => "bypass",
        }
    }

    /// Pre-formatted HUD values (so the lock is held briefly, outside the UI closure).
    struct HudLine {
        import: String,
        decode_ms: f64,
        prepare_ms: f64,
        gpu: String,
        age_prepare_ms: f64,
        age_done: String,
        in_flight: usize,
        stalls: u64,
    }

    impl HudLine {
        fn from_stats(s: &Stats) -> Self {
            let import = match (&s.zero_copy_reason, s.last_zero_copy) {
                (None, true) => "zero-copy (IOSurface → CVMetalTexture → wgpu)".to_string(),
                (None, false) => "CPU upload (write_texture)".to_string(),
                (Some(reason), _) => format!("CPU upload, zero-copy disabled: {reason}"),
            };
            let gpu = match s.last_gpu {
                Some(t) if t.metalfx_ms > 0.0 => format!(
                    "{:.3} ms (nv12 {:.3} / metalfx {:.3})",
                    t.total_ms(),
                    t.convert_ms,
                    t.metalfx_ms
                ),
                Some(t) => format!(
                    "{:.3} ms (nv12 {:.3} / easu {:.3} / rcas {:.3})",
                    t.total_ms(),
                    t.convert_ms,
                    t.easu_ms,
                    t.rcas_ms
                ),
                None if s.timestamps_supported => "pending…".to_string(),
                None => "n/a (no TIMESTAMP_QUERY)".to_string(),
            };
            Self {
                import,
                decode_ms: s.decode.last().copied().unwrap_or(0.0),
                prepare_ms: s.last_prepare_ms,
                gpu,
                age_prepare_ms: s.last_age_prepare_ms,
                age_done: s
                    .last_age_done_ms
                    .map_or("pending…".to_string(), |ms| format!("{ms:.2} ms")),
                in_flight: s.in_flight,
                stalls: s.stalls,
            }
        }
    }

    /// Decode thread: paces the access units at 60 fps forever, publishes each decoded
    /// frame into the triple buffer and wakes egui.
    fn decode_loop(
        aus: Vec<Vec<u8>>,
        mut input: triple_buffer::Input<Option<Published>>,
        ctx: egui::Context,
        stop: Arc<AtomicBool>,
    ) {
        let mut serial = 0u64;
        let sink: FrameSink = Box::new(move |frame| {
            serial += 1;
            input.write(Some(Published {
                serial,
                frame: Arc::new(frame),
            }));
            ctx.request_repaint();
        });
        let mut decoder = VtDecoder::new(VtConfig::default(), sink);
        let start = Instant::now();
        let mut i = 0u64;
        while !stop.load(Ordering::Relaxed) {
            let au = &aus[(i % aus.len() as u64) as usize];
            let due = start + Duration::from_secs_f64(i as f64 / f64::from(FPS));
            pace_until(due);
            if let Err(e) = decoder.decode(au, Instant::now()) {
                eprintln!("decode AU {i}: {e}");
            }
            i += 1;
        }
        decoder.flush();
    }

    /// Sleeps coarsely, then spins for the last stretch so the pacing is tight.
    fn pace_until(due: Instant) {
        loop {
            let now = Instant::now();
            if now >= due {
                return;
            }
            let remaining = due - now;
            if remaining > Duration::from_micros(1500) {
                std::thread::sleep(remaining - Duration::from_micros(1000));
            } else {
                std::hint::spin_loop();
            }
        }
    }

    pub fn main() -> Result<(), Box<dyn std::error::Error>> {
        let args = parse_args()?;
        let stream = std::fs::read(&args.path).map_err(|e| format!("{}: {e}", args.path))?;
        let aus: Vec<Vec<u8>> = split_access_units(&stream)
            .into_iter()
            .map(<[u8]>::to_vec)
            .collect();
        if aus.is_empty() {
            return Err("no access units in the input".into());
        }
        println!(
            "{}: {} bytes, {} access units; present={:?} frames={} upscaler={} (metalfx compiled: {})",
            args.path,
            stream.len(),
            aus.len(),
            args.present,
            args.frames,
            args.upscaler.as_str(),
            xcloud_gpu::metalfx::COMPILED
        );

        let mut wgpu_options = egui_wgpu::WgpuConfiguration {
            surface: egui_wgpu::SurfaceConfig {
                present_mode: args.present,
                desired_maximum_frame_latency: Some(1),
            },
            ..Default::default()
        };
        if let egui_wgpu::WgpuSetup::CreateNew(setup) = &mut wgpu_options.wgpu_setup {
            // Ask for timestamp queries when the adapter has them; keep egui's limits.
            let base = Arc::clone(&setup.device_descriptor);
            setup.device_descriptor = Arc::new(move |adapter| {
                let mut desc = base(adapter);
                desc.required_features |= adapter.features() & wgpu::Features::TIMESTAMP_QUERY;
                desc
            });
        }
        let options = eframe::NativeOptions {
            viewport: egui::ViewportBuilder::default()
                .with_title("xcloud-gpu replay")
                .with_inner_size([1280.0, 760.0]),
            renderer: eframe::Renderer::Wgpu,
            wgpu_options,
            ..Default::default()
        };
        eframe::run_native(
            "xcloud-gpu replay",
            options,
            Box::new(move |cc| {
                ReplayApp::new(cc, &args, aus)
                    .map(|app| Box::new(app) as Box<dyn eframe::App>)
                    .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> { e.into() })
            }),
        )?;
        Ok(())
    }
}
