//! Checkpointable Windows legacy PIT. The worker is fenced before IRQ state is captured.

use std::io;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Instant;

use serde::{Deserialize, Serialize};

use devices::legacy::IrqChip;
use utils::event::{EventNotifier, RawEventSource};
use windows_sys::Win32::Foundation::WAIT_FAILED;
use windows_sys::Win32::System::Threading::{
    CreateWaitableTimerExW, SetWaitableTimerEx, WaitForMultipleObjects,
    CREATE_WAITABLE_TIMER_HIGH_RESOLUTION, TIMER_ALL_ACCESS,
};

const TICK_HZ: u64 = 1_193_182;
const MAX_PERIOD_NS: u64 = 65_536 * 1_000_000_000 / TICK_HZ;

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub(crate) struct PitState {
    channels: [u8; 3],
    command: u8,
    access: u8,
    mode: u8,
    write_lsb: Option<u8>,
    period_ns: u64,
    periodic: bool,
    remaining_ns: Option<u64>,
    channel2_latch: u32,
    channel2_remaining_ns: Option<u64>,
    channel2_write_lsb: Option<u8>,
    channel2_read_msb_next: bool,
}

struct Clock {
    state: PitState,
    resumed_at: Option<Instant>,
    stopped: bool,
    failure: Option<String>,
}

pub(crate) struct Pit {
    shared: Arc<(Mutex<Clock>, EventNotifier)>,
    worker: Option<JoinHandle<()>>,
}

impl PitState {
    /// Explicit recovery of a periodic channel 0; old snapshots did not retain its phase.
    pub(crate) fn recover_periodic(divisor: u32) -> Result<Self, String> {
        if !(1..=65_536).contains(&divisor) {
            return Err("legacy PIT recovery divisor must be between 1 and 65536".into());
        }
        let period_ns = u64::from(divisor) * 1_000_000_000 / TICK_HZ;
        Ok(Self {
            command: 0x34,
            access: 3,
            mode: 2,
            channels: [((divisor & 0xffff) >> 8) as u8, 0, 0],
            period_ns,
            periodic: true,
            remaining_ns: Some(period_ns.max(1_000_000)),
            ..Default::default()
        })
    }

    fn validate(&self) -> Result<(), String> {
        if self.access > 3
            || self.mode > 7
            || self.period_ns > MAX_PERIOD_NS
            || self
                .remaining_ns
                .is_some_and(|n| self.period_ns == 0 || n > self.period_ns.max(1_000_000))
            || self.channel2_latch > 65_536
            || self
                .channel2_remaining_ns
                .is_some_and(|n| n > MAX_PERIOD_NS)
        {
            return Err("invalid Windows PIT checkpoint state".into());
        }
        Ok(())
    }
}

impl Clock {
    fn advance(&mut self, now: Instant) {
        if let Some(previous) = self.resumed_at.replace(now) {
            let elapsed = now
                .duration_since(previous)
                .as_nanos()
                .min(u64::MAX as u128) as u64;
            if let Some(left) = &mut self.state.remaining_ns {
                *left = left.saturating_sub(elapsed);
            }
            if let Some(left) = &mut self.state.channel2_remaining_ns {
                *left = left.saturating_sub(elapsed);
            }
        } else {
            self.resumed_at = None;
        }
    }
}

impl Pit {
    pub(crate) fn new(intc: IrqChip) -> io::Result<Self> {
        // Condvar timeouts use the Windows scheduler tick (often 15.6 ms), which
        // is too coarse for guest PIT calibration and a 250 Hz guest clock.
        let handle = unsafe {
            CreateWaitableTimerExW(
                std::ptr::null(),
                std::ptr::null(),
                CREATE_WAITABLE_TIMER_HIGH_RESOLUTION,
                TIMER_ALL_ACCESS,
            )
        };
        if handle.is_null() {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: the unnamed timer is a new, non-inherited handle owned by this worker.
        let timer = unsafe { OwnedHandle::from_raw_handle(handle) };
        let shared = Arc::new((
            Mutex::new(Clock {
                state: PitState::default(),
                resumed_at: None,
                stopped: false,
                failure: None,
            }),
            EventNotifier::new()?,
        ));
        let thread_shared = shared.clone();
        let worker = std::thread::Builder::new()
            .name("pit".into())
            .spawn(move || {
                let (lock, wake) = &*thread_shared;
                let mut clock = lock.lock().unwrap();
                loop {
                    if clock.stopped {
                        break;
                    }
                    clock.advance(Instant::now());
                    let remaining = clock.resumed_at.and(clock.state.remaining_ns);
                    if remaining == Some(0) {
                        // Fence in-flight injection before pause returns.
                        if let Err(error) = intc.lock().unwrap().set_irq(Some(0), None) {
                            log::trace!("PIT tick interrupt injection failed: {error:?}");
                        }
                        clock.advance(Instant::now());
                        clock.state.remaining_ns = clock
                            .state
                            .periodic
                            .then(|| clock.state.period_ns.max(1_000_000));
                        continue;
                    }
                    // Reset the wake event while holding the state lock, so a concurrent
                    // pause, reload or resume cannot lose its notification before the wait.
                    let armed = wake.drain().and_then(|()| {
                        if let Some(ns) = remaining {
                            let due = -(ns.div_ceil(100).max(1) as i64);
                            if unsafe {
                                SetWaitableTimerEx(
                                    timer.as_raw_handle(),
                                    &due,
                                    0,
                                    None,
                                    std::ptr::null(),
                                    std::ptr::null(),
                                    0,
                                )
                            } == 0
                            {
                                return Err(io::Error::last_os_error());
                            }
                        }
                        Ok(())
                    });
                    drop(clock);
                    let waited = armed.and_then(|()| {
                        let RawEventSource::WaitableHandle(event) = wake.event_source(0).raw()
                        else {
                            unreachable!("Windows notifier must be waitable")
                        };
                        let handles = [event, timer.as_raw_handle()];
                        let count = if remaining.is_some() { 2 } else { 1 };
                        if unsafe { WaitForMultipleObjects(count, handles.as_ptr(), 0, u32::MAX) }
                            == WAIT_FAILED
                        {
                            return Err(io::Error::last_os_error());
                        }
                        Ok(())
                    });
                    clock = lock.lock().unwrap();
                    if let Err(error) = waited {
                        log::error!("Windows PIT wait failed: {error}");
                        clock.failure = Some(error.to_string());
                        break;
                    }
                }
            })?;
        Ok(Self {
            shared,
            worker: Some(worker),
        })
    }

    pub(crate) fn pause(&self) {
        let mut clock = self.shared.0.lock().unwrap();
        clock.advance(Instant::now());
        clock.resumed_at = None;
        self.shared.1.wake().expect("valid PIT wake event");
    }

    pub(crate) fn resume(&self) -> Result<(), String> {
        let mut clock = self.shared.0.lock().unwrap();
        if let Some(error) = &clock.failure {
            return Err(format!("Windows PIT worker failed: {error}"));
        }
        if clock.resumed_at.is_none() {
            clock.resumed_at = Some(Instant::now());
        }
        self.shared.1.wake().map_err(|e| e.to_string())?;
        Ok(())
    }

    pub(crate) fn capture(&self) -> Result<PitState, String> {
        let clock = self.shared.0.lock().unwrap();
        if let Some(error) = &clock.failure {
            return Err(format!("Windows PIT worker failed: {error}"));
        }
        if clock.resumed_at.is_some() {
            return Err("PIT capture requires paused execution".into());
        }
        Ok(clock.state.clone())
    }

    pub(crate) fn restore(&self, state: PitState) -> Result<(), String> {
        state.validate()?;
        let mut clock = self.shared.0.lock().unwrap();
        if let Some(error) = &clock.failure {
            return Err(format!("Windows PIT worker failed: {error}"));
        }
        if clock.resumed_at.is_some() {
            return Err("PIT restore requires paused execution".into());
        }
        clock.state = state;
        Ok(())
    }
}

impl Drop for Pit {
    fn drop(&mut self) {
        self.shared.0.lock().unwrap().stopped = true;
        self.shared.1.wake().expect("valid PIT wake event");
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

impl devices::BusDevice for Pit {
    fn read(&mut self, _vcpu: u64, offset: u64, data: &mut [u8]) {
        if data.len() != 1 {
            return;
        }
        let mut clock = self.shared.0.lock().unwrap();
        clock.advance(Instant::now());
        let state = &mut clock.state;
        data[0] = match offset {
            0..=1 => state.channels[offset as usize],
            2 => {
                let counter = state
                    .channel2_remaining_ns
                    .map(|n| {
                        (u128::from(n) * u128::from(TICK_HZ) / 1_000_000_000).min(65535) as u16
                    })
                    .unwrap_or(state.channel2_latch.min(65535) as u16);
                let value = if state.channel2_read_msb_next {
                    (counter >> 8) as u8
                } else {
                    counter as u8
                };
                state.channel2_read_msb_next = !state.channel2_read_msb_next;
                value
            }
            3 => state.command,
            _ => 0,
        };
    }

    fn write(&mut self, _vcpu: u64, offset: u64, data: &[u8]) {
        if data.len() != 1 {
            return;
        }
        let value = data[0];
        let mut clock = self.shared.0.lock().unwrap();
        clock.advance(Instant::now());
        let state = &mut clock.state;
        match offset {
            0 => {
                state.channels[0] = value;
                let raw = match state.access {
                    1 => Some(u16::from(value)),
                    2 => Some(u16::from(value) << 8),
                    _ => {
                        if let Some(lsb) = state.write_lsb.take() {
                            Some(u16::from_le_bytes([lsb, value]))
                        } else {
                            state.write_lsb = Some(value);
                            None
                        }
                    }
                };
                if let Some(raw) = raw {
                    let divisor = if raw == 0 { 65536 } else { u64::from(raw) };
                    state.period_ns = divisor * 1_000_000_000 / TICK_HZ;
                    state.periodic = matches!(state.mode, 2 | 3 | 6 | 7);
                    state.remaining_ns = Some(state.period_ns.max(1_000_000));
                }
            }
            1 => state.channels[1] = value,
            2 => {
                state.channels[2] = value;
                if state.command & 0xf0 == 0xb0 {
                    if let Some(lsb) = state.channel2_write_lsb.take() {
                        let raw = u16::from_le_bytes([lsb, value]);
                        state.channel2_latch = if raw == 0 { 65536 } else { u32::from(raw) };
                        state.channel2_remaining_ns =
                            Some(u64::from(state.channel2_latch) * 1_000_000_000 / TICK_HZ);
                        state.channel2_read_msb_next = false;
                    } else {
                        state.channel2_write_lsb = Some(value);
                    }
                }
            }
            3 => {
                state.command = value;
                if value & 0xc0 == 0 && (value >> 4) & 3 != 0 {
                    state.access = (value >> 4) & 3;
                    state.mode = (value >> 1) & 7;
                    state.write_lsb = None;
                    state.remaining_ns = None;
                    state.period_ns = 0;
                }
                if value & 0xc0 == 0x80 {
                    state.channel2_write_lsb = None;
                    state.channel2_read_msb_next = false;
                }
            }
            _ => {}
        }
        self.shared.1.wake().expect("valid PIT wake event");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use devices::legacy::{IrqChipDevice, IrqChipT};
    use devices::BusDevice;
    use std::sync::mpsc::{self, Receiver, Sender};
    use std::time::Duration;
    use utils::eventfd::EventFd;

    struct Interrupts(Sender<u32>);

    impl BusDevice for Interrupts {}

    impl IrqChipT for Interrupts {
        fn get_mmio_addr(&self) -> u64 {
            0
        }
        fn get_mmio_size(&self) -> u64 {
            0
        }
        fn num_pins(&self) -> usize {
            24
        }
        fn set_irq(&self, irq: Option<u32>, _: Option<&EventFd>) -> Result<(), devices::Error> {
            self.0.send(irq.unwrap()).unwrap();
            Ok(())
        }
    }

    fn timer() -> (Pit, Receiver<u32>) {
        let (send, recv) = mpsc::channel();
        (
            Pit::new(Arc::new(Mutex::new(IrqChipDevice::new(Box::new(
                Interrupts(send),
            )))))
            .unwrap(),
            recv,
        )
    }

    fn roundtrip(source: &Pit, destination: &Pit) {
        let bytes =
            bincode::serde::encode_to_vec(source.capture().unwrap(), bincode::config::standard())
                .unwrap();
        let (state, used) =
            bincode::serde::decode_from_slice(&bytes, bincode::config::standard()).unwrap();
        assert_eq!(used, bytes.len());
        destination.restore(state).unwrap();
    }

    #[test]
    fn periodic_timer_keeps_millisecond_cadence() {
        let (mut pit, irqs) = timer();
        pit.write(0, 3, &[0x34]);
        pit.write(0, 0, &[0xa5]);
        pit.write(0, 0, &[0x12]); // 4773 ticks, approximately 4 ms.
        let start = Instant::now();
        pit.resume().unwrap();
        for _ in 0..200 {
            let remaining = Duration::from_secs(2)
                .checked_sub(start.elapsed())
                .expect("250 Hz PIT fell below 100 Hz");
            assert_eq!(irqs.recv_timeout(remaining).unwrap(), 0);
        }
        pit.pause();
    }

    #[test]
    fn periodic_timer_stops_at_pause_and_survives_serialized_restore() {
        let (mut source, source_irqs) = timer();
        source.write(0, 3, &[0x34]);
        source.write(0, 0, &[0xa9]);
        source.write(0, 0, &[0x04]); // ~1 ms period.
        source.resume().unwrap();
        assert_eq!(source_irqs.recv_timeout(Duration::from_secs(2)).unwrap(), 0);
        source.pause();
        while source_irqs.try_recv().is_ok() {}
        assert!(source_irqs
            .recv_timeout(Duration::from_millis(100))
            .is_err());

        let (restored, irqs) = timer();
        roundtrip(&source, &restored);
        assert!(irqs.recv_timeout(Duration::from_millis(100)).is_err());
        restored.resume().unwrap();
        for _ in 0..3 {
            assert_eq!(irqs.recv_timeout(Duration::from_secs(2)).unwrap(), 0);
        }
        restored.pause();
        while irqs.try_recv().is_ok() {}
        assert!(irqs.recv_timeout(Duration::from_millis(100)).is_err());
    }

    #[test]
    fn partially_programmed_one_shot_completes_after_restore_and_fires_once() {
        let (mut source, _) = timer();
        source.write(0, 3, &[0x30]);
        source.write(0, 0, &[0xff]);
        let (mut restored, irqs) = timer();
        roundtrip(&source, &restored);
        restored.write(0, 0, &[0xff]);
        restored.resume().unwrap();
        assert_eq!(irqs.recv_timeout(Duration::from_secs(2)).unwrap(), 0);
        assert!(irqs.recv_timeout(Duration::from_millis(150)).is_err());
        restored.pause();
        let (again, again_irqs) = timer();
        roundtrip(&restored, &again);
        again.resume().unwrap();
        assert!(again_irqs.recv_timeout(Duration::from_millis(150)).is_err());
    }

    #[test]
    fn channel_two_partial_write_and_read_position_survive_restore() {
        let (mut source, _) = timer();
        source.write(0, 3, &[0xb0]);
        source.write(0, 2, &[0x34]);
        let (mut restored, _) = timer();
        roundtrip(&source, &restored);
        restored.write(0, 2, &[0x12]);
        let mut low = [0];
        restored.read(0, 2, &mut low);
        let (mut again, _) = timer();
        roundtrip(&restored, &again);
        let mut high = [0];
        again.read(0, 2, &mut high);
        assert_eq!(high[0], 0x12);
        assert!((0x33..=0x34).contains(&low[0]));
    }
}
