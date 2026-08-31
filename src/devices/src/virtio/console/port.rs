use std::borrow::Cow;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::{mem, thread};

use vm_memory::GuestMemoryMmap;

use crate::virtio::console::console_control::ConsoleControl;
use crate::virtio::console::port_io::{PortInput, PortOutput};
use crate::virtio::console::process_rx::process_rx;
use crate::virtio::console::process_tx::process_tx;
use crate::virtio::console::DEFAULT_QUEUE_SIZE;
use crate::virtio::port_io::PortTerminalProperties;
use crate::virtio::{InterruptTransport, Queue};

pub struct PortDescription {
    pub name: Cow<'static, str>,
    pub input: Option<Box<dyn PortInput + Send>>,
    pub output: Option<Box<dyn PortOutput + Send>>,
    pub terminal: Option<Box<dyn PortTerminalProperties>>,
    pub queue_size: u16,
}

impl PortDescription {
    pub fn console(
        input: Option<Box<dyn PortInput + Send>>,
        output: Option<Box<dyn PortOutput + Send>>,
        terminal: Box<dyn PortTerminalProperties>,
    ) -> Self {
        Self {
            name: "".into(),
            input,
            output,
            terminal: Some(terminal),
            queue_size: DEFAULT_QUEUE_SIZE,
        }
    }

    pub fn output_pipe(
        name: impl Into<Cow<'static, str>>,
        output: Box<dyn PortOutput + Send>,
    ) -> Self {
        Self {
            name: name.into(),
            input: None,
            output: Some(output),
            terminal: None,
            queue_size: DEFAULT_QUEUE_SIZE,
        }
    }

    pub fn input_pipe(
        name: impl Into<Cow<'static, str>>,
        input: Box<dyn PortInput + Send>,
    ) -> Self {
        Self {
            name: name.into(),
            input: Some(input),
            output: None,
            terminal: None,
            queue_size: DEFAULT_QUEUE_SIZE,
        }
    }
}

enum PortState {
    Inactive,
    Active(Box<ActivePortState>),
}

struct ActivePortState {
    stopfd: utils::eventfd::EventFd,
    stop: Arc<AtomicBool>,
    rx_thread: Option<JoinHandle<Queue>>,
    tx_thread: Option<JoinHandle<Queue>>,
    rx_queue: Option<Queue>,
    tx_queue: Option<Queue>,
}

pub(crate) struct Port {
    port_id: u32,
    /// Empty if no name given
    name: Cow<'static, str>,
    state: PortState,
    input: Option<Arc<Mutex<Box<dyn PortInput + Send>>>>,
    output: Option<Arc<Mutex<Box<dyn PortOutput + Send>>>>,
    terminal: Option<Box<dyn PortTerminalProperties>>,
}

impl Port {
    pub(crate) fn new(port_id: u32, description: PortDescription) -> Self {
        Self {
            port_id,
            name: description.name,
            state: PortState::Inactive,
            input: description.input.map(|input| Arc::new(Mutex::new(input))),
            output: description
                .output
                .map(|output| Arc::new(Mutex::new(output))),
            terminal: description.terminal,
        }
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn terminal(&self) -> Option<&dyn PortTerminalProperties> {
        self.terminal.as_deref()
    }

    pub fn notify_rx(&self) {
        if let PortState::Active(state) = &self.state {
            if let Some(handle) = &state.rx_thread {
                handle.thread().unpark()
            }
        }
    }

    pub fn notify_tx(&self) {
        if let PortState::Active(state) = &self.state {
            if let Some(handle) = &state.tx_thread {
                handle.thread().unpark()
            }
        }
    }

    pub fn is_active(&self) -> bool {
        matches!(self.state, PortState::Active(_))
    }

    pub fn start(
        &mut self,
        mem: GuestMemoryMmap,
        rx_queue: Queue,
        tx_queue: Queue,
        interrupt: InterruptTransport,
        control: Arc<ConsoleControl>,
    ) {
        if let PortState::Active(_) = &mut self.state {
            self.shutdown();
        };

        let input = self.input.as_ref().cloned();
        let output = self.output.as_ref().cloned();

        let stopfd = utils::eventfd::EventFd::new(utils::eventfd::EFD_NONBLOCK)
            .expect("Failed to create EventFd for interrupt_evt");
        let stop = Arc::new(AtomicBool::new(false));

        let mut rx_queue = Some(rx_queue);
        let rx_thread = input.map(|input| {
            let mem = mem.clone();
            let interrupt = interrupt.clone();
            let port_id = self.port_id;
            let stopfd = stopfd.try_clone().unwrap();
            let stop = stop.clone();
            let rx_queue = rx_queue.take().expect("RX queue is owned once");
            thread::Builder::new()
                .name("console port".into())
                .spawn(move || {
                    process_rx(
                        mem, rx_queue, interrupt, input, control, port_id, stopfd, stop,
                    )
                })
                .unwrap()
        });

        let mut tx_queue = Some(tx_queue);
        let tx_thread = output.map(|output| {
            let stop = stop.clone();
            let tx_queue = tx_queue.take().expect("TX queue is owned once");
            thread::spawn(move || process_tx(mem, tx_queue, interrupt, output, stop))
        });

        self.state = PortState::Active(Box::new(ActivePortState {
            stopfd,
            stop,
            rx_thread,
            tx_thread,
            rx_queue,
            tx_queue,
        }))
    }

    pub fn shutdown(&mut self) {
        let _ = self.quiesce();
    }

    /// Stops both directions and returns the queues at terminal descriptor boundaries.
    pub(crate) fn quiesce(&mut self) -> Result<(Queue, Queue), &'static str> {
        if let PortState::Active(state) = &mut self.state {
            state.stop.store(true, Ordering::Release);
            let tx_queue = if let Some(tx_thread) = mem::take(&mut state.tx_thread) {
                tx_thread.thread().unpark();
                tx_thread.join().map_err(|_| "console TX worker panicked")?
            } else {
                state.tx_queue.take().ok_or("console TX queue is missing")?
            };
            state.stopfd.write(1).unwrap();
            let rx_queue = if let Some(rx_thread) = mem::take(&mut state.rx_thread) {
                rx_thread.thread().unpark();
                rx_thread.join().map_err(|_| "console RX worker panicked")?
            } else {
                state.rx_queue.take().ok_or("console RX queue is missing")?
            };
            self.state = PortState::Inactive;
            return Ok((rx_queue, tx_queue));
        }
        Err("console port is not active")
    }
}
