// SPDX-License-Identifier: GPL-3.0-or-later
//! Nested Winit Development Backend for Mochi Mochi
//!
//! Composes the full Mochi Mochi scene graph using OpenGL ES (`GlesRenderer`),
//! offscreen FBO multi-pass background blur (`OffscreenBackdropFbo`),
//! `MochiShaderPipeline` (`mochi-render`), `BackdropEffectNode` (`mochi-scene`),
//! physical spring animations (`mochi-anim`), and `OutputDamageTracker`.

use std::time::{Duration, Instant};

use calloop::EventLoop;
use mochi_core::MochiState;
use mochi_render::MochiShaderPipeline;
use smithay::{
    backend::{
        renderer::{damage::OutputDamageTracker, gles::GlesRenderer, utils::CommitCounter, ImportDma},
        winit::{self, WinitEvent},
    },
    desktop::layer_map_for_output,
    output::{Mode, Output, PhysicalProperties, Subpixel},
    utils::{Rectangle, Transform},
};
use tracing::{info, warn};

use crate::{assemble_scene_elements, capture_framebuffer_to_ppm, OffscreenBackdropFbo, OverlayIds};

/// Initializes and runs the nested Winit backend loop.
pub fn run_winit(
    event_loop: &mut EventLoop<'static, MochiState>,
    state: &mut MochiState,
    initial_command: Option<&str>,
) -> anyhow::Result<()> {
    let (mut backend, winit_evt_loop) = winit::init::<GlesRenderer>()
        .map_err(|err| anyhow::anyhow!("Failed to initialize Smithay Winit backend: {err:?}"))?;

    backend.window().set_title(&format!(
        "Mochi Mochi — Workspace {} ({}) [{}]",
        state.wm.active_workspace_number(),
        state.wm.active_layout().as_str(),
        state.wm.visual_style.style.as_str()
    ));

    // Compile Mochi Mochi GLSL shader pipeline on the active GlesRenderer
    let shaders = MochiShaderPipeline::compile(backend.renderer())?;

    // Advertise hardware-accelerated DMABUF formats supported by GlesRenderer
    let dmabuf_formats = backend.renderer().dmabuf_formats();
    let dmabuf_global = state
        .dmabuf_state
        .create_global::<MochiState>(&state.display_handle, dmabuf_formats);
    state.dmabuf_global = Some(dmabuf_global);

    let size = backend.window_size();
    let mode = Mode {
        size,
        refresh: 60_000,
    };

    let output_name = "mochi-winit-0";
    let (out_loc, out_scale) = state
        .wm
        .configured_output_placement(output_name, (0, 0).into());

    let output = Output::new(
        output_name.to_string(),
        PhysicalProperties {
            size: (0, 0).into(),
            subpixel: Subpixel::Unknown,
            make: "Mochi Mochi".into(),
            model: "Nested Winit Output".into(),
        },
    );
    let _global = output.create_global::<MochiState>(&state.display_handle);
    output.change_current_state(
        Some(mode),
        Some(Transform::Flipped180),
        out_scale.map(smithay::output::Scale::Fractional),
        Some(out_loc),
    );
    output.set_preferred(mode);

    state.wm.map_output(&output, out_loc);

    let mut damage_tracker = OutputDamageTracker::from_output(&output);
    let mut backdrop_fbo = OffscreenBackdropFbo::new();
    let overlay_ids = OverlayIds::new();
    let mut frame_commit = CommitCounter::default();
    let mut frame_sequence: u64 = 0;
    let mut last_frame_instant = Instant::now();

    std::env::set_var("WAYLAND_DISPLAY", &state.socket_name);
    info!(
        wayland_display = ?state.socket_name,
        ipc_socket = %state.ipc_socket_path.display(),
        resolution = ?size,
        style = state.wm.visual_style.style.as_str(),
        layout = state.wm.active_layout().as_str(),
        "Mochi Mochi nested Winit backend is ready (Offscreen FBO Blur + Hybrid Tiling + Live IPC)"
    );

    state.run_startup_commands();

    if let Some(cmd) = initial_command {
        state.spawn_client_command(cmd);
    }

    event_loop
        .handle()
        .insert_source(winit_evt_loop, move |event, _, state| match event {
            WinitEvent::Resized { size, .. } => {
                output.change_current_state(
                    Some(Mode {
                        size,
                        refresh: 60_000,
                    }),
                    None,
                    None,
                    None,
                );
                layer_map_for_output(&output).arrange();
                state.wm.retile_active_workspace();
            }
            WinitEvent::Input(event) => {
                let prev_ws = state.wm.active_workspace_number();
                let prev_style = state.wm.visual_style.style;
                let prev_layout = state.wm.active_layout();
                state.process_input_event(event);
                if state.wm.active_workspace_number() != prev_ws
                    || state.wm.visual_style.style != prev_style
                    || state.wm.active_layout() != prev_layout
                {
                    backend.window().set_title(&format!(
                        "Mochi Mochi — Workspace {} ({}) [{}]",
                        state.wm.active_workspace_number(),
                        state.wm.active_layout().as_str(),
                        state.wm.visual_style.style.as_str()
                    ));
                }
            }
            WinitEvent::Redraw => {
                let now = Instant::now();
                let dt_secs = now.duration_since(last_frame_instant).as_secs_f64();
                last_frame_instant = now;

                // Advance all physical springs (window snap/tile/open, workspace slide, visual style morph, night light)
                state.tick_frame(dt_secs);

                let size = backend.window_size();
                let damage = Rectangle::from_size(size);
                frame_commit.increment();

                // Pass 1: Render background windows + wallpaper into Offscreen FBO texture
                let fbo_tex = backdrop_fbo.render_backdrop_pass(
                    backend.renderer(),
                    state,
                    &output,
                    &shaders,
                );

                // Pass 2: Bind primary output surface and composite full scene with FBO blur/refraction
                {
                    let (renderer, mut framebuffer) = backend.bind().unwrap();
                    let clear_color = state.wm.visual_style.style.backdrop_clear_color();
                    let scene_elements = assemble_scene_elements(
                        renderer,
                        state,
                        &output,
                        &shaders,
                        fbo_tex.as_ref(),
                        &overlay_ids,
                        frame_commit,
                    );

                    if let Err(err) = damage_tracker.render_output(
                        renderer,
                        &mut framebuffer,
                        0,
                        &scene_elements,
                        clear_color,
                    ) {
                        warn!(?err, "Error rendering Mochi Mochi scene frame");
                    }

                    if let Some(shot_path) = state.pending_screenshot_path.take() {
                        if let Err(err) =
                            capture_framebuffer_to_ppm(renderer, &framebuffer, size, &shot_path)
                        {
                            warn!(?err, path = %shot_path.display(), "Failed to capture screenshot");
                        }
                    }

                    // Process pending wlr-screencopy captures (grim, slurp, OBS, PipeWire)
                    mochi_core::screencopy::process_screencopy_for_output(
                        renderer,
                        &framebuffer,
                        &output,
                        size,
                        &mut state.screencopy_state,
                        state.session_locked,
                    );
                }

                if let Err(err) = backend.submit(Some(&[damage])) {
                    warn!(?err, "Error submitting Winit frame");
                }

                frame_sequence = frame_sequence.wrapping_add(1);
                state.send_frames_and_presentation_feedback(
                    &output,
                    frame_sequence,
                    Duration::from_nanos(16_666_667),
                );

                state.wm.active_space_mut().refresh();
                state.popups.cleanup();
                let _ = state.display_handle.flush_clients();

                backend.window().request_redraw();
            }
            WinitEvent::CloseRequested => {
                info!("Winit host window closed; stopping Mochi Mochi");
                state.running = false;
                state.loop_signal.stop();
            }
            _ => {}
        })
        .map_err(|err| anyhow::anyhow!("Failed to insert Winit event source: {err:?}"))?;

    while state.running {
        event_loop.dispatch(Duration::from_millis(16), state)?;
    }

    Ok(())
}
