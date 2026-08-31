use std::cmp;
use std::io::Write;
use std::iter::zip;
use std::mem::{size_of, size_of_val};
#[cfg(unix)]
use std::os::fd::{AsRawFd, RawFd};
use std::sync::Arc;

use utils::eventfd::EventFd;
use vm_memory::{ByteValued, Bytes, GuestMemoryMmap};

use super::super::{
    ActivateError, ActivateResult, DeviceQueue, DeviceState, QueueConfig, VirtioDevice,
    VirtioStateError,
};
use super::{defs, defs::control_event, defs::uapi};
use crate::virtio::console::console_control::{
    ConsoleControl, VirtioConsoleControl, VirtioConsoleResize,
};
use crate::virtio::console::port::Port;
use crate::virtio::console::port_queue_mapping::{
    num_queues, port_id_to_queue_idx, QueueDirection,
};
use crate::virtio::console::{is_valid_queue_size, ConsoleError, DEFAULT_QUEUE_SIZE};
use crate::virtio::{InterruptTransport, PortDescription, VmmExitObserver};

pub(crate) const CONTROL_RXQ_INDEX: usize = 2;
pub(crate) const CONTROL_TXQ_INDEX: usize = 3;

pub(crate) const AVAIL_FEATURES: u64 = (1 << uapi::VIRTIO_CONSOLE_F_SIZE as u64)
    | (1 << uapi::VIRTIO_CONSOLE_F_MULTIPORT as u64)
    | (1 << uapi::VIRTIO_F_VERSION_1 as u64);

#[derive(Copy, Clone, Debug, Default)]
#[repr(C, packed)]
pub struct VirtioConsoleConfig {
    cols: u16,
    rows: u16,
    max_nr_ports: u32,
    emerg_wr: u32,
}

// Safe because it only has data and has no implicit padding.
unsafe impl ByteValued for VirtioConsoleConfig {}

impl VirtioConsoleConfig {
    pub fn new(cols: u16, rows: u16, max_nr_ports: u32) -> Self {
        VirtioConsoleConfig {
            cols,
            rows,
            max_nr_ports,
            emerg_wr: 0u32,
        }
    }
}

pub struct Console {
    pub(crate) device_state: DeviceState,
    pub(crate) control: Arc<ConsoleControl>,
    pub(crate) ports: Vec<Port>,
    /// Ports whose host workers were active before reversible quiescence.
    resume_ports: Vec<bool>,

    queue_config: Vec<QueueConfig>,
    // Queues are stored as Option so individual queues can be taken when ports start.
    pub(crate) queues: Vec<Option<DeviceQueue>>,
    // TODO: move the queue event handling to the correct threads!
    pub(crate) queue_events: Vec<Arc<EventFd>>,

    pub(crate) avail_features: u64,
    pub(crate) acked_features: u64,

    pub(crate) activate_evt: EventFd,
    pub(crate) sigwinch_evt: EventFd,

    config: VirtioConsoleConfig,
}

impl Console {
    pub fn new(ports: Vec<PortDescription>) -> super::Result<Console> {
        assert!(!ports.is_empty(), "Expected at least 1 port");

        let num_queues = num_queues(ports.len());
        let mut queue_config: Vec<QueueConfig> = (0..num_queues)
            .map(|_| QueueConfig::new(DEFAULT_QUEUE_SIZE))
            .collect();

        // Control queues retain the default while each port independently sizes its RX/TX pair.
        for (port_id, description) in ports.iter().enumerate() {
            if !is_valid_queue_size(description.queue_size) {
                return Err(ConsoleError::InvalidQueueSize {
                    port_id,
                    queue_size: description.queue_size,
                });
            }

            queue_config[port_id_to_queue_idx(QueueDirection::Rx, port_id)] =
                QueueConfig::new(description.queue_size);
            queue_config[port_id_to_queue_idx(QueueDirection::Tx, port_id)] =
                QueueConfig::new(description.queue_size);
        }

        let ports: Vec<Port> = zip(0u32.., ports)
            .map(|(port_id, description)| Port::new(port_id, description))
            .collect();
        let resume_ports = vec![false; ports.len()];

        let (cols, rows) = ports[0]
            .terminal()
            .map(|t| t.get_win_size())
            .unwrap_or((0, 0));
        let config = VirtioConsoleConfig::new(cols, rows, ports.len() as u32);

        Ok(Console {
            control: ConsoleControl::new(),
            ports,
            resume_ports,
            queue_config,
            queues: Vec::new(),
            queue_events: Vec::new(),
            avail_features: AVAIL_FEATURES,
            acked_features: 0,
            activate_evt: EventFd::new(utils::eventfd::EFD_NONBLOCK)
                .map_err(super::ConsoleError::EventFd)?,
            sigwinch_evt: EventFd::new(utils::eventfd::EFD_NONBLOCK)
                .map_err(super::ConsoleError::EventFd)?,
            device_state: DeviceState::Inactive,
            config,
        })
    }

    pub fn id(&self) -> &str {
        defs::CONSOLE_DEV_ID
    }

    #[cfg(unix)]
    pub fn get_sigwinch_fd(&self) -> RawFd {
        self.sigwinch_evt.as_raw_fd()
    }

    pub fn update_console_size(&mut self, port_id: u32, cols: u16, rows: u16) {
        log::debug!("update_console_size {port_id}: {cols} {rows}");
        self.control
            .console_resize(port_id, VirtioConsoleResize { rows, cols });
    }

    /// Return one guest-opened port to active host I/O with its current queues.
    fn start_port(&mut self, port_id: usize, mem: GuestMemoryMmap, interrupt: InterruptTransport) {
        if self.ports[port_id].is_active() {
            return;
        }

        let rx_idx = port_id_to_queue_idx(QueueDirection::Rx, port_id);
        let tx_idx = port_id_to_queue_idx(QueueDirection::Tx, port_id);
        let rx_queue = self.queues[rx_idx]
            .take()
            .expect("port rx queue should exist")
            .queue;
        let tx_queue = self.queues[tx_idx]
            .take()
            .expect("port tx queue should exist")
            .queue;
        self.ports[port_id].start(
            mem,
            rx_queue,
            tx_queue,
            interrupt,
            Arc::clone(&self.control),
        );
    }

    pub(crate) fn process_control_rx(&mut self) -> bool {
        log::trace!("process_control_rx");
        let DeviceState::Activated(ref mem, _) = self.device_state else {
            unreachable!()
        };
        let mut raise_irq = false;

        let control_rx = self.queues[CONTROL_RXQ_INDEX]
            .as_mut()
            .expect("control rx queue should exist");

        while let Some(head) = control_rx.queue.pop(mem) {
            if let Some(buf) = self.control.queue_pop() {
                match mem.write(&buf, head.addr) {
                    Ok(n) => {
                        if n != buf.len() {
                            log::error!("process_control_rx: partial write");
                        }
                        raise_irq = true;
                        log::trace!("process_control_rx wrote {n}");
                        if let Err(e) = control_rx.queue.add_used(mem, head.index, n as u32) {
                            error!("failed to add used elements to the queue: {e:?}");
                        }
                    }
                    Err(e) => {
                        log::error!("process_control_rx failed to write: {e}");
                    }
                }
            } else {
                control_rx.queue.undo_pop();
                break;
            }
        }
        raise_irq
    }

    pub(crate) fn process_control_tx(&mut self) -> bool {
        log::trace!("process_control_tx");
        let (mem, interrupt) = match &self.device_state {
            DeviceState::Activated(mem, interrupt) => (mem.clone(), interrupt.clone()),
            DeviceState::Inactive => unreachable!(),
        };

        let control_tx = self.queues[CONTROL_TXQ_INDEX]
            .as_mut()
            .expect("control tx queue should exist");
        let mut raise_irq = false;

        let mut ports_to_start = Vec::new();

        while let Some(head) = control_tx.queue.pop(&mem) {
            raise_irq = true;

            let cmd: VirtioConsoleControl = match mem.read_obj(head.addr) {
                Ok(cmd) => cmd,
                Err(e) => {
                    log::error!(
                    "Failed to read VirtioConsoleControl struct: {e:?}, struct len = {len}, head.len = {head_len}",
                    len = size_of::<VirtioConsoleControl>(),
                    head_len = head.len,
                );
                    continue;
                }
            };
            if let Err(e) = control_tx
                .queue
                .add_used(&mem, head.index, size_of_val(&cmd) as u32)
            {
                error!("failed to add used elements to the queue: {e:?}");
            }

            log::trace!("VirtioConsoleControl cmd: {cmd:?}");
            match cmd.event {
                control_event::VIRTIO_CONSOLE_DEVICE_READY => {
                    log::debug!(
                        "Device is ready: initialization {}",
                        if cmd.value == 1 { "ok" } else { "failed" }
                    );
                    for port_id in 0..self.ports.len() {
                        self.control.port_add(port_id as u32);
                    }
                }
                control_event::VIRTIO_CONSOLE_PORT_READY => {
                    if cmd.value != 1 {
                        log::error!("Port initialization failed: {cmd:?}");
                        continue;
                    }

                    if let Some(term) = self.ports[cmd.id as usize].terminal() {
                        self.control.mark_console_port(&mem, cmd.id);
                        self.control.port_open(cmd.id, true);
                        let (cols, rows) = term.get_win_size();
                        self.control
                            .console_resize(cmd.id, VirtioConsoleResize { cols, rows });
                    } else {
                        // We start with all ports open, this makes sense for now,
                        // because underlying file descriptors STDIN, STDOUT, STDERR are always open too
                        self.control.port_open(cmd.id, true)
                    }

                    let name = self.ports[cmd.id as usize].name();
                    log::trace!("Port ready {id}: {name}", id = cmd.id);
                    if !name.is_empty() {
                        self.control.port_name(cmd.id, name)
                    }
                }
                control_event::VIRTIO_CONSOLE_PORT_OPEN => {
                    match cmd.value {
                        0 => log::debug!("Guest closed port {}", cmd.id),
                        // PORT_READY only confirms that the guest driver recognizes the port.
                        // Host-to-guest delivery is safe once guest userspace sends PORT_OPEN.
                        1 => ports_to_start.push(cmd.id as usize),
                        _ => {
                            log::error!(
                                "Invalid value ({}) for VIRTIO_CONSOLE_PORT_OPEN on port {}",
                                cmd.value,
                                cmd.id
                            );
                            continue;
                        }
                    }
                }
                _ => log::warn!("Unknown console control event {:x}", cmd.event),
            }
        }

        for port_id in ports_to_start {
            log::trace!("Starting port io for port {port_id}");
            self.start_port(port_id, mem.clone(), interrupt.clone());
        }

        raise_irq
    }
}

impl VirtioDevice for Console {
    fn avail_features(&self) -> u64 {
        self.avail_features
    }

    fn acked_features(&self) -> u64 {
        self.acked_features
    }

    fn set_acked_features(&mut self, acked_features: u64) {
        self.acked_features = acked_features
    }

    fn device_type(&self) -> u32 {
        uapi::VIRTIO_ID_CONSOLE
    }

    fn device_name(&self) -> &str {
        "console"
    }

    fn queue_config(&self) -> &[QueueConfig] {
        &self.queue_config
    }

    fn read_config(&self, offset: u64, mut data: &mut [u8]) {
        let config_slice = self.config.as_slice();
        let config_len = config_slice.len() as u64;
        if offset >= config_len {
            error!("Failed to read config space");
            return;
        }
        if let Some(end) = offset.checked_add(data.len() as u64) {
            // This write can't fail, offset and end are checked against config_len.
            data.write_all(&config_slice[offset as usize..cmp::min(end, config_len) as usize])
                .unwrap();
        }
    }

    fn write_config(&mut self, offset: u64, data: &[u8]) {
        warn!(
            "console: guest driver attempted to write device config (offset={:x}, len={:x})",
            offset,
            data.len()
        );
    }

    fn activate(
        &mut self,
        mem: GuestMemoryMmap,
        interrupt: InterruptTransport,
        queues: Vec<DeviceQueue>,
    ) -> ActivateResult {
        if queues.len() != self.queue_config.len() {
            error!(
                "Cannot activate console. Expected {} queue(s), got {}",
                self.queue_config.len(),
                queues.len()
            );
            return Err(ActivateError::BadActivate);
        }
        if self.activate_evt.write(1).is_err() {
            error!("Cannot write to activate_evt");
            return Err(ActivateError::BadActivate);
        }

        self.queue_events = queues.iter().map(|dq| dq.event.clone()).collect();
        self.queues = queues.into_iter().map(Some).collect();
        self.device_state = DeviceState::Activated(mem.clone(), interrupt.clone());

        // PORT_OPEN is a one-time guest negotiation event. Reversible quiescence must restart
        // the workers that owned open ports directly or agent traffic remains stalled after thaw.
        let resume_ports = self
            .resume_ports
            .iter_mut()
            .enumerate()
            .filter_map(|(port_id, resume)| std::mem::take(resume).then_some(port_id))
            .collect::<Vec<_>>();
        for port_id in resume_ports {
            self.start_port(port_id, mem.clone(), interrupt.clone());
        }

        Ok(())
    }

    fn is_activated(&self) -> bool {
        self.device_state.is_activated()
    }

    fn reset(&mut self) -> bool {
        // Shutdown ports and clear queues.
        for port in &mut self.ports {
            port.shutdown();
        }
        self.queues.clear();
        self.queue_events.clear();
        self.resume_ports.fill(false);
        self.device_state = DeviceState::Inactive;
        true
    }

    fn supports_quiesce(&self) -> bool {
        true
    }

    fn quiesce(&mut self) -> Result<Vec<DeviceQueue>, VirtioStateError> {
        if !self.device_state.is_activated() {
            return Err(VirtioStateError::InvalidLifecycle(
                "console must be activated before quiescence",
            ));
        }

        // Port workers own their RX/TX queues independently of the event-manager-owned control
        // queues. Join every active port first so no host I/O or guest-memory access can outlive
        // the returned queue boundary.
        for (port_id, port) in self.ports.iter_mut().enumerate() {
            if !port.is_active() {
                continue;
            }
            self.resume_ports[port_id] = true;
            let (rx, tx) = port
                .quiesce()
                .map_err(|message| VirtioStateError::Device(std::io::Error::other(message)))?;
            let rx_index = port_id_to_queue_idx(QueueDirection::Rx, port_id);
            let tx_index = port_id_to_queue_idx(QueueDirection::Tx, port_id);
            self.queues[rx_index] = Some(DeviceQueue::new(rx, self.queue_events[rx_index].clone()));
            self.queues[tx_index] = Some(DeviceQueue::new(tx, self.queue_events[tx_index].clone()));
        }

        let queues = self
            .queues
            .iter_mut()
            .map(|queue| {
                queue.take().ok_or(VirtioStateError::InvalidLifecycle(
                    "console queue is missing after worker drain",
                ))
            })
            .collect::<Result<Vec<_>, _>>()?;
        self.device_state = DeviceState::Inactive;
        Ok(queues)
    }
}

impl VmmExitObserver for Console {
    fn on_vmm_exit(&mut self, _exit_code: i32) {
        self.reset();
        log::trace!("Console on_vmm_exit finished");
    }
}

//--------------------------------------------------------------------------------------------------
// Tests
//--------------------------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use utils::eventfd::EventFd;
    use vm_memory::{GuestAddress, GuestMemoryMmap};

    use crate::legacy::DummyIrqChip;
    use crate::virtio::console::port_io;
    use crate::virtio::{DeviceQueue, InterruptTransport, Queue};

    use super::*;

    fn port(name: &'static str, queue_size: u16) -> PortDescription {
        PortDescription {
            name: name.into(),
            input: None,
            output: None,
            terminal: None,
            queue_size,
        }
    }

    #[test]
    fn console_assigns_independent_port_queue_sizes() {
        let console = Console::new(vec![port("agent", 32), port("agent-bulk", 256)]).unwrap();
        let sizes: Vec<u16> = console
            .queue_config
            .iter()
            .map(|queue| queue.size)
            .collect();

        assert_eq!(sizes, vec![32, 32, 32, 32, 256, 256]);
    }

    #[test]
    fn console_rejects_invalid_port_queue_sizes() {
        for queue_size in [15, 24, 2048] {
            match Console::new(vec![port("agent", queue_size)]) {
                Err(ConsoleError::InvalidQueueSize {
                    port_id: 0,
                    queue_size: actual,
                }) => assert_eq!(actual, queue_size),
                _ => panic!("queue size {queue_size} should be rejected"),
            }
        }
    }

    #[test]
    fn console_quiesce_stops_blocked_port_workers_and_returns_every_queue() {
        let description = PortDescription::input_pipe("agent", port_io::input_empty().unwrap());
        let mut console = Console::new(vec![description]).unwrap();
        let mem = GuestMemoryMmap::from_ranges(&[(GuestAddress(0), 0x1_0000)]).unwrap();
        let interrupt =
            InterruptTransport::new(DummyIrqChip::new().into(), "test-console".into()).unwrap();
        let queues = console
            .queue_config()
            .iter()
            .map(|config| {
                DeviceQueue::new(Queue::new(config.size), Arc::new(EventFd::new(0).unwrap()))
            })
            .collect();
        console
            .activate(mem.clone(), interrupt.clone(), queues)
            .unwrap();

        let rx_index = port_id_to_queue_idx(QueueDirection::Rx, 0);
        let tx_index = port_id_to_queue_idx(QueueDirection::Tx, 0);
        let rx = console.queues[rx_index].take().unwrap().queue;
        let tx = console.queues[tx_index].take().unwrap().queue;
        console.ports[0].start(
            mem.clone(),
            rx,
            tx,
            interrupt.clone(),
            Arc::clone(&console.control),
        );

        let queues = console.quiesce().unwrap();
        assert_eq!(queues.len(), console.queue_config().len());
        assert!(!console.is_activated());
        assert!(!console.ports[0].is_active());

        console.activate(mem, interrupt, queues).unwrap();
        assert!(console.is_activated());
        assert!(console.ports[0].is_active());
    }
}
