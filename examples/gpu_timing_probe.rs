//! Bounded functional probe for opt-in GPU timing on an imported DMA-BUF.
//!
//! This creates only offscreen 64x64 synthetic buffers. It neither connects to
//! a compositor nor reads any desktop or game pixels. Its timings are not a
//! gaming benchmark. Set `SMITHAY_GPU_TIMING` to an existing private directory
//! containing `enable` before running it to exercise the timing collector.

use std::{
    error::Error,
    fs::File,
    io,
    os::fd::OwnedFd,
    path::{Path, PathBuf},
    thread,
    time::Duration,
};

use clap::Parser;
use smithay::{
    backend::{
        allocator::{
            Allocator, Buffer, Fourcc,
            dmabuf::DmabufAllocator,
            gbm::{GbmAllocator, GbmBufferFlags, GbmDevice},
        },
        drm::DrmDeviceFd,
        egl::{EGLContext, EGLDevice, EGLDisplay},
        renderer::{
            Bind, Color32F, ExportMem, Frame, ImportDma, Offscreen, Renderer,
            gles::{GlesRenderbuffer, GlesRenderer},
        },
    },
    utils::{DeviceFd, Rectangle, Transform},
};

#[derive(Debug, Parser)]
#[command(about = "Exercise GPU timing with a synthetic offscreen DMA-BUF")]
struct Args {
    /// DRM render node that allocates and renders the source buffer.
    #[arg(long)]
    source: PathBuf,
    /// DRM render node that imports and samples the source buffer.
    #[arg(long)]
    target: PathBuf,
    /// Use the source allocator's GBM device as the source EGL native display.
    #[arg(long)]
    gbm_source: bool,
    /// Number of tiny target draws; deliberately bounded, not a benchmark.
    #[arg(long, default_value_t = 24, value_parser = clap::value_parser!(u16).range(1..=256))]
    frames: u16,
}

fn renderer(path: &Path) -> Result<(EGLDisplay, GlesRenderer), Box<dyn Error>> {
    let device = EGLDevice::enumerate()?
        .find(|device| {
            device.drm_device_path().ok().as_deref() == Some(path)
                || device.render_device_path().ok().as_deref() == Some(path)
        })
        .ok_or_else(|| io::Error::other(format!("No EGL device for {}", path.display())))?;
    // The new context is used only on this thread, and no foreign EGL context
    // is wrapped or concurrently accessed.
    let display = unsafe { EGLDisplay::new(device)? };
    let context = EGLContext::new(&display)?;
    let renderer = unsafe { GlesRenderer::new(context)? };
    Ok((display, renderer))
}

fn main() -> Result<(), Box<dyn Error>> {
    tracing_subscriber::fmt::init();
    let args = Args::parse();
    let size = (64, 64).into();
    let full = [Rectangle::from_size(size)];
    let source_file = File::options().read(true).write(true).open(&args.source)?;
    let source_fd = DrmDeviceFd::new(DeviceFd::from(OwnedFd::from(source_file)));
    let gbm = GbmDevice::new(source_fd)?;
    let (source_display, mut source) = if args.gbm_source {
        // Keep the same GBM native device alive for both EGL and allocation;
        // some drivers distinguish this platform from EGL_PLATFORM_DEVICE.
        let display = unsafe { EGLDisplay::new(gbm.clone())? };
        let context = EGLContext::new(&display)?;
        let renderer = unsafe { GlesRenderer::new(context)? };
        (display, renderer)
    } else {
        renderer(&args.source)?
    };
    let (target_display, mut target) = renderer(&args.target)?;
    let mut allocator = DmabufAllocator(GbmAllocator::new(gbm, GbmBufferFlags::RENDERING));

    let candidates = source_display
        .dmabuf_render_formats()
        .intersection(target_display.dmabuf_texture_formats())
        .filter(|format| {
            matches!(
                format.code,
                Fourcc::Abgr8888 | Fourcc::Argb8888 | Fourcc::Xbgr8888 | Fourcc::Xrgb8888
            )
        })
        .copied()
        .collect::<Vec<_>>();
    const MAX_CANDIDATES: usize = 64;
    let candidate_count = candidates.len();
    tracing::info!(
        candidate_count,
        MAX_CANDIDATES,
        "Testing shared format candidates"
    );
    let mut last_error = None;
    let mut prepared = None;
    for (index, format) in candidates.into_iter().take(MAX_CANDIDATES).enumerate() {
        tracing::info!(index, ?format, "Trying requested source format");
        let mut stage = "allocation";
        let result: Result<_, Box<dyn Error>> = (|| {
            let mut buffer = allocator.create_buffer(64, 64, format.code, &[format.modifier])?;
            tracing::info!(index, actual_format = ?buffer.format(), "Source allocation succeeded");
            stage = "source bind";
            {
                let mut framebuffer = source.bind(&mut buffer)?;
                stage = "source render";
                let mut frame = source.render(&mut framebuffer, size, Transform::Normal)?;
                frame.clear(Color32F::new(0.0, 1.0, 0.0, 1.0), &full)?;
                // Complete synthetic setup before the target samples it. This
                // setup wait is not a producer-dependency timing measurement.
                frame.finish()?.wait()?;
            }
            stage = "target import";
            let texture = target.import_dmabuf(&buffer, None)?;
            Ok((buffer, texture))
        })();
        match result {
            Ok(result) => {
                prepared = Some(result);
                break;
            }
            Err(error) => {
                tracing::warn!(index, ?format, stage, %error, "Shared format candidate failed");
                last_error = Some(format!("{stage}: {error}"));
                source.cleanup_texture_cache()?;
                target.cleanup_texture_cache()?;
            }
        }
    }
    let (buffer, texture) = prepared.ok_or_else(|| {
        io::Error::other(format!(
            "No usable shared 32-bit RGB format in {} candidates (of {} advertised): {}",
            candidate_count.min(MAX_CANDIDATES),
            candidate_count,
            last_error.as_deref().unwrap_or("empty format intersection")
        ))
    })?;
    let mut offscreen: GlesRenderbuffer = target.create_buffer(Fourcc::Abgr8888, (64, 64).into())?;
    let mut last_sync = None;
    for _ in 0..args.frames {
        let mut framebuffer = target.bind(&mut offscreen)?;
        let mut frame = target.render(&mut framebuffer, size, Transform::Normal)?;
        frame.render_texture_at(
            &texture,
            (0, 0).into(),
            1,
            1.0,
            Transform::Normal,
            &full,
            &full,
            1.0,
        )?;
        last_sync = Some(frame.finish()?);
        // Give availability polling an opportunity without forcing GPU-query
        // completion or running a sustained competing GPU workload.
        thread::sleep(Duration::from_millis(2));
    }
    if let Some(sync) = last_sync {
        sync.wait()?;
    }
    for _ in 0..10 {
        target.cleanup_texture_cache()?;
        thread::sleep(Duration::from_millis(2));
    }

    // Validate one synthetic pixel, not a screen capture. A green pixel has
    // identical channel positions in both supported 32-bit RGB byte layouts.
    let framebuffer = target.bind(&mut offscreen)?;
    let mapping = target.copy_framebuffer(
        &framebuffer,
        Rectangle::from_size((1, 1).into()),
        Fourcc::Abgr8888,
    )?;
    let pixel = target.map_texture(&mapping)?;
    if pixel.len() < 4 || pixel[0] > 2 || pixel[1] < 253 || pixel[2] > 2 || pixel[3] < 253 {
        return Err(io::Error::other("Synthetic imported pixel did not match green").into());
    }
    tracing::info!(
        source = %args.source.display(),
        target = %args.target.display(),
        format = ?buffer.format(),
        frames = args.frames,
        "GPU timing probe passed: synthetic pixel is green"
    );
    drop(mapping);
    drop(framebuffer);
    drop(offscreen);
    drop(texture);
    target.cleanup_texture_cache()?;
    drop(target);
    drop(source);
    // The diagnostic writer is intentionally asynchronous. Give the tiny
    // probe's queued records a bounded chance to drain after sender teardown;
    // production renderer destruction remains nonblocking.
    thread::sleep(Duration::from_millis(100));
    Ok(())
}
