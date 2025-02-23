// SPDX-License-Identifier: GPL-3.0-only

#[cfg(feature = "debug")]
use crate::backend::render::element::AsGlowRenderer;
use crate::{
    backend::render::{workspace_elements, CLEAR_COLOR},
    config::OutputConfig,
    shell::Shell,
    state::{BackendData, ClientState, Common, Fps, SurfaceDmabufFeedback},
    utils::prelude::*,
    wayland::{
        handlers::screencopy::{render_session, UserdataExt},
        protocols::screencopy::{BufferParams, Session as ScreencopySession},
    },
};

use anyhow::{Context, Result};
use cosmic_protocols::screencopy::v1::server::zcosmic_screencopy_session_v1::FailureReason;
use libc::dev_t;
use smithay::{
    backend::{
        allocator::{
            dmabuf::{AnyError, Dmabuf, DmabufAllocator},
            gbm::{GbmAllocator, GbmBufferFlags, GbmDevice},
            Allocator, Format, Fourcc,
        },
        drm::{
            compositor::{BlitFrameResultError, DrmCompositor, FrameError, PrimaryPlaneElement},
            DrmDevice, DrmDeviceFd, DrmEvent, DrmEventTime, DrmNode, NodeType,
        },
        egl::{EGLContext, EGLDevice, EGLDisplay},
        input::InputEvent,
        libinput::{LibinputInputBackend, LibinputSessionInterface},
        renderer::{
            buffer_dimensions,
            damage::{Error as RenderError, RenderOutputResult},
            element::Element,
            gles::GlesRenderbuffer,
            glow::GlowRenderer,
            multigpu::{gbm::GbmGlesBackend, Error as MultiError, GpuManager},
            sync::SyncPoint,
            Bind, ImportDma, Offscreen,
        },
        session::{libseat::LibSeatSession, Event as SessionEvent, Session},
        udev::{all_gpus, primary_gpu, UdevBackend, UdevEvent},
    },
    desktop::utils::OutputPresentationFeedback,
    input::Seat,
    output::{Mode as OutputMode, Output, OutputNoMode, PhysicalProperties, Subpixel},
    reexports::{
        calloop::{
            timer::{TimeoutAction, Timer},
            Dispatcher, EventLoop, InsertError, LoopHandle, RegistrationToken,
        },
        drm::{
            control::{connector, crtc, Device as ControlDevice, ModeTypeFlags},
            Device as _,
        },
        input::{self, Libinput},
        rustix::fs::OFlags,
        wayland_protocols::wp::{
            linux_dmabuf::zv1::server::zwp_linux_dmabuf_feedback_v1,
            presentation_time::server::wp_presentation_feedback,
        },
        wayland_server::{protocol::wl_surface::WlSurface, DisplayHandle, Resource},
    },
    utils::{DeviceFd, Size, Transform},
    wayland::{
        dmabuf::{get_dmabuf, DmabufFeedbackBuilder, DmabufGlobal},
        drm_lease::{DrmLease, DrmLeaseState},
        relative_pointer::RelativePointerManagerState,
        seat::WaylandFocus,
        shm::{shm_format_to_fourcc, with_buffer_contents},
    },
    xwayland::XWaylandClientData,
};
use tracing::{error, info, trace, warn};

use std::{
    cell::RefCell,
    collections::{HashMap, HashSet},
    fmt,
    path::PathBuf,
    time::Duration,
};

mod drm_helpers;
mod socket;
use socket::*;

use super::render::{init_shaders, CursorMode, GlMultiRenderer};
// for now we assume we need at least 3ms
const MIN_RENDER_TIME: Duration = Duration::from_millis(3);

#[derive(Debug)]
pub struct KmsState {
    pub devices: HashMap<DrmNode, Device>,
    pub input_devices: HashMap<String, input::Device>,
    pub api: GpuManager<GbmGlesBackend<GlowRenderer>>,
    pub primary: DrmNode,
    session: LibSeatSession,
    _tokens: Vec<RegistrationToken>,
}

pub struct Device {
    render_node: DrmNode,
    surfaces: HashMap<crtc::Handle, Surface>,
    pub drm: DrmDevice,
    gbm: GbmDevice<DrmDeviceFd>,
    allocator: Box<dyn Allocator<Buffer = Dmabuf, Error = AnyError>>,
    formats: HashSet<Format>,
    supports_atomic: bool,
    pub non_desktop_connectors: Vec<(connector::Handle, crtc::Handle)>,
    pub leasing_global: Option<DrmLeaseState>,
    pub active_leases: Vec<DrmLease>,
    event_token: Option<RegistrationToken>,
    socket: Option<Socket>,
}

impl fmt::Debug for Device {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Device")
            .field("render_node", &self.render_node)
            .field("surfaces", &self.surfaces)
            .field("drm", &self.drm)
            .field("gbm", &self.gbm)
            .field("allocator", &"...")
            .field("formats", &self.formats)
            .field("supports_atomic", &self.supports_atomic)
            .field("non_desktop_connectors", &self.non_desktop_connectors)
            .field("leasing_global", &self.leasing_global)
            .field("active_leases", &self.active_leases)
            .field("event_token", &self.event_token)
            .field("socket", &self.socket)
            .finish()
    }
}

#[derive(Debug)]
pub struct Surface {
    surface: Option<GbmDrmCompositor>,
    connector: connector::Handle,
    output: Output,
    refresh_rate: u32,
    vrr: bool,
    scheduled: bool,
    pending: bool,
    dirty: bool,
    last_animation_state: bool,
    render_timer_token: Option<RegistrationToken>,
    fps: Fps,
    feedback: HashMap<DrmNode, SurfaceDmabufFeedback>,
}

pub type GbmDrmCompositor = DrmCompositor<
    GbmAllocator<DrmDeviceFd>,
    GbmDevice<DrmDeviceFd>,
    Option<OutputPresentationFeedback>,
    DrmDeviceFd,
>;

pub fn init_backend(
    dh: &DisplayHandle,
    event_loop: &mut EventLoop<'static, State>,
    state: &mut State,
) -> Result<()> {
    let (session, notifier) = LibSeatSession::new().context("Failed to acquire session")?;

    let udev_backend = UdevBackend::new(session.seat())?;
    let mut libinput_context =
        Libinput::new_with_udev::<LibinputSessionInterface<LibSeatSession>>(session.clone().into());
    libinput_context
        .udev_assign_seat(&session.seat())
        .map_err(|_| anyhow::anyhow!("Failed to assign seat to libinput"))?;
    let libinput_backend = LibinputInputBackend::new(libinput_context.clone());

    let libinput_event_source = event_loop
        .handle()
        .insert_source(libinput_backend, move |mut event, _, state| {
            if let InputEvent::DeviceAdded { ref mut device } = &mut event {
                state.common.config.read_device(device);
                state
                    .backend
                    .kms()
                    .input_devices
                    .insert(device.name().into(), device.clone());
            } else if let InputEvent::DeviceRemoved { device } = &event {
                state.backend.kms().input_devices.remove(device.name());
            }
            state.process_input_event(event, true);
            for output in state.common.shell.outputs() {
                if let Err(err) = state.backend.kms().schedule_render(
                    &state.common.event_loop_handle,
                    output,
                    None,
                    None,
                ) {
                    error!(
                        ?err,
                        "Error scheduling event loop for output {}.",
                        output.name(),
                    );
                }
            }
        })
        .map_err(|err| err.error)
        .context("Failed to initialize libinput event source")?;
    let api = GpuManager::new(GbmGlesBackend::<GlowRenderer>::default())
        .context("Failed to initialize renderers")?;

    // TODO get this info from system76-power, if available and setup a watcher
    let primary = if let Some(path) = std::env::var("COSMIC_RENDER_DEVICE")
        .ok()
        .and_then(|x| DrmNode::from_path(x).ok())
    {
        path
    } else {
        let primary_node = primary_gpu(session.seat())
            .ok()
            .flatten()
            .and_then(|x| DrmNode::from_path(x).ok());
        primary_node
            .and_then(|x| x.node_with_type(NodeType::Render).and_then(Result::ok))
            .unwrap_or_else(|| {
                for dev in all_gpus(session.seat()).expect("No GPU found") {
                    if let Some(node) = DrmNode::from_path(dev)
                        .ok()
                        .and_then(|x| x.node_with_type(NodeType::Render).and_then(Result::ok))
                    {
                        return node;
                    }
                }
                // If we find no render nodes, use primary node
                if let Some(primary_node) = primary_node {
                    return primary_node;
                } else {
                    panic!("Failed to initialize any GPU");
                }
            })
    };
    info!("Using {} as primary gpu for rendering.", primary);

    let udev_dispatcher = Dispatcher::new(udev_backend, move |event, _, state: &mut State| {
        let dh = state.common.display_handle.clone();
        match match event {
            UdevEvent::Added { device_id, path } => state
                .device_added(device_id, path, &dh)
                .with_context(|| format!("Failed to add drm device: {}", device_id)),
            UdevEvent::Changed { device_id } => state
                .device_changed(device_id)
                .with_context(|| format!("Failed to update drm device: {}", device_id)),
            UdevEvent::Removed { device_id } => state
                .device_removed(device_id, &dh)
                .with_context(|| format!("Failed to remove drm device: {}", device_id)),
        } {
            Ok(()) => {
                trace!("Successfully handled udev event.")
            }
            Err(err) => {
                error!(?err, "Error while handling udev event.")
            }
        }
    });
    let udev_event_source = event_loop
        .handle()
        .register_dispatcher(udev_dispatcher.clone())
        .unwrap();

    let handle = event_loop.handle();
    let loop_signal = state.common.event_loop_signal.clone();
    let dispatcher = udev_dispatcher.clone();
    let session_event_source = event_loop
        .handle()
        .insert_source(notifier, move |event, &mut (), state| match event {
            SessionEvent::ActivateSession => {
                if let Err(err) = libinput_context.resume() {
                    error!(?err, "Failed to resume libinput context.");
                }
                for device in state.backend.kms().devices.values_mut() {
                    if let Err(err) = device.drm.activate(true) {
                        error!(?err, "Failed to resume drm device");
                    }
                    // TODO save state, do the disable part manually, etc
                    if let Some(lease_state) = device.leasing_global.as_mut() {
                        lease_state.resume::<State>();
                    }
                }
                let dispatcher = dispatcher.clone();
                handle.insert_idle(move |state| {
                    for (dev, path) in dispatcher.as_source_ref().device_list() {
                        let drm_node = match DrmNode::from_dev_id(dev) {
                            Ok(node) => node,
                            Err(err) => {
                                error!(?err, "Failed to read drm device {}.", path.display(),);
                                continue;
                            }
                        };
                        if state.backend.kms().devices.contains_key(&drm_node) {
                            if let Err(err) = state.device_changed(dev) {
                                error!(?err, "Failed to update drm device {}.", path.display(),);
                            }
                        } else {
                            let dh = state.common.display_handle.clone();
                            if let Err(err) = state.device_added(dev, path.into(), &dh) {
                                error!(?err, "Failed to add drm device {}.", path.display(),);
                            }
                        }
                    }

                    let seats = state.common.seats().cloned().collect::<Vec<_>>();
                    state.common.config.read_outputs(
                        &mut state.common.output_configuration_state,
                        &mut state.backend,
                        &mut state.common.shell,
                        seats.into_iter(),
                        &state.common.event_loop_handle,
                    );
                    for surface in state
                        .backend
                        .kms()
                        .devices
                        .values_mut()
                        .flat_map(|d| d.surfaces.values_mut())
                    {
                        surface.scheduled = false;
                        surface.pending = false;
                    }
                    for output in state.common.shell.outputs() {
                        let sessions = output.pending_buffers().collect::<Vec<_>>();
                        if let Err(err) = state.backend.kms().schedule_render(
                            &state.common.event_loop_handle,
                            output,
                            None,
                            if !sessions.is_empty() {
                                Some(sessions)
                            } else {
                                None
                            },
                        ) {
                            error!(
                                ?err,
                                "Error scheduling event loop for output {}.",
                                output.name(),
                            );
                        }
                    }
                });
                loop_signal.wakeup();
            }
            SessionEvent::PauseSession => {
                libinput_context.suspend();
                for device in state.backend.kms().devices.values_mut() {
                    device.drm.pause();
                    if let Some(lease_state) = device.leasing_global.as_mut() {
                        lease_state.suspend();
                    }
                    for surface in device.surfaces.values_mut() {
                        surface.surface = None;
                        if let Some(token) = surface.render_timer_token.take() {
                            state.common.event_loop_handle.remove(token);
                        }
                        surface.scheduled = false;
                    }
                }
            }
        })
        .map_err(|err| err.error)
        .context("Failed to initialize session event source")?;

    state.backend = BackendData::Kms(KmsState {
        api,
        _tokens: vec![
            libinput_event_source,
            session_event_source,
            udev_event_source,
        ],
        primary,
        session,
        devices: HashMap::new(),
        input_devices: HashMap::new(),
    });

    // Create relative pointer global
    RelativePointerManagerState::new::<State>(&dh);

    state.launch_xwayland(Some(primary));

    for (dev, path) in udev_dispatcher.as_source_ref().device_list() {
        if let Err(err) = state.device_added(dev, path.into(), dh) {
            warn!("Failed to add device {}: {:?}", path.display(), err);
        }
    }

    Ok(())
}

impl State {
    fn device_added(&mut self, dev: dev_t, path: PathBuf, dh: &DisplayHandle) -> Result<()> {
        if !self.backend.kms().session.is_active() {
            return Ok(());
        }

        let fd = DrmDeviceFd::new(DeviceFd::from(
            self.backend
                .kms()
                .session
                .open(
                    &path,
                    OFlags::RDWR | OFlags::CLOEXEC | OFlags::NOCTTY | OFlags::NONBLOCK,
                )
                .with_context(|| {
                    format!(
                        "Failed to optain file descriptor for drm device: {}",
                        path.display()
                    )
                })?,
        ));
        let (drm, notifier) = DrmDevice::new(fd.clone(), false)
            .with_context(|| format!("Failed to initialize drm device for: {}", path.display()))?;
        let drm_node = DrmNode::from_dev_id(dev)?;
        let supports_atomic = drm.is_atomic();

        let gbm = GbmDevice::new(fd)
            .with_context(|| format!("Failed to initialize GBM device for {}", path.display()))?;
        let (render_node, formats) = {
            let egl_display = unsafe { EGLDisplay::new(gbm.clone()) }.with_context(|| {
                format!("Failed to create EGLDisplay for device: {}", path.display())
            })?;
            let egl_device = EGLDevice::device_for_display(&egl_display).with_context(|| {
                format!("Unable to find matching egl device for {}", path.display())
            })?;
            let render_node = egl_device
                .try_get_render_node()
                .ok()
                .and_then(std::convert::identity)
                .unwrap_or(drm_node);
            let egl_context = EGLContext::new(&egl_display).with_context(|| {
                format!(
                    "Failed to create EGLContext for device {:?}:{}",
                    egl_device,
                    path.display()
                )
            })?;
            let formats = egl_context.dmabuf_texture_formats().clone();
            (render_node, formats)
            // NOTE: We need the to drop the EGL types here again,
            //  otherwise the EGLDisplay created below might share the same GBM context
        };

        let token = self
            .common
            .event_loop_handle
            .insert_source(
                notifier,
                move |event, metadata, state: &mut State| match event {
                    DrmEvent::VBlank(crtc) => {
                        let rescheduled = if let Some(device) =
                            state.backend.kms().devices.get_mut(&drm_node)
                        {
                            if let Some(surface) = device.surfaces.get_mut(&crtc) {
                                trace!(?crtc, "VBlank");
                                #[cfg(feature = "debug")]
                                surface.fps.displayed();

                                match surface.surface.as_mut().map(|x| x.frame_submitted()) {
                                    Some(Ok(feedback)) => {
                                        if let Some(mut feedback) = feedback.flatten() {
                                            let submit_time =
                                                match metadata.take().map(|data| data.time) {
                                                    Some(DrmEventTime::Monotonic(tp)) => Some(tp),
                                                    _ => None,
                                                };
                                            let seq = metadata
                                                .as_ref()
                                                .map(|metadata| metadata.sequence)
                                                .unwrap_or(0);

                                            let (clock, flags) = if let Some(tp) = submit_time {
                                                (
                                                tp.into(),
                                                wp_presentation_feedback::Kind::Vsync
                                                    | wp_presentation_feedback::Kind::HwClock
                                                    | wp_presentation_feedback::Kind::HwCompletion,
                                            )
                                            } else {
                                                (
                                                    state.common.clock.now(),
                                                    wp_presentation_feedback::Kind::Vsync,
                                                )
                                            };

                                            feedback.presented(
                                                clock,
                                                surface
                                                    .output
                                                    .current_mode()
                                                    .map(|mode| {
                                                        Duration::from_secs_f64(
                                                            1_000.0 / mode.refresh as f64,
                                                        )
                                                    })
                                                    .unwrap_or_default(),
                                                seq as u64,
                                                flags,
                                            );
                                        }

                                        surface.pending = false;
                                        let animations_going =
                                            state.common.shell.animations_going();
                                        let animation_diff = std::mem::replace(
                                            &mut surface.last_animation_state,
                                            animations_going,
                                        ) != animations_going;
                                        (surface.dirty || animations_going || animation_diff).then(
                                            || {
                                                (
                                                    surface.output.clone(),
                                                    surface.fps.avg_rendertime(5),
                                                )
                                            },
                                        )
                                    }
                                    Some(Err(err)) => {
                                        warn!(?err, "Failed to submit frame.");
                                        None
                                    }
                                    _ => None, // got disabled
                                }
                            } else {
                                None
                            }
                        } else {
                            None
                        };

                        if let Some((output, avg_rendertime)) = rescheduled {
                            let mut scheduled_sessions =
                                state.workspace_session_for_output(&output);
                            let mut output_sessions = output.pending_buffers().peekable();
                            if output_sessions.peek().is_some() {
                                scheduled_sessions
                                    .get_or_insert_with(Vec::new)
                                    .extend(output_sessions);
                            }

                            let estimated_rendertime =
                                std::cmp::max(avg_rendertime, MIN_RENDER_TIME);
                            if let Err(err) = state.backend.kms().schedule_render(
                                &state.common.event_loop_handle,
                                &output,
                                Some(estimated_rendertime),
                                scheduled_sessions,
                            ) {
                                warn!(?err, "Failed to schedule render.");
                            }
                        }
                    }
                    DrmEvent::Error(err) => {
                        warn!(?err, "Failed to read events of device {:?}.", dev);
                    }
                },
            )
            .with_context(|| format!("Failed to add drm device to event loop: {}", dev))?;

        let socket = match self.create_socket(dh, render_node, formats.clone().into_iter()) {
            Ok(socket) => Some(socket),
            Err(err) => {
                warn!(
                    ?err,
                    "Failed to initialize hardware-acceleration for clients on {}.", render_node,
                );
                None
            }
        };

        let allocator = Box::new(DmabufAllocator(GbmAllocator::new(
            gbm.clone(),
            GbmBufferFlags::RENDERING,
        )));
        let mut device = Device {
            render_node,
            surfaces: HashMap::new(),
            gbm: gbm.clone(),
            allocator,
            drm,
            formats,
            supports_atomic,
            non_desktop_connectors: Vec::new(),
            leasing_global: DrmLeaseState::new::<State>(dh, &drm_node)
                .map_err(|err| {
                    // TODO: replace with inspect_err, once stable
                    warn!(
                        ?err,
                        "Failed to initialize drm lease global for: {}", drm_node
                    );
                    err
                })
                .ok(),
            active_leases: Vec::new(),
            event_token: Some(token),
            socket,
        };

        let outputs = device.enumerate_surfaces()?.added; // There are no removed outputs on newly added devices
        let mut wl_outputs = Vec::new();
        let mut w = self.common.shell.global_space().size.w;
        {
            let backend = self.backend.kms();
            backend
                .api
                .as_mut()
                .add_node(render_node, gbm)
                .with_context(|| {
                    format!(
                        "Failed to initialize renderer for device: {}, skipping",
                        render_node
                    )
                })?;

            let mut renderer = match backend.api.single_renderer(&render_node) {
                Ok(renderer) => renderer,
                Err(err) => {
                    warn!(?err, "Failed to initialize output.");
                    backend.api.as_mut().remove_node(&render_node);
                    return Ok(());
                }
            };
            init_shaders(&mut renderer).expect("Failed to initialize renderer");

            for (crtc, conn) in outputs {
                let non_desktop =
                    match drm_helpers::get_property_val(&device.drm, conn, "non-desktop") {
                        Ok((val_type, value)) => {
                            val_type.convert_value(value).as_boolean().unwrap()
                        }
                        Err(err) => {
                            warn!(
                            ?err,
                            "Failed to determine if connector is meant desktop usage, assuming so."
                        );
                            false
                        }
                    };

                if non_desktop {
                    let Ok(output_name) = drm_helpers::interface_name(&device.drm, conn) else {
                        continue
                    };
                    let drm_helpers::EdidInfo {
                        model,
                        manufacturer,
                    } = match drm_helpers::edid_info(&device.drm, conn) {
                        Ok(info) => info,
                        Err(_) => drm_helpers::EdidInfo {
                            model: "Unknown".into(),
                            manufacturer: "Unknown".into(),
                        },
                    };

                    device.non_desktop_connectors.push((conn, crtc));
                    info!(
                        "Connector {} is non-desktop, setting up for leasing",
                        output_name
                    );
                    if let Some(lease_state) = device.leasing_global.as_mut() {
                        lease_state.add_connector::<State>(
                            conn,
                            output_name,
                            format!("{} {}", manufacturer, model),
                        );
                    }
                } else {
                    match device.setup_surface(crtc, conn, (w, 0), &mut renderer) {
                        Ok(output) => {
                            w += output
                                .user_data()
                                .get::<RefCell<OutputConfig>>()
                                .unwrap()
                                .borrow()
                                .mode_size()
                                .w;
                            wl_outputs.push(output);
                        }
                        Err(err) => warn!(?err, "Failed to initialize output."),
                    };
                }
            }
            backend.devices.insert(drm_node, device);
        }

        self.common
            .output_configuration_state
            .add_heads(wl_outputs.iter());
        let seats = self.common.seats().cloned().collect::<Vec<_>>();
        self.common.config.read_outputs(
            &mut self.common.output_configuration_state,
            &mut self.backend,
            &mut self.common.shell,
            seats.into_iter(),
            &self.common.event_loop_handle,
        );

        Ok(())
    }

    fn device_changed(&mut self, dev: dev_t) -> Result<()> {
        if !self.backend.kms().session.is_active() {
            return Ok(());
        }

        let drm_node = DrmNode::from_dev_id(dev)?;
        let mut outputs_removed = Vec::new();
        let mut outputs_added = Vec::new();
        {
            let backend = self.backend.kms();
            if let Some(device) = backend.devices.get_mut(&drm_node) {
                let changes = device.enumerate_surfaces()?;
                let mut w = self.common.shell.global_space().size.w;
                for crtc in changes.removed {
                    if let Some(pos) = device
                        .non_desktop_connectors
                        .iter()
                        .position(|(_, handle)| *handle == crtc)
                    {
                        let (conn, _) = device.non_desktop_connectors.remove(pos);
                        if let Some(leasing_state) = device.leasing_global.as_mut() {
                            leasing_state.withdraw_connector(conn);
                        }
                    } else if let Some(surface) = device.surfaces.remove(&crtc) {
                        if let Some(token) = surface.render_timer_token {
                            self.common.event_loop_handle.remove(token);
                        }
                        w -= surface.output.current_mode().map(|m| m.size.w).unwrap_or(0);
                        outputs_removed.push(surface.output.clone());
                    }
                }
                for (crtc, conn) in changes.added {
                    let non_desktop =
                        match drm_helpers::get_property_val(&device.drm, conn, "non-desktop") {
                            Ok((val_type, value)) => {
                                val_type.convert_value(value).as_boolean().unwrap()
                            }
                            Err(err) => {
                                warn!(
                            ?err,
                            "Failed to determine if connector is meant desktop usage, assuming so."
                        );
                                false
                            }
                        };

                    if non_desktop {
                        let Ok(output_name) = drm_helpers::interface_name(&device.drm, conn) else {
                            continue
                        };
                        let drm_helpers::EdidInfo {
                            model,
                            manufacturer,
                        } = match drm_helpers::edid_info(&device.drm, conn) {
                            Ok(info) => info,
                            Err(_) => drm_helpers::EdidInfo {
                                model: "Unknown".into(),
                                manufacturer: "Unknown".into(),
                            },
                        };

                        device.non_desktop_connectors.push((conn, crtc));
                        info!(
                            "Connector {} is non-desktop, setting up for leasing",
                            output_name
                        );
                        if let Some(lease_state) = device.leasing_global.as_mut() {
                            lease_state.add_connector::<State>(
                                conn,
                                output_name,
                                format!("{} {}", manufacturer, model),
                            );
                        }
                    } else {
                        let mut renderer = match backend.api.single_renderer(&device.render_node) {
                            Ok(renderer) => renderer,
                            Err(err) => {
                                warn!(?err, "Failed to initialize output.");
                                continue;
                            }
                        };
                        match device.setup_surface(crtc, conn, (w, 0), &mut renderer) {
                            Ok(output) => {
                                w += output
                                    .user_data()
                                    .get::<RefCell<OutputConfig>>()
                                    .unwrap()
                                    .borrow()
                                    .mode_size()
                                    .w;
                                outputs_added.push(output);
                            }
                            Err(err) => warn!(?err, "Failed to initialize output."),
                        };
                    }
                }
            }
        }

        self.common
            .output_configuration_state
            .remove_heads(outputs_removed.iter());
        self.common
            .output_configuration_state
            .add_heads(outputs_added.iter());
        let seats = self.common.seats().cloned().collect::<Vec<_>>();
        self.common.config.read_outputs(
            &mut self.common.output_configuration_state,
            &mut self.backend,
            &mut self.common.shell,
            seats.iter().cloned(),
            &self.common.event_loop_handle,
        );
        // Don't remove the outputs, before potentially new ones have been initialized.
        // reading a new config means outputs might become enabled, that were previously disabled.
        // If we have 0 outputs at some point, we won't quit, but shell doesn't know where to move
        // windows and workspaces to.
        for output in outputs_removed {
            self.common
                .shell
                .remove_output(&output, seats.iter().cloned());
        }

        Ok(())
    }

    fn device_removed(&mut self, dev: dev_t, dh: &DisplayHandle) -> Result<()> {
        let drm_node = DrmNode::from_dev_id(dev)?;
        let mut outputs_removed = Vec::new();
        let backend = self.backend.kms();
        if let Some(mut device) = backend.devices.remove(&drm_node) {
            if let Some(mut leasing_global) = device.leasing_global.take() {
                leasing_global.disable_global::<State>();
            }
            backend.api.as_mut().remove_node(&device.render_node);
            for surface in device.surfaces.values_mut() {
                if let Some(token) = surface.render_timer_token.take() {
                    self.common.event_loop_handle.remove(token);
                }
                outputs_removed.push(surface.output.clone());
            }
            if let Some(token) = device.event_token.take() {
                self.common.event_loop_handle.remove(token);
            }
            if let Some(socket) = device.socket.take() {
                self.common.event_loop_handle.remove(socket.token);
                self.common
                    .dmabuf_state
                    .destroy_global::<State>(dh, socket.dmabuf_global);
                dh.remove_global::<State>(socket.drm_global);
            }
        }
        self.common
            .output_configuration_state
            .remove_heads(outputs_removed.iter());

        let seats = self.common.seats().cloned().collect::<Vec<_>>();
        if self.backend.kms().session.is_active() {
            for output in outputs_removed {
                self.common
                    .shell
                    .remove_output(&output, seats.iter().cloned());
            }
            self.common.config.read_outputs(
                &mut self.common.output_configuration_state,
                &mut self.backend,
                &mut self.common.shell,
                seats.into_iter(),
                &self.common.event_loop_handle,
            );
        } else {
            self.common.output_configuration_state.update();
        }

        Ok(())
    }
}

pub struct OutputChanges {
    pub added: Vec<(crtc::Handle, connector::Handle)>,
    pub removed: Vec<crtc::Handle>,
}

impl Device {
    pub fn enumerate_surfaces(&mut self) -> Result<OutputChanges> {
        // enumerate our outputs
        let config = drm_helpers::display_configuration(&mut self.drm, self.supports_atomic)?;

        let surfaces = self
            .surfaces
            .iter()
            .map(|(c, s)| (*c, s.connector))
            .chain(
                self.non_desktop_connectors
                    .iter()
                    .map(|(conn, crtc)| (*crtc, *conn)),
            )
            .collect::<HashMap<crtc::Handle, connector::Handle>>();

        let added = config
            .iter()
            .filter(|(conn, crtc)| surfaces.get(&crtc).map(|c| c != *conn).unwrap_or(true))
            .map(|(conn, crtc)| (crtc, conn))
            .map(|(crtc, conn)| (*crtc, *conn))
            .collect::<Vec<_>>();
        let removed = surfaces
            .iter()
            .filter(|(crtc, conn)| config.get(conn).map(|c| c != *crtc).unwrap_or(true))
            .map(|(crtc, _)| *crtc)
            .collect::<Vec<_>>();

        Ok(OutputChanges { added, removed })
    }

    fn setup_surface(
        &mut self,
        crtc: crtc::Handle,
        conn: connector::Handle,
        position: (i32, i32),
        renderer: &mut GlMultiRenderer<'_, '_>,
    ) -> Result<Output> {
        let drm = &mut self.drm;
        let crtc_info = drm.get_crtc(crtc)?;
        let conn_info = drm.get_connector(conn, false)?;
        let vrr = drm_helpers::set_vrr(drm, crtc, conn, false).unwrap_or(false);
        let max_bpc = drm_helpers::get_max_bpc(drm, conn)?.map(|(_val, range)| range.end.min(16));
        let interface = drm_helpers::interface_name(drm, conn)?;
        let edid_info = drm_helpers::edid_info(drm, conn);
        let mode = crtc_info.mode().unwrap_or_else(|| {
            conn_info
                .modes()
                .iter()
                .find(|mode| mode.mode_type().contains(ModeTypeFlags::PREFERRED))
                .copied()
                .unwrap_or(conn_info.modes()[0])
        });
        let refresh_rate = drm_helpers::calculate_refresh_rate(mode);
        let output_mode = OutputMode {
            size: (mode.size().0 as i32, mode.size().1 as i32).into(),
            refresh: refresh_rate as i32,
        };
        let (phys_w, phys_h) = conn_info.size().unwrap_or((0, 0));
        let output = Output::new(
            interface,
            PhysicalProperties {
                size: (phys_w as i32, phys_h as i32).into(),
                subpixel: match conn_info.subpixel() {
                    connector::SubPixel::HorizontalRgb => Subpixel::HorizontalRgb,
                    connector::SubPixel::HorizontalBgr => Subpixel::HorizontalBgr,
                    connector::SubPixel::VerticalRgb => Subpixel::VerticalRgb,
                    connector::SubPixel::VerticalBgr => Subpixel::VerticalBgr,
                    connector::SubPixel::None => Subpixel::None,
                    _ => Subpixel::Unknown,
                },
                make: edid_info
                    .as_ref()
                    .map(|info| info.manufacturer.clone())
                    .unwrap_or_else(|_| String::from("Unknown")),
                model: edid_info
                    .as_ref()
                    .map(|info| info.model.clone())
                    .unwrap_or_else(|_| String::from("Unknown")),
            },
        );
        for mode in conn_info.modes() {
            let refresh_rate = drm_helpers::calculate_refresh_rate(*mode);
            let mode = OutputMode {
                size: (mode.size().0 as i32, mode.size().1 as i32).into(),
                refresh: refresh_rate as i32,
            };
            output.add_mode(mode);
        }
        output.set_preferred(output_mode);
        output.change_current_state(
            Some(output_mode),
            // TODO: Readout property for monitor rotation
            Some(Transform::Normal),
            None,
            Some(position.into()),
        );
        output.user_data().insert_if_missing(|| {
            RefCell::new(OutputConfig {
                mode: ((output_mode.size.w, output_mode.size.h), Some(refresh_rate)),
                vrr,
                position,
                max_bpc,
                ..Default::default()
            })
        });

        let data = Surface {
            output: output.clone(),
            surface: None,
            connector: conn,
            vrr,
            refresh_rate,
            scheduled: false,
            pending: false,
            dirty: false,
            last_animation_state: false,
            render_timer_token: None,
            fps: Fps::new(renderer.as_mut()),
            feedback: HashMap::new(),
        };
        self.surfaces.insert(crtc, data);

        Ok(output)
    }
}

pub fn source_node_for_surface(w: &WlSurface, dh: &DisplayHandle) -> Option<DrmNode> {
    // Lets check the global drm-node the client got either through default-feedback or wl_drm
    let client = dh.get_client(w.id()).ok()?;
    if let Some(normal_client) = client.get_data::<ClientState>() {
        return normal_client.drm_node.clone();
    }
    // last but not least all xwayland-surfaces should also share a single node
    if let Some(xwayland_client) = client.get_data::<XWaylandClientData>() {
        return xwayland_client.user_data().get::<DrmNode>().cloned();
    }
    None
}

fn render_node_for_output(
    dh: &DisplayHandle,
    output: &Output,
    target_node: DrmNode,
    shell: &Shell,
) -> DrmNode {
    let workspace = shell.active_space(output);
    let nodes = workspace
        .get_fullscreen()
        .map(|w| vec![w.clone()])
        .unwrap_or_else(|| workspace.windows().collect::<Vec<_>>())
        .into_iter()
        .flat_map(|w| w.wl_surface().and_then(|s| source_node_for_surface(&s, dh)))
        .collect::<Vec<_>>();
    if nodes.contains(&target_node) || nodes.is_empty() {
        target_node
    } else {
        nodes
            .iter()
            .fold(HashMap::new(), |mut count_map, node| {
                let count = count_map.entry(node).or_insert(0);
                *count += 1;
                count_map
            })
            .into_iter()
            .reduce(|a, b| if a.1 > b.1 { a } else { b })
            .map(|(node, _)| *node)
            .unwrap_or(target_node)
    }
}

fn get_surface_dmabuf_feedback(
    render_node: DrmNode,
    render_formats: HashSet<Format>,
    target_formats: HashSet<Format>,
    compositor: &GbmDrmCompositor,
) -> SurfaceDmabufFeedback {
    let combined_formats = render_formats
        .intersection(&target_formats)
        .copied()
        .collect::<HashSet<_>>();

    let surface = compositor.surface();
    let planes = surface.planes();
    // We limit the scan-out trache to formats we can also render from
    // so that there is always a fallback render path available in case
    // the supplied buffer can not be scanned out directly
    let planes_formats = planes
        .primary
        .formats
        .iter()
        .cloned()
        .chain(
            planes
                .overlay
                .iter()
                .flat_map(|p| p.formats.iter().cloned()),
        )
        .collect::<HashSet<_>>()
        .intersection(&combined_formats)
        .copied()
        .collect::<Vec<_>>();

    let target_node = surface.device_fd().dev_id().unwrap();
    let mut builder = DmabufFeedbackBuilder::new(render_node.dev_id(), render_formats);
    if target_node != render_node.dev_id() && !combined_formats.is_empty() {
        builder = builder.add_preference_tranche(
            target_node,
            Some(zwp_linux_dmabuf_feedback_v1::TrancheFlags::Scanout),
            combined_formats,
        );
    };
    let render_feedback = builder.clone().build().unwrap();

    let scanout_feedback = builder
        .add_preference_tranche(
            target_node,
            Some(zwp_linux_dmabuf_feedback_v1::TrancheFlags::Scanout),
            planes_formats,
        )
        .build()
        .unwrap();

    SurfaceDmabufFeedback {
        render_feedback,
        scanout_feedback,
    }
}

impl Surface {
    pub fn render_output(
        &mut self,
        api: &mut GpuManager<GbmGlesBackend<GlowRenderer>>,
        render_node: Option<(
            &DrmNode,
            &mut dyn Allocator<Buffer = Dmabuf, Error = AnyError>,
        )>,
        target_node: &DrmNode,
        state: &mut Common,
        screencopy: Option<&[(ScreencopySession, BufferParams)]>,
    ) -> Result<()> {
        #[cfg(feature = "debug")]
        puffin::profile_function!();

        if self.surface.is_none() {
            return Ok(());
        }

        let compositor = self.surface.as_mut().unwrap();
        let (render_node, mut renderer) = match render_node {
            Some((render_node, allocator)) => (
                render_node,
                api.renderer(&render_node, &target_node, allocator, compositor.format())
                    .unwrap(),
            ),
            None => (target_node, api.single_renderer(&target_node).unwrap()),
        };

        self.fps.start();
        #[cfg(feature = "debug")]
        if let Some(rd) = self.fps.rd.as_mut() {
            rd.start_frame_capture(
                renderer.glow_renderer().egl_context().get_context_handle(),
                std::ptr::null(),
            );
        }

        let (previous_workspace, workspace) = state.shell.workspaces.active(&self.output);
        let (previous_idx, idx) = state.shell.workspaces.active_num(&self.output);
        let previous_workspace = previous_workspace
            .zip(previous_idx)
            .map(|((w, start), idx)| (w.handle, idx, start));
        let workspace = (workspace.handle, idx);

        let elements = workspace_elements(
            Some(&render_node),
            &mut renderer,
            state,
            &self.output,
            previous_workspace,
            workspace,
            CursorMode::All,
            &mut Some(&mut self.fps),
            false,
        )
        .map_err(|err| {
            anyhow::format_err!("Failed to accumulate elements for rendering: {:?}", err)
        })?;
        self.fps.elements();

        let res = compositor.render_frame(
            &mut renderer,
            &elements,
            CLEAR_COLOR, // TODO use a theme neutral color
        );
        self.fps.render();

        match res {
            Ok(frame_result) => {
                let feedback = if !frame_result.is_empty {
                    Some(state.take_presentation_feedback(&self.output, &frame_result.states))
                } else {
                    None
                };

                if frame_result.needs_sync() {
                    if let PrimaryPlaneElement::Swapchain(elem) = &frame_result.primary_element {
                        elem.sync.wait();
                    }
                }
                match compositor.queue_frame(feedback) {
                    Ok(()) | Err(FrameError::EmptyFrame) => {}
                    Err(err) => {
                        return Err(err).with_context(|| "Failed to submit result for display")
                    }
                };

                if let Some(screencopy) = screencopy {
                    for (session, params) in screencopy {
                        match render_session(
                            Some(*render_node),
                            &mut renderer,
                            &session,
                            params,
                            self.output.current_transform(),
                            |_node, buffer, renderer, dt, age| {
                                let res = dt.damage_output(age, &elements)?;

                                let mut sync = SyncPoint::default();
                                if let (Some(ref damage), _) = &res {
                                    if let Ok(dmabuf) = get_dmabuf(buffer) {
                                        renderer.bind(dmabuf).map_err(RenderError::Rendering)?;
                                    } else {
                                        let size = buffer_dimensions(buffer).ok_or(
                                            RenderError::Rendering(MultiError::ImportFailed),
                                        )?;
                                        let format =
                                            with_buffer_contents(buffer, |_, _, data| shm_format_to_fourcc(data.format))
                                                .map_err(|_| OutputNoMode)? // eh, we have to do some error
                                                .expect("We should be able to convert all hardcoded shm screencopy formats");
                                        let render_buffer =
                                            Offscreen::<GlesRenderbuffer>::create_buffer(
                                                renderer, format, size,
                                            )
                                            .map_err(RenderError::Rendering)?;
                                        renderer
                                            .bind(render_buffer)
                                            .map_err(RenderError::Rendering)?;
                                    }

                                    let (output_size, output_scale, output_transform) = (
                                        self.output.current_mode().ok_or(OutputNoMode)?.size,
                                        self.output.current_scale().fractional_scale(),
                                        self.output.current_transform(),
                                    );
                                    sync = frame_result
                                        .blit_frame_result(
                                            output_size,
                                            output_transform,
                                            output_scale,
                                            renderer,
                                            damage.iter().copied(),
                                            // TODO: Filter cursor element
                                            elements.iter().map(|e| e.id().clone()),
                                        )
                                        .map_err(|err| match err {
                                            BlitFrameResultError::Rendering(err) => {
                                                RenderError::Rendering(err)
                                            }
                                            BlitFrameResultError::Export(_) => {
                                                RenderError::Rendering(MultiError::DeviceMissing)
                                            }
                                        })?;
                                }

                                Ok(RenderOutputResult {
                                    damage: res.0,
                                    states: res.1,
                                    sync,
                                })
                            },
                        ) {
                            Ok(true) => {} // success
                            Ok(false) => state.still_pending(session.clone(), params.clone()),
                            Err(err) => {
                                warn!(?err, "Error rendering to screencopy session.");
                                session.failed(FailureReason::Unspec);
                            }
                        }
                    }
                    self.fps.screencopy();
                }

                state.send_frames(&self.output, &frame_result.states, |source_node| {
                    Some(
                        self.feedback
                            .entry(source_node)
                            .or_insert_with(|| {
                                let render_formats = api
                                    .single_renderer(&source_node)
                                    .unwrap()
                                    .dmabuf_formats()
                                    .collect::<HashSet<_>>();
                                let target_formats = api
                                    .single_renderer(target_node)
                                    .unwrap()
                                    .dmabuf_formats()
                                    .collect::<HashSet<_>>();
                                get_surface_dmabuf_feedback(
                                    source_node,
                                    render_formats,
                                    target_formats,
                                    compositor,
                                )
                            })
                            .clone(),
                    )
                });
            }
            Err(err) => {
                compositor.reset_buffers();
                anyhow::bail!("Rendering failed: {}", err);
            }
        }

        Ok(())
    }
}

impl KmsState {
    pub fn switch_vt(&mut self, num: i32) -> Result<(), anyhow::Error> {
        self.session.change_vt(num).map_err(Into::into)
    }

    pub fn apply_config_for_output(
        &mut self,
        output: &Output,
        seats: impl Iterator<Item = Seat<State>>,
        shell: &mut Shell,
        test_only: bool,
        loop_handle: &LoopHandle<'_, State>,
    ) -> Result<(), anyhow::Error> {
        let recreated = if let Some(device) = self
            .devices
            .values_mut()
            .find(|dev| dev.surfaces.values().any(|s| s.output == *output))
        {
            let (crtc, surface) = device
                .surfaces
                .iter_mut()
                .find(|(_, s)| s.output == *output)
                .unwrap();
            let output_config = output
                .user_data()
                .get::<RefCell<OutputConfig>>()
                .unwrap()
                .borrow();

            if !output_config.enabled {
                if !test_only {
                    shell.remove_output(output, seats);
                    if surface.surface.take().is_some() {
                        // just drop it
                        surface.pending = false;
                        surface.dirty = false;
                    }
                }
                false
            } else {
                let drm = &mut device.drm;
                let conn = surface.connector;
                let conn_info = drm.get_connector(conn, false)?;
                let mode = conn_info
                    .modes()
                    .iter()
                    // match the size
                    .filter(|mode| {
                        let (x, y) = mode.size();
                        Size::from((x as i32, y as i32)) == output_config.mode_size()
                    })
                    // and then select the closest refresh rate (e.g. to match 59.98 as 60)
                    .min_by_key(|mode| {
                        let refresh_rate = drm_helpers::calculate_refresh_rate(**mode);
                        (output_config.mode.1.unwrap() as i32 - refresh_rate as i32).abs()
                    })
                    .ok_or(anyhow::anyhow!("Unknown mode"))?;

                if !test_only {
                    let res = if let Some(compositor) = surface.surface.as_mut() {
                        if output_config.vrr != surface.vrr {
                            surface.vrr = drm_helpers::set_vrr(
                                drm,
                                *crtc,
                                conn_info.handle(),
                                output_config.vrr,
                            )?;
                        }
                        compositor
                            .use_mode(*mode)
                            .context("Failed to apply new mode")?;
                        false
                    } else {
                        surface.vrr = drm_helpers::set_vrr(drm, *crtc, conn, output_config.vrr)
                            .unwrap_or(false);
                        if let Some(bpc) = output_config.max_bpc {
                            if let Err(err) = drm_helpers::set_max_bpc(drm, conn, bpc) {
                                warn!(
                                    ?bpc,
                                    ?err,
                                    "Failed to set max_bpc on connector: {}",
                                    output.name()
                                );
                            }
                        }
                        surface.refresh_rate = drm_helpers::calculate_refresh_rate(*mode);

                        let drm_surface = drm.create_surface(*crtc, *mode, &[conn])?;
                        let driver = drm
                            .get_driver()
                            .with_context(|| "Failed to query drm driver")?;
                        let mut planes = drm_surface.planes().clone();
                        // QUIRK: Using an overlay plane on a nvidia card breaks the display controller (wtf...)
                        if driver
                            .name()
                            .to_string_lossy()
                            .to_lowercase()
                            .contains("nvidia")
                        {
                            planes.overlay = vec![];
                        }

                        let target = DrmCompositor::new(
                            &surface.output,
                            drm_surface,
                            Some(planes),
                            GbmAllocator::new(
                                device.gbm.clone(),
                                GbmBufferFlags::RENDERING | GbmBufferFlags::SCANOUT,
                            ),
                            device.gbm.clone(),
                            &[
                                Fourcc::Abgr2101010,
                                Fourcc::Argb2101010,
                                Fourcc::Abgr8888,
                                Fourcc::Argb8888,
                            ],
                            device.formats.clone(),
                            drm.cursor_size(),
                            Some(device.gbm.clone()),
                        )
                        .with_context(|| {
                            format!(
                                "Failed to initialize drm surface for {}",
                                drm_helpers::interface_name(drm, conn)
                                    .unwrap_or_else(|_| String::from("Unknown"))
                            )
                        })?;
                        surface.surface = Some(target);
                        true
                    };
                    shell.add_output(output);
                    res
                } else {
                    false
                }
            }
        } else {
            false
        };

        if recreated {
            let sessions = output.pending_buffers().collect::<Vec<_>>();
            if let Err(err) = self.schedule_render(
                loop_handle,
                output,
                None,
                if !sessions.is_empty() {
                    Some(sessions)
                } else {
                    None
                },
            ) {
                error!(
                    ?err,
                    "Error scheduling event loop for output {}.",
                    output.name(),
                );
            }
        }
        Ok(())
    }
    pub fn target_node_for_output(&self, output: &Output) -> Option<DrmNode> {
        self.devices
            .values()
            .find(|dev| dev.surfaces.values().any(|s| s.output == *output))
            .map(|dev| &dev.render_node)
            .copied()
    }

    pub fn try_early_import(
        &mut self,
        dh: &DisplayHandle,
        surface: &WlSurface,
        output: &Output,
        target: DrmNode,
        shell: &Shell,
    ) {
        let render = render_node_for_output(dh, &output, target, &shell);
        if let Err(err) = self.api.early_import(
            if let Some(client) = dh.get_client(surface.id()).ok() {
                if let Some(normal_client) = client.get_data::<ClientState>() {
                    normal_client.drm_node.clone()
                } else if let Some(xwayland_client) = client.get_data::<XWaylandClientData>() {
                    xwayland_client.user_data().get::<DrmNode>().cloned()
                } else {
                    None
                }
            } else {
                None
            },
            render,
            surface,
        ) {
            trace!(?err, "Early import failed.");
        }
    }

    pub fn dmabuf_imported(&mut self, global: &DmabufGlobal, dmabuf: Dmabuf) -> Result<()> {
        for device in self.devices.values() {
            if device
                .socket
                .as_ref()
                .map(|s| &s.dmabuf_global == global)
                .unwrap_or(false)
            {
                return self
                    .api
                    .single_renderer(&device.render_node)?
                    .import_dmabuf(&dmabuf, None)
                    .map(|_| ())
                    .map_err(Into::into);
            }
        }
        unreachable!()
    }

    pub fn schedule_render(
        &mut self,
        loop_handle: &LoopHandle<'_, State>,
        output: &Output,
        estimated_rendertime: Option<Duration>,
        mut screencopy_sessions: Option<Vec<(ScreencopySession, BufferParams)>>,
    ) -> Result<(), InsertError<Timer>> {
        if let Some((device, crtc, surface)) = self
            .devices
            .iter_mut()
            .flat_map(|(node, d)| d.surfaces.iter_mut().map(move |(c, s)| (node, c, s)))
            .find(|(_, _, s)| s.output == *output)
        {
            if surface.surface.is_none() {
                if let Some(sessions) = screencopy_sessions {
                    loop_handle.insert_idle(move |state| {
                        for (session, params) in sessions.into_iter() {
                            state.common.still_pending(session, params);
                        }
                    });
                }
                return Ok(());
            }
            if !surface.scheduled {
                let device = *device;
                let crtc = *crtc;
                if let Some(token) = surface.render_timer_token.take() {
                    loop_handle.remove(token);
                }
                surface.render_timer_token = Some(loop_handle.insert_source(
                    if surface.vrr || estimated_rendertime.is_none() {
                        Timer::immediate()
                    } else {
                        Timer::from_duration(
                            Duration::from_secs_f64(1000.0 / surface.refresh_rate as f64)
                                .saturating_sub(estimated_rendertime.unwrap()),
                        )
                    },
                    move |_time, _, state| {
                        let backend = state.backend.kms();
                        let (mut device, mut other) = backend
                            .devices
                            .iter_mut()
                            .partition::<Vec<_>, _>(|(key, _val)| *key == &device);
                        let target_device = &mut device[0].1;

                        if let Some(surface) = target_device.surfaces.get_mut(&crtc) {
                            let target_node = target_device.render_node;
                            let render_node = render_node_for_output(
                                &state.common.display_handle,
                                &surface.output,
                                target_node,
                                &state.common.shell,
                            );
                            let common = &mut state.common;

                            let result = if render_node != target_node {
                                let render_device = &mut other
                                    .iter_mut()
                                    .find(|(_, val)| val.render_node == render_node)
                                    .unwrap()
                                    .1;
                                surface.render_output(
                                    &mut backend.api,
                                    Some((
                                        &render_device.render_node,
                                        render_device.allocator.as_mut(),
                                    )),
                                    &target_node,
                                    common,
                                    screencopy_sessions.as_deref(),
                                )
                            } else {
                                surface.render_output(
                                    &mut backend.api,
                                    None,
                                    &target_node,
                                    common,
                                    screencopy_sessions.as_deref(),
                                )
                            };

                            match result {
                                Ok(_) => {
                                    trace!(?crtc, "Frame pending");
                                    surface.dirty = false;
                                    surface.pending = true;
                                    surface.scheduled = false;
                                    surface.render_timer_token = None;
                                    return TimeoutAction::Drop;
                                }
                                Err(err) => {
                                    if backend.session.is_active() {
                                        error!(?err, "Error rendering.");
                                        return TimeoutAction::ToDuration(Duration::from_secs_f64(
                                            (1000.0 / surface.refresh_rate as f64) - 0.003,
                                        ));
                                    }
                                }
                            };
                        }

                        if let Some(sessions) = screencopy_sessions.as_mut() {
                            for (session, params) in sessions.drain(..) {
                                state.common.still_pending(session, params);
                            }
                        }
                        TimeoutAction::Drop
                    },
                )?);
                trace!(?surface.render_timer_token, ?crtc, "Frame scheduled");
                surface.scheduled = true;
            } else {
                if let Some(sessions) = screencopy_sessions {
                    loop_handle.insert_idle(|state| {
                        for (session, params) in sessions.into_iter() {
                            state.common.still_pending(session, params);
                        }
                    });
                }
                surface.dirty = true;
            }
        }
        Ok(())
    }
}
