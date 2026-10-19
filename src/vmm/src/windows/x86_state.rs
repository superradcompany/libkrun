//! WHP x86-64 state, completed by the controller while every worker is stopped.

use serde::{Deserialize, Serialize};
use windows_sys::Win32::System::Hypervisor::*;

use super::{ApParkState, ApStartupRouter, Error, Result};

//--------------------------------------------------------------------------------------------------
// Constants
//--------------------------------------------------------------------------------------------------

// WHvVirtualProcessorStateTypeNestedState from the x64 WHP API. The pinned
// windows-sys bindings do not yet expose this state category.
const NESTED_STATE: i32 = 0x0000_1002;

//--------------------------------------------------------------------------------------------------
// Types
//--------------------------------------------------------------------------------------------------

#[derive(Serialize, Deserialize)]
struct VmState {
    features: Vec<Vec<u8>>,
    tsc_frequency_hz: u64,
    startup: Vec<ApParkState>,
}

#[derive(Serialize, Deserialize)]
struct CpuState {
    registers: Vec<(WHV_REGISTER_NAME, [u8; 16])>,
    xsave: Vec<u8>,
    apic: Vec<u8>,
    synic: [Vec<u8>; 3],
}

//--------------------------------------------------------------------------------------------------
// Functions
//--------------------------------------------------------------------------------------------------

pub(super) fn capture_vm(
    partition: WHV_PARTITION_HANDLE,
    router: &ApStartupRouter,
    nested_enabled: bool,
) -> Result<Vec<u8>> {
    let startup = router
        .slots
        .iter()
        .map(|slot| {
            slot.lock()
                .map(|state| *state)
                .map_err(|_| codec("startup state mutex poisoned"))
        })
        .collect::<Result<Vec<_>>>()?;
    let state = VmState {
        features: features(partition, nested_enabled)?,
        tsc_frequency_hz: tsc_frequency_hz(partition)?,
        startup,
    };
    if nested_enabled {
        encode(&(state, reference_time(partition)?))
    } else {
        encode(&state)
    }
}

pub(super) fn restore_vm(
    partition: WHV_PARTITION_HANDLE,
    router: &ApStartupRouter,
    bytes: &[u8],
    nested_enabled: bool,
) -> Result<()> {
    let (state, reference_time): (VmState, Option<u64>) = if nested_enabled {
        let (state, time) = decode(bytes)?;
        (state, Some(time))
    } else {
        (decode(bytes)?, None)
    };
    if state.features != features(partition, nested_enabled)?
        || state.tsc_frequency_hz != tsc_frequency_hz(partition)?
        || state.startup.len() != router.slots.len()
        || state.startup.first() != Some(&ApParkState::Running)
    {
        return Err(codec(
            "WHP x86 processor features, frequency or topology differ",
        ));
    }
    // A newly constructed partition has never run. Explicitly freeze before
    // installing TSC/APIC state so construction latency cannot age the snapshot.
    super::set_partition_time_running(partition, false)?;
    if let Some(reference_time) = reference_time {
        // Hyper-V's reference clock must resume at the captured epoch before
        // reference-TSC and per-CPU state are installed.
        let status = unsafe {
            WHvSetPartitionProperty(
                partition,
                WHvPartitionPropertyCodeReferenceTime,
                (&reference_time as *const u64).cast(),
                8,
            )
        };
        if status < 0 {
            return Err(codec(format!(
                "WHP reference time restore: HRESULT {status:#x}"
            )));
        }
    }
    for (slot, value) in router.slots.iter().zip(state.startup) {
        *slot
            .lock()
            .map_err(|_| codec("startup state mutex poisoned"))? = value;
    }
    Ok(())
}

pub(super) fn capture_cpu(
    partition: WHV_PARTITION_HANDLE,
    id: u32,
    nested_enabled: bool,
) -> Result<Vec<u8>> {
    let cpu = u8::try_from(id).map_err(codec)?;
    let names = register_names(partition, cpu)?;
    let values = super::get_vcpu_register_bytes(partition, cpu, &names)?;
    let get = |kind| super::get_virtual_processor_state(partition, id, kind);
    let state = CpuState {
        registers: names.into_iter().zip(values).collect(),
        xsave: get(WHvVirtualProcessorStateTypeXsaveState)?,
        apic: get(WHvVirtualProcessorStateTypeInterruptControllerState2)?,
        synic: [
            get(WHvVirtualProcessorStateTypeSynicMessagePage)?,
            get(WHvVirtualProcessorStateTypeSynicEventFlagPage)?,
            get(WHvVirtualProcessorStateTypeSynicTimerState)?,
        ],
    };
    let nested = if nested_enabled {
        Some(get(NESTED_STATE)?)
    } else {
        None
    };
    encode_cpu_payload(&state, nested.as_deref())
}

fn encode_cpu_payload(state: &CpuState, nested: Option<&[u8]>) -> Result<Vec<u8>> {
    if let Some(nested) = nested {
        validate_nested_state(nested)?;
        encode(&(state, nested))
    } else {
        encode(state)
    }
}

fn decode_cpu_payload(bytes: &[u8], nested_enabled: bool) -> Result<(CpuState, Option<Vec<u8>>)> {
    if nested_enabled {
        let (state, nested): (CpuState, Vec<u8>) = decode(bytes)?;
        validate_nested_state(&nested)?;
        Ok((state, Some(nested)))
    } else {
        Ok((decode(bytes)?, None))
    }
}

// WHV_NESTED_STATE is an 8192-byte SDK structure. Its VMX header uses
// CurrentVmcsValid (bit 2) and VmcsEnlightened (bit 4). Ordinary current VMCS
// backing RAM may be stale even when WHP returns the current cached fields;
// restoring that state can later reload stale RAM. Never patch its private
// layout. Require the documented guest-maintained eVMCS representation instead.
fn validate_nested_state(bytes: &[u8]) -> Result<()> {
    if bytes.len() != 8192 {
        return Err(codec("invalid WHP nested execution state size"));
    }
    let vendor = u32::from_le_bytes(bytes[..4].try_into().unwrap());
    let flags = u32::from_le_bytes(bytes[4..8].try_into().unwrap());
    match vendor {
        0 if flags & (1 << 2) != 0 && flags & (1 << 4) == 0 => Err(codec(
            "Intel nested snapshots require Hyper-V enlightened VMCS in the guest kernel",
        )),
        0 | 1 => Ok(()),
        _ => Err(codec("unknown WHP nested execution state vendor")),
    }
}

pub(super) fn restore_cpu(
    partition: WHV_PARTITION_HANDLE,
    id: u32,
    bytes: &[u8],
    nested_enabled: bool,
) -> Result<()> {
    let cpu = u8::try_from(id).map_err(codec)?;
    let (state, nested) = decode_cpu_payload(bytes, nested_enabled)?;
    let names = register_names(partition, cpu)?;
    if names
        != state
            .registers
            .iter()
            .map(|(name, _)| *name)
            .collect::<Vec<_>>()
    {
        return Err(codec("WHP x86 register contract differs"));
    }
    let values = state
        .registers
        .iter()
        .map(|(_, bytes)| super::register_value_from_bytes(bytes))
        .collect::<Vec<_>>();
    super::set_vcpu_registers(partition, cpu, &names, &values)?;
    // The opaque XSAVE payload retains extended state, not just x87/SSE.
    // APIC v2 state includes pending/in-service vectors and timer bookkeeping.
    super::set_virtual_processor_state(
        partition,
        id,
        WHvVirtualProcessorStateTypeXsaveState,
        &state.xsave,
    )?;
    // Nested control state is separate from registers and XSAVE. Keep it ahead
    // of interrupt-controller restoration; live Intel/AMD qualification is required.
    if let Some(nested) = nested {
        let interrupt_state = state
            .registers
            .iter()
            .find(|(name, _)| *name == WHvRegisterInterruptState)
            .ok_or_else(|| codec("missing WHP interrupt state"))?;

        super::set_virtual_processor_state(partition, id, NESTED_STATE, &nested)?;

        // Loading NestedState can overwrite interrupt state restored with the ordinary
        // registers. Reapply the captured value before APIC state so an active nested
        // vCPU can continue receiving the outer guest's timer interrupts.
        super::set_vcpu_registers(
            partition,
            cpu,
            &[WHvRegisterInterruptState],
            &[super::register_value_from_bytes(&interrupt_state.1)],
        )?;
    }
    super::set_virtual_processor_state(
        partition,
        id,
        WHvVirtualProcessorStateTypeInterruptControllerState2,
        &state.apic,
    )?;
    for (kind, payload) in [
        WHvVirtualProcessorStateTypeSynicMessagePage,
        WHvVirtualProcessorStateTypeSynicEventFlagPage,
        WHvVirtualProcessorStateTypeSynicTimerState,
    ]
    .into_iter()
    .zip(&state.synic)
    {
        super::set_virtual_processor_state(partition, id, kind, payload)?;
    }
    Ok(())
}

fn register_names(partition: WHV_PARTITION_HANDLE, id: u8) -> Result<Vec<WHV_REGISTER_NAME>> {
    // General/segment/control/debug state is mandatory. XSAVE is separate.
    let mut names = (WHvX64RegisterRax..=WHvX64RegisterXCr0).collect::<Vec<_>>();
    names.extend([
        WHvX64RegisterTsc,
        WHvX64RegisterEfer,
        WHvX64RegisterKernelGsBase,
        WHvX64RegisterApicBase,
        WHvX64RegisterPat,
        WHvX64RegisterSysenterCs,
        WHvX64RegisterSysenterEip,
        WHvX64RegisterSysenterEsp,
        WHvX64RegisterStar,
        WHvX64RegisterLstar,
        WHvX64RegisterCstar,
        WHvX64RegisterSfmask,
        WHvRegisterPendingInterruption,
        WHvRegisterInterruptState,
        WHvRegisterPendingEvent,
        WHvX64RegisterDeliverabilityNotifications,
        WHvRegisterInternalActivityState,
    ]);
    // Architectural MSRs vary with WHP's exposed feature set. Probe only known
    // register names, retain every supported one, and require the same contract
    // on restore. Unexpected backend failures never mean "optional".
    let mut optional = vec![
        WHvX64RegisterTscAux,
        WHvX64RegisterTscDeadline,
        WHvX64RegisterTscAdjust,
        WHvX64RegisterBndcfgs,
        WHvX64RegisterSpecCtrl,
        WHvX64RegisterTsxCtrl,
        WHvX64RegisterXss,
        WHvX64RegisterUCet,
        WHvX64RegisterSCet,
        WHvX64RegisterSsp,
        WHvX64RegisterPl0Ssp,
        WHvX64RegisterPl1Ssp,
        WHvX64RegisterPl2Ssp,
        WHvX64RegisterPl3Ssp,
        WHvX64RegisterInterruptSspTableAddr,
        WHvX64RegisterUmwaitControl,
        WHvX64RegisterXfd,
        WHvX64RegisterXfdErr,
        WHvX64RegisterPendingDebugException,
        WHvX64RegisterMsrMtrrDefType,
        // Hyper-V requires the guest OS ID before enabling the hypercall page.
        WHvRegisterGuestOsId,
        WHvX64RegisterHypercall,
        WHvRegisterVpAssistPage,
        WHvRegisterReferenceTsc,
        WHvRegisterReferenceTscSequence,
        WHvRegisterScontrol,
        WHvRegisterSiefp,
        WHvRegisterSimp,
    ];
    optional.extend(WHvX64RegisterMsrMtrrPhysBase0..=WHvX64RegisterMsrMtrrPhysBaseF);
    optional.extend(WHvX64RegisterMsrMtrrPhysMask0..=WHvX64RegisterMsrMtrrPhysMaskF);
    optional.extend(WHvX64RegisterMsrMtrrFix64k00000..=WHvX64RegisterMsrMtrrFix4kF8000);
    optional.extend(WHvRegisterSint0..=WHvRegisterSint15);
    for name in optional {
        match super::get_vcpu_register_bytes(partition, id, &[name]) {
            Ok(_) => names.push(name),
            Err(Error::GetVirtualProcessorRegisters { hresult, .. })
                if matches!(hresult as u32, 0x8007_0057 | 0xc035_0005 | 0x8037_0302) => {}
            Err(error) => return Err(error),
        }
    }
    Ok(names)
}

fn features(partition: WHV_PARTITION_HANDLE, nested_enabled: bool) -> Result<Vec<Vec<u8>>> {
    let mut codes = vec![
        WHvPartitionPropertyCodeProcessorFeaturesBanks,
        WHvPartitionPropertyCodeProcessorXsaveFeatures,
        WHvPartitionPropertyCodeLocalApicEmulationMode,
        WHvPartitionPropertyCodeInterruptClockFrequency,
    ];
    if nested_enabled {
        codes.push(WHvPartitionPropertyCodeSyntheticProcessorFeaturesBanks);
    }
    codes
        .into_iter()
        .map(|code| {
            let mut buffer = [0_u64; 32];
            let mut written = 0;
            let status = unsafe {
                WHvGetPartitionProperty(
                    partition,
                    code,
                    buffer.as_mut_ptr().cast(),
                    std::mem::size_of_val(&buffer) as u32,
                    &mut written,
                )
            };
            if status < 0 || written == 0 || written as usize > std::mem::size_of_val(&buffer) {
                return Err(codec(format!(
                    "partition feature {code:#x}: HRESULT {status:#x}"
                )));
            }
            let bytes = buffer
                .iter()
                .flat_map(|value| value.to_le_bytes())
                .take(written as usize)
                .collect();
            Ok(bytes)
        })
        .collect()
}

fn reference_time(partition: WHV_PARTITION_HANDLE) -> Result<u64> {
    let mut value = 0u64;
    let mut written = 0;
    let status = unsafe {
        WHvGetPartitionProperty(
            partition,
            WHvPartitionPropertyCodeReferenceTime,
            (&mut value as *mut u64).cast(),
            8,
            &mut written,
        )
    };
    if status < 0 || written != 8 {
        return Err(codec(format!(
            "WHP reference time capture: HRESULT {status:#x}"
        )));
    }
    Ok(value)
}

pub(super) fn tsc_frequency_hz(partition: WHV_PARTITION_HANDLE) -> Result<u64> {
    let mut value = 0_u64;
    let mut written = 0;
    let status = unsafe {
        WHvGetPartitionProperty(
            partition,
            WHvPartitionPropertyCodeProcessorClockFrequency,
            (&mut value as *mut u64).cast(),
            8,
            &mut written,
        )
    };
    if status < 0 || written != 8 || value == 0 {
        return Err(codec("WHP partition clock frequency unavailable"));
    }
    Ok(value)
}

pub(crate) fn saved_frequency(bytes: &[u8], nested: bool) -> Result<u64> {
    let state: VmState = if nested {
        let (state, _time): (VmState, u64) = decode(bytes)?;
        state
    } else {
        decode(bytes)?
    };
    if state.tsc_frequency_hz == 0 {
        return Err(codec("zero saved TSC frequency"));
    }
    Ok(state.tsc_frequency_hz)
}

fn encode<T: Serialize>(state: &T) -> Result<Vec<u8>> {
    bincode::serde::encode_to_vec(state, bincode::config::standard()).map_err(codec)
}

fn decode<T: serde::de::DeserializeOwned>(bytes: &[u8]) -> Result<T> {
    let (state, used) = bincode::serde::decode_from_slice(
        bytes,
        bincode::config::standard().with_limit::<{ 64 * 1024 * 1024 }>(),
    )
    .map_err(codec)?;
    if used != bytes.len() {
        return Err(codec("trailing WHP x86 state bytes"));
    }
    Ok(state)
}

fn codec(error: impl std::fmt::Display) -> Error {
    Error::StateCodec(error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nested_snapshot_rejects_an_ordinary_current_vmx_cache() {
        let mut state = vec![0; 8192];
        state[4..8].copy_from_slice(&6u32.to_le_bytes());
        assert!(validate_nested_state(&state)
            .unwrap_err()
            .to_string()
            .contains("enlightened VMCS"));
        state[4..8].copy_from_slice(&54u32.to_le_bytes());
        assert!(validate_nested_state(&state).is_ok());
        state[..4].copy_from_slice(&1u32.to_le_bytes());
        state[4..8].copy_from_slice(&6u32.to_le_bytes());
        assert!(validate_nested_state(&state).is_ok());
        state[..4].copy_from_slice(&2u32.to_le_bytes());
        assert!(validate_nested_state(&state).is_err());
        assert!(validate_nested_state(&state[..8191]).is_err());
    }

    #[test]
    fn nested_execution_payload_preserves_state_and_rejects_corruption() {
        let state = CpuState {
            registers: vec![(WHvX64RegisterRip, [0x42; 16])],
            xsave: vec![1, 2, 3],
            apic: vec![4, 5],
            synic: [vec![6], vec![7], vec![8]],
        };
        let legacy = encode(&state).unwrap();
        assert_eq!(encode_cpu_payload(&state, None).unwrap(), legacy);
        let (ordinary, nested) = decode_cpu_payload(&legacy, false).unwrap();
        assert_eq!(ordinary.registers, state.registers);
        assert!(nested.is_none());

        let mut nested = vec![0x5a; 8192];
        nested[..4].copy_from_slice(&0u32.to_le_bytes());
        nested[4..8].copy_from_slice(&54u32.to_le_bytes());
        let payload = encode_cpu_payload(&state, Some(&nested)).unwrap();
        let (restored, restored_nested) = decode_cpu_payload(&payload, true).unwrap();
        assert_eq!(restored_nested.unwrap(), nested);
        assert_eq!(restored.registers, vec![(WHvX64RegisterRip, [0x42; 16])]);
        assert_eq!(restored.xsave, vec![1, 2, 3]);
        assert_eq!(restored.apic, vec![4, 5]);
        assert_eq!(restored.synic, [vec![6], vec![7], vec![8]]);
        assert_eq!(encode_cpu_payload(&restored, None).unwrap(), legacy);

        assert!(encode_cpu_payload(&state, Some(&[])).is_err());
        let empty = encode(&(&state, Vec::<u8>::new())).unwrap();
        assert!(decode_cpu_payload(&empty, true).is_err());
        assert!(decode_cpu_payload(&legacy, true).is_err());
        assert!(decode_cpu_payload(&payload[..payload.len() - 1], true).is_err());
        let mut trailing = payload;
        trailing.push(0);
        assert!(decode_cpu_payload(&trailing, true).is_err());
    }
}
