//! ARM64 KVM execution state and controller-owned clock transitions.

use std::os::fd::{AsRawFd, BorrowedFd, OwnedFd};

use kvm_bindings::{kvm_mp_state, kvm_one_reg, kvm_vcpu_events};
use kvm_ioctls::VcpuFd;
use serde::{Deserialize, Serialize};

use super::vstate::{Error, Result, VcpuHandle};

//--------------------------------------------------------------------------------------------------
// Constants
//--------------------------------------------------------------------------------------------------

const SYSREG: u64 = 0x6030_0000_0013_0000;
pub(super) const MPIDR: u64 = SYSREG | 0xc005;
// KVM's stable userspace ABI accidentally swapped CNT and CVAL encodings.
// These are the UAPI values, deliberately not architectural encodings.
const VT_COUNT: u64 = SYSREG | 0xdf1a;
const VT_COMPARE: u64 = SYSREG | 0xdf02;
const VT_CONTROL: u64 = SYSREG | 0xdf19;
const PT_COUNT: u64 = SYSREG | 0xdf01;
const PT_COMPARE: u64 = SYSREG | 0xdf12;
const PT_CONTROL: u64 = SYSREG | 0xdf11;
const MAX_REGISTERS: usize = 8192;
const MAX_STATE_BYTES: usize = 16 * 1024 * 1024;
// _IOW(KVMIO, 0xab/0xac, struct kvm_one_reg), Linux ARM64 UAPI.
const GET_ONE_REG: libc::c_ulong = 0x4010_aeab;
const SET_ONE_REG: libc::c_ulong = 0x4010_aeac;
const GET_REG_LIST: libc::c_ulong = 0xc008_aeb0;

//--------------------------------------------------------------------------------------------------
// Types
//--------------------------------------------------------------------------------------------------

/// A duplicated vCPU descriptor used only behind the completed pause barrier.
pub(super) struct VcpuControl(OwnedFd);

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub(super) struct ClockState {
    virtual_count: u64,
    physical_count: u64,
    frequency: u64,
}

#[derive(Clone, Copy, Debug)]
pub(super) struct TimerState {
    virtual_compare: u64,
    virtual_control: u64,
    physical_compare: u64,
    physical_control: u64,
}

pub(super) struct PausedState {
    pub clock: ClockState,
    pub timers: Vec<TimerState>,
}

#[derive(Serialize, Deserialize)]
struct CpuState {
    registers: Vec<(u64, Vec<u8>)>,
    mp_state: kvm_mp_state,
    events: ExceptionState,
}

// Serialize defined fields only: the KVM ABI includes reserved padding which
// must remain zero and is not part of the guest's architectural state.
#[derive(Serialize, Deserialize)]
struct ExceptionState {
    serror_pending: u8,
    serror_has_esr: u8,
    ext_dabt_pending: u8,
    serror_esr: u64,
}

//--------------------------------------------------------------------------------------------------
// Methods
//--------------------------------------------------------------------------------------------------

impl VcpuControl {
    pub fn duplicate(vcpu: &VcpuFd) -> Result<Self> {
        // Borrow only long enough to duplicate; the owned descriptor remains
        // valid independently of worker shutdown and does not expose KVM_RUN.
        let borrowed = unsafe { BorrowedFd::borrow_raw(vcpu.as_raw_fd()) };
        borrowed.try_clone_to_owned().map(Self).map_err(codec)
    }

    pub fn read(&self, id: u64) -> Result<u64> {
        let mut value = 0_u64;
        let reg = kvm_one_reg {
            id,
            addr: &mut value as *mut u64 as u64,
        };
        let result = unsafe { libc::ioctl(self.0.as_raw_fd(), GET_ONE_REG as _, &reg) };
        if result < 0 {
            return Err(codec(format!(
                "read register {id:#x}: {}",
                std::io::Error::last_os_error()
            )));
        }
        Ok(value)
    }

    fn write(&self, id: u64, value: u64) -> Result<()> {
        let reg = kvm_one_reg {
            id,
            addr: &value as *const u64 as u64,
        };
        let result = unsafe { libc::ioctl(self.0.as_raw_fd(), SET_ONE_REG as _, &reg) };
        if result < 0 {
            return Err(codec(format!(
                "write register {id:#x}: {}",
                std::io::Error::last_os_error()
            )));
        }
        Ok(())
    }
}

impl PausedState {
    pub fn capture(handles: &[VcpuHandle]) -> Result<Self> {
        let first = handles
            .first()
            .ok_or_else(|| codec("no vCPU clock"))?
            .arm_control()?;
        let clock = ClockState {
            virtual_count: first.read(VT_COUNT)?,
            physical_count: first.read(PT_COUNT)?,
            frequency: counter_frequency(),
        };
        let mut timers = Vec::with_capacity(handles.len());
        // No guest CPU can run while the controller reads/disables these timers.
        // Preserve the original controls for both capture and ordinary resume.
        for handle in handles {
            let cpu = handle.arm_control()?;
            timers.push(TimerState {
                virtual_compare: cpu.read(VT_COMPARE)?,
                virtual_control: cpu.read(VT_CONTROL)? & 3,
                physical_compare: cpu.read(PT_COMPARE)?,
                physical_control: cpu.read(PT_CONTROL)? & 3,
            });
        }
        for handle in handles {
            let cpu = handle.arm_control()?;
            cpu.write(VT_CONTROL, 0)?;
            cpu.write(PT_CONTROL, 0)?;
        }
        Ok(Self { clock, timers })
    }

    pub fn resume(&self, handles: &[VcpuHandle]) -> Result<()> {
        if handles.len() != self.timers.len() {
            return Err(codec("timer topology mismatch"));
        }
        let first = handles
            .first()
            .ok_or_else(|| codec("no vCPU clock"))?
            .arm_control()?;
        if counter_frequency() != self.clock.frequency {
            return Err(codec("counter frequency mismatch"));
        }
        // KVM's timer counter setters update VM-wide offsets. Apply each view
        // once, not once per CPU, and retain distinct physical/virtual origins.
        first.write(VT_COUNT, self.clock.virtual_count)?;
        first.write(PT_COUNT, self.clock.physical_count)?;
        for (handle, timer) in handles.iter().zip(&self.timers) {
            let cpu = handle.arm_control()?;
            cpu.write(VT_COMPARE, timer.virtual_compare)?;
            cpu.write(PT_COMPARE, timer.physical_compare)?;
            cpu.write(VT_CONTROL, timer.virtual_control)?;
            cpu.write(PT_CONTROL, timer.physical_control)?;
        }
        Ok(())
    }
}

//--------------------------------------------------------------------------------------------------
// Functions
//--------------------------------------------------------------------------------------------------

fn counter_frequency() -> u64 {
    // KVM exposes the host's architectural counter frequency to EL1 guests,
    // but does not export CNTFRQ through GET_ONE_REG. EL0 can read it directly.
    let frequency: u64;
    unsafe {
        std::arch::asm!("mrs {frequency}, cntfrq_el0", frequency = out(reg) frequency, options(nomem, nostack, preserves_flags))
    };
    frequency
}

pub(super) fn capture_cpu(fd: &VcpuFd) -> Result<Vec<u8>> {
    let ids = register_ids(fd)?;
    let mut registers = Vec::with_capacity(ids.len());
    for id in ids {
        let mut value = vec![0; register_size(id)?];
        fd.get_one_reg(id, &mut value).map_err(codec)?;
        registers.push((id, value));
    }
    let exception = fd.get_vcpu_events().map_err(codec)?.exception;
    encode(&CpuState {
        registers,
        mp_state: fd.get_mp_state().map_err(codec)?,
        events: ExceptionState {
            serror_pending: exception.serror_pending,
            serror_has_esr: exception.serror_has_esr,
            ext_dabt_pending: exception.ext_dabt_pending,
            serror_esr: exception.serror_esr,
        },
    })
}

pub(super) fn complete_capture(bytes: &[u8], paused: &PausedState, id: usize) -> Result<Vec<u8>> {
    let mut state = decode_cpu(bytes)?;
    let timer = paused
        .timers
        .get(id)
        .ok_or_else(|| codec("timer topology mismatch"))?;
    for (reg, value) in &mut state.registers {
        let saved = match *reg {
            VT_COUNT => Some(paused.clock.virtual_count),
            PT_COUNT => Some(paused.clock.physical_count),
            VT_COMPARE => Some(timer.virtual_compare),
            PT_COMPARE => Some(timer.physical_compare),
            VT_CONTROL => Some(timer.virtual_control),
            PT_CONTROL => Some(timer.physical_control),
            _ => None,
        };
        if let Some(saved) = saved {
            *value = saved.to_le_bytes().to_vec();
        }
    }
    encode(&state)
}

pub(super) fn stage_timer(bytes: &[u8]) -> Result<TimerState> {
    let state = decode_cpu(bytes)?;
    let get = |id| -> Result<u64> {
        let (_, bytes) = state
            .registers
            .iter()
            .find(|(reg, _)| *reg == id)
            .ok_or_else(|| codec(format!("missing timer register {id:#x}")))?;
        Ok(u64::from_le_bytes(
            bytes.as_slice().try_into().map_err(codec)?,
        ))
    };
    Ok(TimerState {
        virtual_compare: get(VT_COMPARE)?,
        virtual_control: get(VT_CONTROL)? & 3,
        physical_compare: get(PT_COMPARE)?,
        physical_control: get(PT_CONTROL)? & 3,
    })
}

pub(super) fn restore_cpu(fd: &VcpuFd, bytes: &[u8]) -> Result<()> {
    let state = decode_cpu(bytes)?;
    let ids = register_ids(fd)?;
    if ids
        != state
            .registers
            .iter()
            .map(|(id, _)| *id)
            .collect::<Vec<_>>()
    {
        return Err(codec("ARM64 register contract differs from destination"));
    }
    fd.set_one_reg(VT_CONTROL, &0_u64.to_le_bytes())
        .map_err(codec)?;
    fd.set_one_reg(PT_CONTROL, &0_u64.to_le_bytes())
        .map_err(codec)?;
    for (id, value) in state.registers {
        if matches!(id, VT_COUNT | PT_COUNT | VT_CONTROL | PT_CONTROL) {
            continue;
        }
        if let Err(error) = fd.set_one_reg(id, &value) {
            // Read-only feature/cache registers need not be written, but must
            // match exactly. Other failures are never silently ignored.
            if !matches!(error.errno(), libc::EINVAL | libc::EPERM) {
                return Err(codec(error));
            }
            let mut current = vec![0; value.len()];
            fd.get_one_reg(id, &mut current).map_err(codec)?;
            if current != value {
                return Err(codec(format!("register {id:#x}: {error}")));
            }
        }
    }
    let mut events = kvm_vcpu_events::default();
    events.exception.serror_pending = state.events.serror_pending;
    events.exception.serror_has_esr = state.events.serror_has_esr;
    events.exception.ext_dabt_pending = state.events.ext_dabt_pending;
    events.exception.serror_esr = state.events.serror_esr;
    fd.set_vcpu_events(&events).map_err(codec)?;
    // Registers alone do not wake secondary CPUs created with POWER_OFF.
    fd.set_mp_state(state.mp_state).map_err(codec)?;
    Ok(())
}

pub(super) fn encode<T: Serialize>(state: &T) -> Result<Vec<u8>> {
    bincode::serde::encode_to_vec(state, bincode::config::standard()).map_err(codec)
}

pub(super) fn decode<T: serde::de::DeserializeOwned>(bytes: &[u8]) -> Result<T> {
    let (state, used) = bincode::serde::decode_from_slice(
        bytes,
        bincode::config::standard().with_limit::<MAX_STATE_BYTES>(),
    )
    .map_err(codec)?;
    if used != bytes.len() {
        return Err(codec("trailing ARM64 state bytes"));
    }
    Ok(state)
}

fn decode_cpu(bytes: &[u8]) -> Result<CpuState> {
    let state: CpuState = decode(bytes)?;
    if state.registers.len() > MAX_REGISTERS {
        return Err(codec("too many ARM64 registers"));
    }
    let mut previous = None;
    for (id, value) in &state.registers {
        if previous.is_some_and(|old| old >= *id) || value.len() != register_size(*id)? {
            return Err(codec("invalid ARM64 register layout"));
        }
        previous = Some(*id);
    }
    Ok(state)
}

fn register_ids(fd: &VcpuFd) -> Result<Vec<u64>> {
    // kvm-bindings' FamStruct caps this list at 500 entries, although KVM's
    // ABI has no such limit. Use its exact aligned layout (u64 n, u64 reg[])
    // so feature-rich CPUs are captured without truncating their state.
    let mut list = vec![0_u64; MAX_REGISTERS + 1];
    list[0] = MAX_REGISTERS as u64;
    let result = unsafe { libc::ioctl(fd.as_raw_fd(), GET_REG_LIST as _, list.as_mut_ptr()) };
    if result < 0 {
        return Err(codec(format!(
            "enumerate registers: {}",
            std::io::Error::last_os_error()
        )));
    }
    let count = usize::try_from(list[0]).map_err(codec)?;
    if count > MAX_REGISTERS {
        return Err(codec("too many ARM64 registers"));
    }
    let mut ids = list[1..=count].to_vec();
    ids.sort_unstable();
    Ok(ids)
}

fn register_size(id: u64) -> Result<usize> {
    let exponent = (id & kvm_bindings::KVM_REG_SIZE_MASK) >> kvm_bindings::KVM_REG_SIZE_SHIFT;
    if exponent > 11 {
        return Err(codec("unsupported ARM64 register width"));
    }
    Ok(1 << exponent)
}

fn codec(error: impl std::fmt::Display) -> Error {
    Error::StateCodec(error.to_string())
}

//--------------------------------------------------------------------------------------------------
// Tests
//--------------------------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn cpu_state() -> CpuState {
        let mut registers = [
            VT_COUNT, PT_COUNT, VT_COMPARE, PT_COMPARE, VT_CONTROL, PT_CONTROL,
        ]
        .into_iter()
        .map(|id| (id, 99_u64.to_le_bytes().to_vec()))
        .collect::<Vec<_>>();
        registers.sort_by_key(|(id, _)| *id);
        CpuState {
            registers,
            mp_state: kvm_mp_state {
                mp_state: kvm_bindings::KVM_MP_STATE_RUNNABLE,
            },
            events: ExceptionState {
                serror_pending: 1,
                serror_has_esr: 1,
                ext_dabt_pending: 0,
                serror_esr: 0x1234,
            },
        }
    }

    #[test]
    fn capture_uses_frozen_clocks_and_original_timer_controls() {
        let paused = PausedState {
            clock: ClockState {
                virtual_count: 10,
                physical_count: 20,
                frequency: 24_000_000,
            },
            timers: vec![TimerState {
                virtual_compare: 30,
                virtual_control: 3,
                physical_compare: 40,
                physical_control: 1,
            }],
        };
        let bytes = complete_capture(&encode(&cpu_state()).unwrap(), &paused, 0).unwrap();
        let decoded = decode_cpu(&bytes).unwrap();
        let get = |id| {
            u64::from_le_bytes(
                decoded
                    .registers
                    .iter()
                    .find(|(reg, _)| *reg == id)
                    .unwrap()
                    .1
                    .as_slice()
                    .try_into()
                    .unwrap(),
            )
        };
        assert_eq!(get(VT_COUNT), 10);
        assert_eq!(get(PT_COUNT), 20);
        assert_eq!(stage_timer(&bytes).unwrap().virtual_control, 3);
        assert_eq!(stage_timer(&bytes).unwrap().physical_compare, 40);
        assert_eq!(decoded.events.serror_esr, 0x1234);
        assert!(complete_capture(&bytes, &paused, 1).is_err());
    }

    #[test]
    fn malformed_register_contract_is_rejected() {
        let mut state = cpu_state();
        state.registers.push(state.registers[0].clone());
        assert!(decode_cpu(&encode(&state).unwrap()).is_err());
        let mut state = cpu_state();
        state.registers[0].1.pop();
        assert!(decode_cpu(&encode(&state).unwrap()).is_err());
        let mut bytes = encode(&cpu_state()).unwrap();
        bytes.push(0);
        assert!(decode_cpu(&bytes).is_err());
    }

    #[test]
    fn timer_registers_are_mandatory() {
        let mut state = cpu_state();
        state.registers.retain(|(id, _)| *id != PT_COMPARE);
        assert!(stage_timer(&encode(&state).unwrap()).is_err());
    }
}
