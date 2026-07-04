use std::cmp;
use std::io::Write;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Condvar, Mutex};

use utils::eventfd::EventFd;
use vm_memory::{ByteValued, GuestMemoryMmap};

use super::super::{
    ActivateError, ActivateResult, CpuDeviceError, DeviceQueue, DeviceState, QueueConfig,
    VirtioDevice,
};
use super::{defs, defs::uapi};
use crate::virtio::InterruptTransport;

pub(crate) const BASE_AVAIL_FEATURES: u64 = 1 << uapi::VIRTIO_F_VERSION_1 as u64;

/// How long a vCPU above the enforced count keeps running before it is
/// hard-parked, giving a cooperative guest time to offline it cleanly.
pub const ENFORCEMENT_GRACE: std::time::Duration = std::time::Duration::from_secs(5);

// Config space offsets (three little-endian u32 fields).
const CONFIG_ACTUAL_ONLINE_OFFSET: u64 = 8;

#[derive(Copy, Clone, Debug, Default)]
#[repr(C, packed)]
struct VirtioCpuConfig {
    possible: u32,
    requested_online: u32,
    actual_online: u32,
}

// Safe because it only has data and has no implicit padding.
unsafe impl ByteValued for VirtioCpuConfig {}

/// Host-authoritative CPU count shared with every vCPU run loop.
///
/// vCPUs whose index is at or above `enforced` park between emulation steps
/// until the count is raised again, so a guest that refuses to offline a CPU
/// stops receiving execution time on it anyway. Run loops grant a short grace
/// window ([`ENFORCEMENT_GRACE`]) before hard-parking, because a graceful
/// offline needs the dying CPU to execute its own PSCI CPU_OFF path. The boot
/// CPU is always runnable: `enforced` is clamped to at least 1.
pub struct CpuEnforcement {
    enforced: AtomicU32,
    gate: Mutex<()>,
    raised: Condvar,
}

impl CpuEnforcement {
    fn new(enforced: u32) -> Arc<Self> {
        Arc::new(Self {
            enforced: AtomicU32::new(enforced.max(1)),
            gate: Mutex::new(()),
            raised: Condvar::new(),
        })
    }

    fn set(&self, enforced: u32) {
        self.enforced.store(enforced.max(1), Ordering::Release);
        let _guard = self.gate.lock().unwrap();
        self.raised.notify_all();
    }

    /// Current enforced online count.
    pub fn enforced(&self) -> u32 {
        self.enforced.load(Ordering::Acquire)
    }

    /// Whether the vCPU with this index may run right now. Cheap enough for
    /// the per-iteration check in vCPU run loops.
    pub fn runnable(&self, cpu_index: u32) -> bool {
        cpu_index < self.enforced()
    }

    /// Park the calling vCPU thread until its index becomes runnable again.
    pub fn wait_until_runnable(&self, cpu_index: u32) {
        let mut guard = self.gate.lock().unwrap();
        while cpu_index >= self.enforced.load(Ordering::Acquire) {
            guard = self.raised.wait(guard).unwrap();
        }
    }
}

/// Point-in-time view of the device for host-side control and reporting.
#[derive(Debug, Clone, Copy)]
pub struct CpuStateSnapshot {
    /// CPUs possible in this boot.
    pub possible: u32,

    /// Online count the host asked the guest to converge on.
    pub requested_online: u32,

    /// Online count the guest last reported.
    pub actual_online: u32,

    /// Online count the VMM currently enforces.
    pub enforced: u32,
}

pub struct Cpu {
    pub(crate) queues: Option<Vec<DeviceQueue>>,
    pub(crate) avail_features: u64,
    pub(crate) acked_features: u64,
    pub(crate) activate_evt: EventFd,
    pub(crate) device_state: DeviceState,
    config: VirtioCpuConfig,
    enforcement: Arc<CpuEnforcement>,
}

impl Cpu {
    /// Create the CPU capacity device for a VM booting `initial_online` of
    /// `possible` CPUs. Enforcement starts at the initial count.
    pub fn new(possible: u32, initial_online: u32) -> super::Result<Cpu> {
        Ok(Cpu {
            queues: None,
            avail_features: BASE_AVAIL_FEATURES,
            acked_features: 0,
            activate_evt: EventFd::new(utils::eventfd::EFD_NONBLOCK)
                .map_err(CpuDeviceError::EventFd)?,
            device_state: DeviceState::Inactive,
            config: VirtioCpuConfig {
                possible,
                requested_online: initial_online,
                actual_online: initial_online,
            },
            enforcement: CpuEnforcement::new(initial_online),
        })
    }

    pub fn id(&self) -> &str {
        defs::CPU_DEV_ID
    }

    /// The enforcement state vCPU run loops consult. Cloned once per vCPU at
    /// creation time.
    pub fn enforcement(&self) -> Arc<CpuEnforcement> {
        self.enforcement.clone()
    }

    /// Host control: ask the guest to converge on `online` CPUs and enforce
    /// that ceiling host-side. Clamps to 1..=possible; signals a config change
    /// when the device is active. Returns the accepted target.
    pub fn set_requested_online(&mut self, online: u32) -> u32 {
        let online = cmp::min(online.max(1), self.config.possible);
        self.config.requested_online = online;
        self.enforcement.set(online);
        if let DeviceState::Activated(_, ref interrupt) = self.device_state {
            interrupt.signal_config_change();
        }
        online
    }

    /// Host control: current requested/actual/enforced view.
    pub fn state_snapshot(&self) -> CpuStateSnapshot {
        CpuStateSnapshot {
            possible: self.config.possible,
            requested_online: self.config.requested_online,
            actual_online: self.config.actual_online,
            enforced: self.enforcement.enforced(),
        }
    }
}

impl VirtioDevice for Cpu {
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
        uapi::VIRTIO_ID_MSB_CPU
    }

    fn device_name(&self) -> &str {
        "msb-cpu"
    }

    fn queue_config(&self) -> &[QueueConfig] {
        &defs::QUEUE_CONFIG
    }

    fn read_config(&self, offset: u64, mut data: &mut [u8]) {
        let config_slice = self.config.as_slice();
        let config_len = config_slice.len() as u64;
        if offset >= config_len {
            error!("virtio-msb-cpu: failed to read config space");
            return;
        }
        if let Some(end) = offset.checked_add(data.len() as u64) {
            // This write can't fail, offset and end are checked against config_len.
            data.write_all(&config_slice[offset as usize..cmp::min(end, config_len) as usize])
                .unwrap();
        }
    }

    fn write_config(&mut self, offset: u64, data: &[u8]) {
        // The guest driver reports the online count it converged on; that is
        // the only writable field.
        if offset == CONFIG_ACTUAL_ONLINE_OFFSET && data.len() == 4 {
            self.config.actual_online = u32::from_le_bytes(data.try_into().unwrap());
            return;
        }
        warn!(
            "virtio-msb-cpu: rejected guest config write (offset={:x}, len={:x})",
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
        if queues.len() != defs::NUM_QUEUES {
            error!(
                "Cannot perform activate. Expected {} queue(s), got {}",
                defs::NUM_QUEUES,
                queues.len()
            );
            return Err(ActivateError::BadActivate);
        }

        if self.activate_evt.write(1).is_err() {
            error!("Cannot write to activate_evt");
            return Err(ActivateError::BadActivate);
        }

        self.queues = Some(queues);
        self.device_state = DeviceState::Activated(mem, interrupt);

        Ok(())
    }

    fn is_activated(&self) -> bool {
        self.device_state.is_activated()
    }
}
