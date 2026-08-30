use std::io::Read;
use std::os::fd::{AsRawFd, BorrowedFd};
use std::sync::{Arc, Mutex};
use std::thread;

use nix::errno::Errno;
use nix::fcntl::{fcntl, FcntlArg, OFlag};
use nix::poll::{poll, PollFd, PollFlags, PollTimeout};
use utils::eventfd::EventFd;

#[cfg(target_os = "macos")]
use crossbeam_channel::Sender;
use rutabaga_gfx::{
    ResourceCreate3D, ResourceCreateBlob, RutabagaFence, Transfer3D,
    RUTABAGA_PIPE_BIND_RENDER_TARGET, RUTABAGA_PIPE_TEXTURE_2D,
};
#[cfg(target_os = "macos")]
use utils::worker_message::WorkerMessage;
use vm_memory::{GuestAddress, GuestMemoryMmap};

use super::super::descriptor_utils::{Reader, Writer};
use super::super::{DeviceQueue, GpuError, Queue as VirtQueue};
use super::protocol::{
    virtio_gpu_ctrl_hdr, virtio_gpu_mem_entry, GpuCommand, GpuResponse, VirtioGpuResult,
};
use super::virtio_gpu::VirtioGpu;
use crate::virtio::display::DisplayInfo;
use crate::virtio::fs::ExportTable;
use crate::virtio::gpu::protocol::{VIRTIO_GPU_FLAG_FENCE, VIRTIO_GPU_FLAG_INFO_RING_IDX};
use crate::virtio::gpu::virtio_gpu::VirtioGpuRing;
use crate::virtio::{InterruptTransport, VirtioShmRegion};
use krun_display::DisplayBackend;
use krun_display::Rect;

pub struct Worker {
    control_evt: EventFd,
    control_queue: Arc<Mutex<VirtQueue>>,
    // The cursor queue never leaves this thread: it carries no fences, so
    // nothing outside the worker (the rutabaga fence handler, in particular)
    // ever has to reach it.
    cursor_evt: EventFd,
    cursor_queue: VirtQueue,
    mem: GuestMemoryMmap,
    interrupt: InterruptTransport,
    shm_region: VirtioShmRegion,
    virgl_flags: u32,
    #[cfg(target_os = "macos")]
    map_sender: Sender<WorkerMessage>,
    export_table: Option<ExportTable>,
    displays: Box<[DisplayInfo]>,
    display_backend: DisplayBackend<'static>,
}

/// Our own file description for a queue's event, in blocking mode: the worker
/// only ever reads it once `poll` has said it is ready.
fn blocking_event(queue: &DeviceQueue) -> EventFd {
    let event = queue.event.try_clone().unwrap();
    // SAFETY: event is valid for the duration of the fcntl calls.
    let fd = unsafe { BorrowedFd::borrow_raw(event.as_raw_fd()) };
    let flags = OFlag::from_bits_retain(fcntl(fd, FcntlArg::F_GETFL).unwrap()) & !OFlag::O_NONBLOCK;
    fcntl(fd, FcntlArg::F_SETFL(flags)).unwrap();
    event
}

impl Worker {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        control_q: DeviceQueue,
        cursor_q: DeviceQueue,
        mem: GuestMemoryMmap,
        interrupt: InterruptTransport,
        shm_region: VirtioShmRegion,
        virgl_flags: u32,
        #[cfg(target_os = "macos")] map_sender: Sender<WorkerMessage>,
        export_table: Option<ExportTable>,
        displays: Box<[DisplayInfo]>,
        display_backend: DisplayBackend<'static>,
    ) -> Self {
        let control_evt = blocking_event(&control_q);
        let cursor_evt = blocking_event(&cursor_q);

        Self {
            control_evt,
            control_queue: Arc::new(Mutex::new(control_q.queue)),
            cursor_evt,
            cursor_queue: cursor_q.queue,
            mem,
            interrupt,
            shm_region,
            virgl_flags,
            #[cfg(target_os = "macos")]
            map_sender,
            export_table,
            displays,
            display_backend,
        }
    }

    pub fn run(self) {
        thread::Builder::new()
            .name("gpu worker".into())
            .spawn(|| self.work())
            .unwrap();
    }

    fn work(mut self) {
        let mut virtio_gpu = VirtioGpu::new(
            self.mem.clone(),
            self.control_queue.clone(),
            self.interrupt.clone(),
            self.virgl_flags,
            #[cfg(target_os = "macos")]
            self.map_sender.clone(),
            self.export_table.take(),
            self.displays.clone(),
            self.display_backend,
        );

        loop {
            // Rebuilt every iteration: `PollFd` borrows the fds, and the
            // handlers below need `&mut self`.
            // SAFETY: both eventfds live as long as `self`.
            let control_fd = unsafe { BorrowedFd::borrow_raw(self.control_evt.as_raw_fd()) };
            let cursor_fd = unsafe { BorrowedFd::borrow_raw(self.cursor_evt.as_raw_fd()) };
            let mut fds = [
                PollFd::new(control_fd, PollFlags::POLLIN),
                PollFd::new(cursor_fd, PollFlags::POLLIN),
            ];
            if let Err(e) = poll(&mut fds, PollTimeout::NONE) {
                if e != Errno::EINTR {
                    error!("Failed to poll the gpu queues: {e:?}");
                }
                continue;
            }
            let readable =
                |fd: &PollFd| fd.revents().is_some_and(|r| r.contains(PollFlags::POLLIN));
            let (control_ready, cursor_ready) = (readable(&fds[0]), readable(&fds[1]));

            let mut used_any = false;
            if control_ready {
                if let Err(e) = self.control_evt.read() {
                    error!("Failed to read control_evt: {e:?}");
                } else {
                    used_any |= self.process_queue(&mut virtio_gpu, &self.control_queue.clone());
                }
            }
            if cursor_ready {
                if let Err(e) = self.cursor_evt.read() {
                    error!("Failed to read cursor_evt: {e:?}");
                } else {
                    used_any |= self.process_cursor_queue(&mut virtio_gpu);
                }
            }
            if used_any {
                if let Err(e) = self.interrupt.try_signal_used_queue() {
                    error!("Error signaling queue: {e:?}");
                }
            }
        }
    }

    /// Handle a command that arrived on either queue but only ever acts on the
    /// cursor plane.
    fn process_cursor_command(virtio_gpu: &mut VirtioGpu, cmd: GpuCommand) -> VirtioGpuResult {
        match cmd {
            GpuCommand::UpdateCursor(info) => virtio_gpu.update_cursor(
                info.pos.scanout_id,
                info.resource_id,
                info.hot_x,
                info.hot_y,
                info.pos.x,
                info.pos.y,
            ),
            GpuCommand::MoveCursor(info) => {
                virtio_gpu.move_cursor(info.pos.scanout_id, info.pos.x, info.pos.y)
            }
            _ => {
                error!("virtio_gpu: {cmd:?} is not a cursor command");
                Err(GpuResponse::ErrUnspec)
            }
        }
    }

    /// Drain the cursor queue.
    ///
    /// It only ever carries UPDATE_CURSOR and MOVE_CURSOR, which need no
    /// fences, and Linux's `virtio_gpu_queue_cursor` queues a single
    /// out-descriptor with no response buffer — so a response is encoded only
    /// when the driver did provide one. Nothing here may block: this thread
    /// also serves the control queue.
    fn process_cursor_queue(&mut self, virtio_gpu: &mut VirtioGpu) -> bool {
        let mem = self.mem.clone();
        let mut used_any = false;

        while let Some(head) = self.cursor_queue.pop(&mem) {
            let mut len = 0;
            match Reader::new(&mem, head.clone()).map_err(GpuError::QueueReader) {
                Ok(mut reader) => match GpuCommand::decode(&mut reader) {
                    Ok((hdr, cmd)) => {
                        let resp =
                            Self::process_cursor_command(virtio_gpu, cmd).unwrap_or_else(|resp| {
                                debug!("{cmd:?} -> {resp:?}");
                                resp
                            });
                        match Writer::new(&mem, head.clone()) {
                            Ok(mut writer) if writer.available_bytes() != 0 => {
                                match resp.encode(
                                    0,
                                    hdr.fence_id,
                                    hdr.ctx_id,
                                    hdr.ring_idx,
                                    &mut writer,
                                ) {
                                    Ok(written) => len = written,
                                    Err(e) => debug!("cursor queue response encode error: {e:?}"),
                                }
                            }
                            Ok(_) => {}
                            Err(e) => debug!("cursor queue writer error: {e:?}"),
                        }
                    }
                    Err(e) => debug!("cursor descriptor decode error: {e:?}"),
                },
                Err(e) => debug!("cursor queue reader error: {e:?}"),
            }

            if let Err(e) = self.cursor_queue.add_used(&mem, head.index, len) {
                error!("failed to add used elements to the cursor queue: {e:?}");
            }
            used_any = true;
        }

        used_any
    }

    fn process_gpu_command(
        &mut self,
        virtio_gpu: &mut VirtioGpu,
        mem: &GuestMemoryMmap,
        hdr: virtio_gpu_ctrl_hdr,
        cmd: GpuCommand,
        reader: &mut Reader,
    ) -> VirtioGpuResult {
        virtio_gpu.force_ctx_0();

        match cmd {
            GpuCommand::GetDisplayInfo => virtio_gpu.display_info(),
            GpuCommand::GetEdid(info) => virtio_gpu.get_edid(info.scanout),
            GpuCommand::ResourceCreate2d(info) => {
                let resource_id = info.resource_id;

                let resource_create_3d = ResourceCreate3D {
                    target: RUTABAGA_PIPE_TEXTURE_2D,
                    format: info.format,
                    bind: RUTABAGA_PIPE_BIND_RENDER_TARGET,
                    width: info.width,
                    height: info.height,
                    depth: 1,
                    array_size: 1,
                    last_level: 0,
                    nr_samples: 0,
                    flags: 0,
                };

                virtio_gpu.resource_create_3d(resource_id, resource_create_3d)
            }
            GpuCommand::ResourceUnref(info) => virtio_gpu.unref_resource(info.resource_id),
            GpuCommand::SetScanout(info) => virtio_gpu.set_scanout(
                info.scanout_id,
                info.resource_id,
                info.r.width,
                info.r.height,
            ),
            GpuCommand::ResourceFlush(info) => {
                let rect = Rect {
                    x: info.r.x,
                    y: info.r.y,
                    width: info.r.width,
                    height: info.r.height,
                };
                virtio_gpu.flush_resource(info.resource_id, rect)
            }
            GpuCommand::TransferToHost2d(info) => {
                let resource_id = info.resource_id;
                let transfer = Transfer3D::new_2d(info.r.x, info.r.y, info.r.width, info.r.height);
                virtio_gpu.transfer_write(0, resource_id, transfer)
            }
            GpuCommand::ResourceAttachBacking(info) => {
                let available_bytes = reader.available_bytes();
                if available_bytes != 0 {
                    let entry_count = info.nr_entries as usize;
                    let mut vecs = Vec::with_capacity(entry_count);
                    for _ in 0..entry_count {
                        match reader.read_obj::<virtio_gpu_mem_entry>() {
                            Ok(entry) => {
                                let addr = GuestAddress(entry.addr);
                                let len = entry.length as usize;
                                vecs.push((addr, len))
                            }
                            Err(_) => return Err(GpuResponse::ErrUnspec),
                        }
                    }
                    virtio_gpu.attach_backing(info.resource_id, mem, vecs)
                } else {
                    error!("missing data for command {cmd:?}");
                    Err(GpuResponse::ErrUnspec)
                }
            }
            GpuCommand::ResourceDetachBacking(info) => virtio_gpu.detach_backing(info.resource_id),
            // The cursor queue is where these belong, but the spec does not
            // forbid the control queue and a panic would take the whole
            // device down with it.
            GpuCommand::UpdateCursor(_) | GpuCommand::MoveCursor(_) => {
                Self::process_cursor_command(virtio_gpu, cmd)
            }
            GpuCommand::ResourceAssignUuid(info) => {
                let resource_id = info.resource_id;
                virtio_gpu.resource_assign_uuid(resource_id)
            }
            GpuCommand::GetCapsetInfo(info) => virtio_gpu.get_capset_info(info.capset_index),
            GpuCommand::GetCapset(info) => {
                virtio_gpu.get_capset(info.capset_id, info.capset_version)
            }

            GpuCommand::CtxCreate(info) => {
                let context_name: Option<String> = String::from_utf8(info.debug_name.to_vec()).ok();
                virtio_gpu.create_context(hdr.ctx_id, info.context_init, context_name.as_deref())
            }
            GpuCommand::CtxDestroy(_info) => virtio_gpu.destroy_context(hdr.ctx_id),
            GpuCommand::CtxAttachResource(info) => {
                virtio_gpu.context_attach_resource(hdr.ctx_id, info.resource_id)
            }
            GpuCommand::CtxDetachResource(info) => {
                virtio_gpu.context_detach_resource(hdr.ctx_id, info.resource_id)
            }
            GpuCommand::ResourceCreate3d(info) => {
                let resource_id = info.resource_id;
                let resource_create_3d = ResourceCreate3D {
                    target: info.target,
                    format: info.format,
                    bind: info.bind,
                    width: info.width,
                    height: info.height,
                    depth: info.depth,
                    array_size: info.array_size,
                    last_level: info.last_level,
                    nr_samples: info.nr_samples,
                    flags: info.flags,
                };

                virtio_gpu.resource_create_3d(resource_id, resource_create_3d)
            }
            GpuCommand::TransferToHost3d(info) => {
                let ctx_id = hdr.ctx_id;
                let resource_id = info.resource_id;

                let transfer = Transfer3D {
                    x: info.box_.x,
                    y: info.box_.y,
                    z: info.box_.z,
                    w: info.box_.w,
                    h: info.box_.h,
                    d: info.box_.d,
                    level: info.level,
                    stride: info.stride,
                    layer_stride: info.layer_stride,
                    offset: info.offset,
                };

                virtio_gpu.transfer_write(ctx_id, resource_id, transfer)
            }
            GpuCommand::TransferFromHost3d(info) => {
                let ctx_id = hdr.ctx_id;
                let resource_id = info.resource_id;

                let transfer = Transfer3D {
                    x: info.box_.x,
                    y: info.box_.y,
                    z: info.box_.z,
                    w: info.box_.w,
                    h: info.box_.h,
                    d: info.box_.d,
                    level: info.level,
                    stride: info.stride,
                    layer_stride: info.layer_stride,
                    offset: info.offset,
                };

                virtio_gpu.transfer_read(ctx_id, resource_id, transfer, None)
            }
            GpuCommand::CmdSubmit3d(info) => {
                if reader.available_bytes() != 0 {
                    let num_in_fences = info.num_in_fences as usize;
                    let cmd_size = info.size as usize;
                    let mut cmd_buf = vec![0; cmd_size];
                    let mut fence_ids: Vec<u64> = Vec::with_capacity(num_in_fences);

                    for _ in 0..num_in_fences {
                        match reader.read_obj::<u64>() {
                            Ok(fence_id) => {
                                fence_ids.push(fence_id);
                            }
                            Err(_) => return Err(GpuResponse::ErrUnspec),
                        }
                    }

                    if reader.read_exact(&mut cmd_buf[..]).is_ok() {
                        virtio_gpu.submit_command(hdr.ctx_id, &mut cmd_buf[..], &fence_ids)
                    } else {
                        Err(GpuResponse::ErrInvalidParameter)
                    }
                } else {
                    // Silently accept empty command buffers to allow for
                    // benchmarking.
                    Ok(GpuResponse::OkNoData)
                }
            }
            GpuCommand::ResourceCreateBlob(info) => {
                let resource_id = info.resource_id;
                let ctx_id = hdr.ctx_id;

                let resource_create_blob = ResourceCreateBlob {
                    blob_mem: info.blob_mem,
                    blob_flags: info.blob_flags,
                    blob_id: info.blob_id,
                    size: info.size,
                };

                let entry_count = info.nr_entries;
                if reader.available_bytes() == 0 && entry_count > 0 {
                    return Err(GpuResponse::ErrUnspec);
                }

                let mut vecs = Vec::with_capacity(entry_count as usize);
                for _ in 0..entry_count {
                    match reader.read_obj::<virtio_gpu_mem_entry>() {
                        Ok(entry) => {
                            let addr = GuestAddress(entry.addr);
                            let len = entry.length as usize;
                            vecs.push((addr, len))
                        }
                        Err(_) => return Err(GpuResponse::ErrUnspec),
                    }
                }

                virtio_gpu.resource_create_blob(
                    ctx_id,
                    resource_id,
                    resource_create_blob,
                    vecs,
                    mem,
                )
            }
            GpuCommand::SetScanoutBlob(_info) => {
                panic!("virtio_gpu: GpuCommand::SetScanoutBlob unimplemented");
            }
            GpuCommand::ResourceMapBlob(info) => {
                let resource_id = info.resource_id;
                let offset = info.offset;
                virtio_gpu.resource_map_blob(resource_id, &self.shm_region, offset)
            }
            GpuCommand::ResourceUnmapBlob(info) => {
                let resource_id = info.resource_id;
                virtio_gpu.resource_unmap_blob(resource_id, &self.shm_region)
            }
        }
    }

    fn process_queue(
        &mut self,
        virtio_gpu: &mut VirtioGpu,
        control_queue: &Arc<Mutex<VirtQueue>>,
    ) -> bool {
        let mut used_any = false;
        let mem = self.mem.clone();

        loop {
            let head = control_queue.lock().unwrap().pop(&mem);

            if let Some(head) = head {
                let mut reader = Reader::new(&mem, head.clone())
                    .map_err(GpuError::QueueReader)
                    .unwrap();
                let mut writer = Writer::new(&mem, head.clone())
                    .map_err(GpuError::QueueWriter)
                    .unwrap();

                let mut resp = Err(GpuResponse::ErrUnspec);
                let mut gpu_cmd = None;
                let mut ctrl_hdr = None;
                let mut len = 0;

                match GpuCommand::decode(&mut reader) {
                    Ok((hdr, cmd)) => {
                        resp = self.process_gpu_command(virtio_gpu, &mem, hdr, cmd, &mut reader);
                        ctrl_hdr = Some(hdr);
                        gpu_cmd = Some(cmd);
                    }
                    Err(e) => debug!("descriptor decode error: {e:?}"),
                }

                let mut gpu_response = match resp {
                    Ok(gpu_response) => gpu_response,
                    Err(gpu_response) => {
                        debug!("{gpu_cmd:?} -> {gpu_response:?}");
                        gpu_response
                    }
                };

                let mut add_to_queue = true;

                if writer.available_bytes() != 0 {
                    let mut fence_id = 0;
                    let mut ctx_id = 0;
                    let mut flags = 0;
                    let mut ring_idx = 0;
                    if let Some(_cmd) = gpu_cmd {
                        let ctrl_hdr = ctrl_hdr.unwrap();
                        if ctrl_hdr.flags & VIRTIO_GPU_FLAG_FENCE != 0 {
                            flags = ctrl_hdr.flags;
                            fence_id = ctrl_hdr.fence_id;
                            ctx_id = ctrl_hdr.ctx_id;
                            ring_idx = ctrl_hdr.ring_idx;

                            let fence = RutabagaFence {
                                flags,
                                fence_id,
                                ctx_id,
                                ring_idx,
                            };
                            gpu_response = match virtio_gpu.create_fence(fence) {
                                Ok(_) => gpu_response,
                                Err(fence_resp) => {
                                    warn!("create_fence {fence_id} -> {fence_resp:?}");
                                    fence_resp
                                }
                            };
                        }
                    }

                    // Prepare the response now, even if it is going to wait until
                    // fence is complete.
                    match gpu_response.encode(flags, fence_id, ctx_id, ring_idx, &mut writer) {
                        Ok(l) => len = l,
                        Err(e) => debug!("ctrl queue response encode error: {e:?}"),
                    }

                    if flags & VIRTIO_GPU_FLAG_FENCE != 0 {
                        let ring = match flags & VIRTIO_GPU_FLAG_INFO_RING_IDX {
                            0 => VirtioGpuRing::Global,
                            _ => VirtioGpuRing::ContextSpecific { ctx_id, ring_idx },
                        };

                        add_to_queue = virtio_gpu.process_fence(ring, fence_id, head.index, len);
                    }
                }

                if add_to_queue {
                    if let Err(e) = control_queue
                        .lock()
                        .unwrap()
                        .add_used(&mem, head.index, len)
                    {
                        error!("failed to add used elements to the queue: {e:?}");
                    }
                    used_any = true;
                }
            } else {
                break;
            }
        }

        debug!("gpu: process_queue exit");
        used_any
    }
}
