// Copyright 2021 Red Hat, Inc.
// SPDX-License-Identifier: Apache-2.0

#[allow(non_camel_case_types)]
#[allow(improper_ctypes)]
#[allow(dead_code)]
#[allow(non_snake_case)]
#[allow(non_upper_case_globals)]
#[allow(deref_nullptr)]
pub mod bindings;

#[macro_use]
extern crate log;

use bindings::*;

#[cfg(target_arch = "aarch64")]
use std::arch::asm;

use std::convert::TryInto;
use std::fmt::{Display, Formatter};
use std::sync::{Arc, LazyLock};
use std::time::Duration;

#[cfg(all(target_arch = "aarch64", target_os = "macos"))]
use arch::aarch64::sysreg::{sys_reg_name, SYSREG_MASK};
use log::debug;
use serde::{Deserialize, Serialize};

//--------------------------------------------------------------------------------------------------
// Constants
//--------------------------------------------------------------------------------------------------

const GENERAL_REGISTER_COUNT: usize = 35;
const SIMD_REGISTER_COUNT: usize = 32;

// Hypervisor.framework does not expose a bulk architectural-state operation for a vCPU. Keep the
// exact writable register set explicit so a framework update cannot silently change the artifact.
const EL1_SYSTEM_REGISTERS: &[hv_sys_reg_t] = &[
    hv_sys_reg_t_HV_SYS_REG_SCTLR_EL1,
    hv_sys_reg_t_HV_SYS_REG_ACTLR_EL1,
    hv_sys_reg_t_HV_SYS_REG_CPACR_EL1,
    hv_sys_reg_t_HV_SYS_REG_TTBR0_EL1,
    hv_sys_reg_t_HV_SYS_REG_TTBR1_EL1,
    hv_sys_reg_t_HV_SYS_REG_TCR_EL1,
    hv_sys_reg_t_HV_SYS_REG_APIAKEYLO_EL1,
    hv_sys_reg_t_HV_SYS_REG_APIAKEYHI_EL1,
    hv_sys_reg_t_HV_SYS_REG_APIBKEYLO_EL1,
    hv_sys_reg_t_HV_SYS_REG_APIBKEYHI_EL1,
    hv_sys_reg_t_HV_SYS_REG_APDAKEYLO_EL1,
    hv_sys_reg_t_HV_SYS_REG_APDAKEYHI_EL1,
    hv_sys_reg_t_HV_SYS_REG_APDBKEYLO_EL1,
    hv_sys_reg_t_HV_SYS_REG_APDBKEYHI_EL1,
    hv_sys_reg_t_HV_SYS_REG_APGAKEYLO_EL1,
    hv_sys_reg_t_HV_SYS_REG_APGAKEYHI_EL1,
    hv_sys_reg_t_HV_SYS_REG_SPSR_EL1,
    hv_sys_reg_t_HV_SYS_REG_ELR_EL1,
    hv_sys_reg_t_HV_SYS_REG_SP_EL0,
    hv_sys_reg_t_HV_SYS_REG_SP_EL1,
    hv_sys_reg_t_HV_SYS_REG_AFSR0_EL1,
    hv_sys_reg_t_HV_SYS_REG_AFSR1_EL1,
    hv_sys_reg_t_HV_SYS_REG_ESR_EL1,
    hv_sys_reg_t_HV_SYS_REG_FAR_EL1,
    hv_sys_reg_t_HV_SYS_REG_PAR_EL1,
    hv_sys_reg_t_HV_SYS_REG_MAIR_EL1,
    hv_sys_reg_t_HV_SYS_REG_AMAIR_EL1,
    hv_sys_reg_t_HV_SYS_REG_VBAR_EL1,
    hv_sys_reg_t_HV_SYS_REG_CONTEXTIDR_EL1,
    hv_sys_reg_t_HV_SYS_REG_TPIDR_EL1,
    hv_sys_reg_t_HV_SYS_REG_TPIDR_EL0,
    hv_sys_reg_t_HV_SYS_REG_TPIDRRO_EL0,
    hv_sys_reg_t_HV_SYS_REG_CNTKCTL_EL1,
    hv_sys_reg_t_HV_SYS_REG_CSSELR_EL1,
    hv_sys_reg_t_HV_SYS_REG_CNTV_CTL_EL0,
    hv_sys_reg_t_HV_SYS_REG_CNTV_CVAL_EL0,
    hv_sys_reg_t_HV_SYS_REG_CNTP_CTL_EL0,
    hv_sys_reg_t_HV_SYS_REG_CNTP_CVAL_EL0,
];

const EL2_SYSTEM_REGISTERS: &[hv_sys_reg_t] = &[
    hv_sys_reg_t_HV_SYS_REG_CNTHCTL_EL2,
    hv_sys_reg_t_HV_SYS_REG_CNTHP_CTL_EL2,
    hv_sys_reg_t_HV_SYS_REG_CNTHP_CVAL_EL2,
    hv_sys_reg_t_HV_SYS_REG_CNTVOFF_EL2,
    hv_sys_reg_t_HV_SYS_REG_CPTR_EL2,
    hv_sys_reg_t_HV_SYS_REG_ELR_EL2,
    hv_sys_reg_t_HV_SYS_REG_ESR_EL2,
    hv_sys_reg_t_HV_SYS_REG_FAR_EL2,
    hv_sys_reg_t_HV_SYS_REG_HCR_EL2,
    hv_sys_reg_t_HV_SYS_REG_HPFAR_EL2,
    hv_sys_reg_t_HV_SYS_REG_MAIR_EL2,
    hv_sys_reg_t_HV_SYS_REG_MDCR_EL2,
    hv_sys_reg_t_HV_SYS_REG_SCTLR_EL2,
    hv_sys_reg_t_HV_SYS_REG_SPSR_EL2,
    hv_sys_reg_t_HV_SYS_REG_SP_EL2,
    hv_sys_reg_t_HV_SYS_REG_TCR_EL2,
    hv_sys_reg_t_HV_SYS_REG_TPIDR_EL2,
    hv_sys_reg_t_HV_SYS_REG_TTBR0_EL2,
    hv_sys_reg_t_HV_SYS_REG_TTBR1_EL2,
    hv_sys_reg_t_HV_SYS_REG_VBAR_EL2,
    hv_sys_reg_t_HV_SYS_REG_VMPIDR_EL2,
    hv_sys_reg_t_HV_SYS_REG_VPIDR_EL2,
    hv_sys_reg_t_HV_SYS_REG_VTCR_EL2,
    hv_sys_reg_t_HV_SYS_REG_VTTBR_EL2,
];

// Hypervisor.framework exposes the GIC CPU interface separately from ordinary vCPU system
// registers. Keep only architecturally writable state here; RPR_EL1 is read-only.
const ICC_REGISTERS: &[hv_gic_icc_reg_t] = &[
    hv_gic_icc_reg_t_HV_GIC_ICC_REG_CTLR_EL1,
    hv_gic_icc_reg_t_HV_GIC_ICC_REG_PMR_EL1,
    hv_gic_icc_reg_t_HV_GIC_ICC_REG_BPR0_EL1,
    hv_gic_icc_reg_t_HV_GIC_ICC_REG_BPR1_EL1,
    hv_gic_icc_reg_t_HV_GIC_ICC_REG_AP0R0_EL1,
    hv_gic_icc_reg_t_HV_GIC_ICC_REG_AP1R0_EL1,
    // Restore interface and interrupt-group enables only after priority and active state.
    hv_gic_icc_reg_t_HV_GIC_ICC_REG_SRE_EL1,
    hv_gic_icc_reg_t_HV_GIC_ICC_REG_IGRPEN0_EL1,
    hv_gic_icc_reg_t_HV_GIC_ICC_REG_IGRPEN1_EL1,
];

// The EL2 system-register interface and virtual interrupt controller exist only for nested VMs.
const NESTED_ICC_REGISTERS: &[hv_gic_icc_reg_t] = &[hv_gic_icc_reg_t_HV_GIC_ICC_REG_SRE_EL2];
const NESTED_ICH_REGISTERS: &[hv_gic_ich_reg_t] = &[
    hv_gic_ich_reg_t_HV_GIC_ICH_REG_AP0R0_EL2,
    hv_gic_ich_reg_t_HV_GIC_ICH_REG_AP1R0_EL2,
    hv_gic_ich_reg_t_HV_GIC_ICH_REG_VMCR_EL2,
    hv_gic_ich_reg_t_HV_GIC_ICH_REG_LR0_EL2,
    hv_gic_ich_reg_t_HV_GIC_ICH_REG_LR1_EL2,
    hv_gic_ich_reg_t_HV_GIC_ICH_REG_LR2_EL2,
    hv_gic_ich_reg_t_HV_GIC_ICH_REG_LR3_EL2,
    hv_gic_ich_reg_t_HV_GIC_ICH_REG_LR4_EL2,
    hv_gic_ich_reg_t_HV_GIC_ICH_REG_LR5_EL2,
    hv_gic_ich_reg_t_HV_GIC_ICH_REG_LR6_EL2,
    hv_gic_ich_reg_t_HV_GIC_ICH_REG_LR7_EL2,
    hv_gic_ich_reg_t_HV_GIC_ICH_REG_LR8_EL2,
    hv_gic_ich_reg_t_HV_GIC_ICH_REG_LR9_EL2,
    hv_gic_ich_reg_t_HV_GIC_ICH_REG_LR10_EL2,
    hv_gic_ich_reg_t_HV_GIC_ICH_REG_LR11_EL2,
    hv_gic_ich_reg_t_HV_GIC_ICH_REG_LR12_EL2,
    hv_gic_ich_reg_t_HV_GIC_ICH_REG_LR13_EL2,
    hv_gic_ich_reg_t_HV_GIC_ICH_REG_LR14_EL2,
    hv_gic_ich_reg_t_HV_GIC_ICH_REG_LR15_EL2,
    // Enable the virtual interface only after its active-priority, control, and list registers are
    // restored, so a pending virtual interrupt cannot become visible through partial state.
    hv_gic_ich_reg_t_HV_GIC_ICH_REG_HCR_EL2,
];

#[derive(Clone, Copy)]
#[repr(C)]
struct MachTimebaseInfo {
    numer: u32,
    denom: u32,
}

extern "C" {
    pub fn mach_absolute_time() -> u64;
    fn mach_timebase_info(info: *mut MachTimebaseInfo) -> i32;
}

static MACH_TIMEBASE_INFO: LazyLock<MachTimebaseInfo> = LazyLock::new(|| {
    let mut info = MachTimebaseInfo { numer: 1, denom: 1 };
    let ret = unsafe { mach_timebase_info(&mut info) };
    if ret == 0 && info.denom != 0 {
        info
    } else {
        MachTimebaseInfo { numer: 1, denom: 1 }
    }
});

const HV_EXIT_REASON_CANCELED: hv_exit_reason_t = 0;
const HV_EXIT_REASON_EXCEPTION: hv_exit_reason_t = 1;
const HV_EXIT_REASON_VTIMER_ACTIVATED: hv_exit_reason_t = 2;

const TMR_CTL_ENABLE: u64 = 1 << 0;
const TMR_CTL_IMASK: u64 = 1 << 1;
const TMR_CTL_ISTATUS: u64 = 1 << 2;

const PSR_MODE_EL1H: u64 = 0x0000_0005;
const PSR_MODE_EL2H: u64 = 0x0000_0009;
const PSR_F_BIT: u64 = 0x0000_0040;
const PSR_I_BIT: u64 = 0x0000_0080;
const PSR_A_BIT: u64 = 0x0000_0100;
const PSR_D_BIT: u64 = 0x0000_0200;
const PSTATE_EL1_FAULT_BITS_64: u64 = PSR_MODE_EL1H | PSR_A_BIT | PSR_F_BIT | PSR_I_BIT | PSR_D_BIT;
const PSTATE_EL2_FAULT_BITS_64: u64 = PSR_MODE_EL2H | PSR_A_BIT | PSR_F_BIT | PSR_I_BIT | PSR_D_BIT;

// Architectural reset value of SCTLR_EL1: only the RES1 bits (11, 20, 22, 23, 28, 29)
// set, so MMU, caches, and alignment checking are all disabled.
const SCTLR_EL1_RES1: u64 = (1 << 11) | (1 << 20) | (1 << 22) | (1 << 23) | (1 << 28) | (1 << 29);

const HCR_TLOR: u64 = 1 << 35;
const HCR_RW: u64 = 1 << 31;
const HCR_TSW: u64 = 1 << 22;
const HCR_TACR: u64 = 1 << 21;
const HCR_TIDCP: u64 = 1 << 20;
const HCR_TSC: u64 = 1 << 19;
const HCR_TID3: u64 = 1 << 18;
const HCR_TWE: u64 = 1 << 14;
const HCR_TWI: u64 = 1 << 13;
const HCR_BSU_IS: u64 = 1 << 10;
const HCR_FB: u64 = 1 << 9;
const HCR_AMO: u64 = 1 << 5;
const HCR_IMO: u64 = 1 << 4;
const HCR_FMO: u64 = 1 << 3;
const HCR_PTW: u64 = 1 << 2;
const HCR_SWIO: u64 = 1 << 1;
const HCR_VM: u64 = 1 << 0;
// Use the same bits as KVM uses in vcpu reset.
const HCR_EL2_BITS: u64 = HCR_TSC
    | HCR_TSW
    | HCR_TWE
    | HCR_TWI
    | HCR_VM
    | HCR_BSU_IS
    | HCR_FB
    | HCR_TACR
    | HCR_AMO
    | HCR_SWIO
    | HCR_TIDCP
    | HCR_RW
    | HCR_TLOR
    | HCR_FMO
    | HCR_IMO
    | HCR_PTW
    | HCR_TID3;

const CNTHCTL_EL0VCTEN: u64 = 1 << 1;
const CNTHCTL_EL0PCTEN: u64 = 1 << 0;
// Trap accesses to both virtual and physical counter registers.
const CNTHCTL_EL2_BITS: u64 = CNTHCTL_EL0VCTEN | CNTHCTL_EL0PCTEN;

const AA64PFR0_EL1_EL2EN: u64 = 1 << 8;
const AA64PFR0_EL1_GIC3EN: u64 = 1 << 24;
const AA64PFR1_EL1_SMEMASK: u64 = 3 << 24;

const EC_WFX_TRAP: u64 = 0x1;
const EC_AA64_HVC: u64 = 0x16;
const EC_AA64_SMC: u64 = 0x17;
#[cfg(all(target_arch = "aarch64", target_os = "macos"))]
const EC_SYSTEMREGISTERTRAP: u64 = 0x18;
const EC_DATAABORT: u64 = 0x24;
const EC_AA64_BKPT: u64 = 0x3c;

#[derive(Debug)]
pub enum Error {
    EnableEL2,
    FindSymbol(libloading::Error),
    MemoryMap,
    MemoryProtect,
    MemoryUnmap,
    NestedCheck,
    VcpuCreate,
    VcpuInitialRegisters,
    VcpuReadRegister,
    VcpuReadPendingInterrupt,
    VcpuReadGicRegister(&'static str, u16),
    VcpuReadSimdRegister,
    VcpuReadSystemRegister,
    VcpuReadVtimer,
    VcpuRequestExit,
    VcpuRun,
    VcpuSetPendingIrq,
    VcpuSetGicRegister(&'static str, u16, u64),
    VcpuSetRegister,
    VcpuSetSimdRegister,
    VcpuSetSystemRegister(u16, u64),
    VcpuSetVtimerMask,
    VcpuSetVtimerOffset,
    VcpuStatePendingMmio,
    VcpuStateTopology,
    GicStateCreate,
    GicStateRead,
    GicStateWrite,
    VmCreate,
}

impl Display for Error {
    fn fmt(&self, f: &mut Formatter) -> std::fmt::Result {
        use self::Error::*;

        match self {
            EnableEL2 => write!(f, "Error enabling EL2 mode in HVF"),
            FindSymbol(ref err) => write!(f, "Couldn't find symbol in HVF library: {err}"),
            MemoryMap => write!(f, "Error registering memory region in HVF"),
            MemoryProtect => write!(f, "Error changing HVF memory permissions"),
            MemoryUnmap => write!(f, "Error unregistering memory region in HVF"),
            NestedCheck => write!(
                f,
                "Nested virtualization was requested but it's not support in this system"
            ),
            VcpuCreate => write!(f, "Error creating HVF vCPU instance"),
            VcpuInitialRegisters => write!(f, "Error setting up initial HVF vCPU registers"),
            VcpuReadRegister => write!(f, "Error reading HVF vCPU register"),
            VcpuReadPendingInterrupt => write!(f, "Error reading HVF vCPU interrupt state"),
            VcpuReadGicRegister(kind, reg) => {
                write!(f, "Error reading HVF vCPU GIC {kind} register {reg:#x}")
            }
            VcpuReadSimdRegister => write!(f, "Error reading HVF vCPU SIMD register"),
            VcpuReadSystemRegister => write!(f, "Error reading HVF vCPU system register"),
            VcpuReadVtimer => write!(f, "Error reading HVF vCPU virtual timer state"),
            VcpuRequestExit => write!(f, "Error requesting HVF vCPU exit"),
            VcpuRun => write!(f, "Error running HVF vCPU"),
            VcpuSetPendingIrq => write!(f, "Error setting HVF vCPU pending irq"),
            VcpuSetGicRegister(kind, reg, value) => write!(
                f,
                "Error setting HVF vCPU GIC {kind} register {reg:#x} to {value:#x}"
            ),
            VcpuSetRegister => write!(f, "Error setting HVF vCPU register"),
            VcpuSetSimdRegister => write!(f, "Error setting HVF vCPU SIMD register"),
            VcpuSetSystemRegister(reg, val) => write!(
                f,
                "Error setting HVF vCPU system register 0x{reg:#x} to 0x{val:#x}"
            ),
            VcpuSetVtimerMask => write!(f, "Error setting HVF vCPU vtimer mask"),
            VcpuSetVtimerOffset => write!(f, "Error setting HVF vCPU vtimer offset"),
            VcpuStatePendingMmio => write!(f, "HVF vCPU has an incomplete MMIO read"),
            VcpuStateTopology => write!(f, "HVF vCPU state does not match its configuration"),
            GicStateCreate => write!(f, "Error creating an HVF GIC state object"),
            GicStateRead => write!(f, "Error reading HVF GIC state"),
            GicStateWrite => write!(f, "Error restoring HVF GIC state"),
            VmCreate => write!(f, "Error creating HVF VM instance"),
        }
    }
}

pub enum InterruptType {
    Irq,
    Fiq,
}

pub trait Vcpus {
    fn set_vtimer_irq(&self, vcpuid: u64);
    fn should_wait(&self, vcpuid: u64) -> bool;
    fn has_pending_irq(&self, vcpuid: u64) -> bool;
    fn get_pending_irq(&self, vcpuid: u64) -> u32;
    fn handle_sysreg_read(&self, vcpuid: u64, reg: u32) -> Option<u64>;
    fn handle_sysreg_write(&self, vcpuid: u64, reg: u32, val: u64) -> bool;
}

pub fn vcpu_request_exit(vcpuid: u64) -> Result<(), Error> {
    let mut vcpu: u64 = vcpuid;
    let ret = unsafe { hv_vcpus_exit(&mut vcpu, 1) };

    if ret != HV_SUCCESS {
        Err(Error::VcpuRequestExit)
    } else {
        Ok(())
    }
}

fn mach_absolute_time_to_ns(ticks: u64) -> u64 {
    let timebase = *MACH_TIMEBASE_INFO;
    let ns = (u128::from(ticks) * u128::from(timebase.numer)) / u128::from(timebase.denom);
    ns.min(u128::from(u64::MAX)) as u64
}

/// Host virtual-counter tick ratio, checked rather than defaulted for execution-state admission.
pub fn counter_timebase() -> Option<(u32, u32)> {
    let mut info = MachTimebaseInfo { numer: 0, denom: 0 };
    let ret = unsafe { mach_timebase_info(&mut info) };
    (ret == 0 && info.numer != 0 && info.denom != 0).then_some((info.numer, info.denom))
}

pub fn vcpu_set_pending_irq(
    vcpuid: u64,
    irq_type: InterruptType,
    pending: bool,
) -> Result<(), Error> {
    let _type = match irq_type {
        InterruptType::Irq => hv_interrupt_type_t_HV_INTERRUPT_TYPE_IRQ,
        InterruptType::Fiq => hv_interrupt_type_t_HV_INTERRUPT_TYPE_FIQ,
    };

    let ret = unsafe { hv_vcpu_set_pending_interrupt(vcpuid, _type, pending) };

    if ret != HV_SUCCESS {
        Err(Error::VcpuSetPendingIrq)
    } else {
        Ok(())
    }
}

pub fn vcpu_set_vtimer_mask(vcpuid: u64, masked: bool) -> Result<(), Error> {
    let ret = unsafe { hv_vcpu_set_vtimer_mask(vcpuid, masked) };

    if ret != HV_SUCCESS {
        Err(Error::VcpuSetVtimerMask)
    } else {
        Ok(())
    }
}

/// Checks if Nested Virtualization is supported on the current system. Only
/// M3 or newer chips on macOS 15+ will satisfy the requirements.
pub fn check_nested_virt() -> Result<bool, Error> {
    type GetEL2Supported =
        libloading::Symbol<'static, unsafe extern "C" fn(*mut bool) -> hv_return_t>;

    let get_el2_supported: Result<GetEL2Supported, libloading::Error> =
        unsafe { HVF.get(b"hv_vm_config_get_el2_supported") };
    if get_el2_supported.is_err() {
        info!("cannot find hv_vm_config_get_el2_supported symbol");
        return Ok(false);
    }

    let mut el2_supported: bool = false;
    let ret = unsafe { (get_el2_supported.unwrap())(&mut el2_supported) };
    if ret != HV_SUCCESS {
        error!("hv_vm_config_get_el2_supported failed: {ret:?}");
        return Err(Error::NestedCheck);
    }

    Ok(el2_supported)
}

pub struct HvfVm {}

static HVF: LazyLock<libloading::Library> = LazyLock::new(|| unsafe {
    libloading::Library::new(
        "/System/Library/Frameworks/Hypervisor.framework/Versions/A/Hypervisor",
    )
    .unwrap()
});

impl HvfVm {
    pub fn new(nested_enabled: bool) -> Result<Self, Error> {
        let config = unsafe { hv_vm_config_create() };
        if nested_enabled {
            let set_el2_enabled: libloading::Symbol<
                'static,
                unsafe extern "C" fn(hv_vm_config_t, bool) -> hv_return_t,
            > = unsafe {
                HVF.get(b"hv_vm_config_set_el2_enabled")
                    .map_err(Error::FindSymbol)?
            };

            let ret = unsafe { (set_el2_enabled)(config, true) };
            if ret != HV_SUCCESS {
                return Err(Error::EnableEL2);
            }
        }

        let ret = unsafe { hv_vm_create(config) };

        if ret != HV_SUCCESS {
            Err(Error::VmCreate)
        } else {
            Ok(Self {})
        }
    }

    pub fn map_memory(
        &self,
        host_start_addr: u64,
        guest_start_addr: u64,
        size: u64,
    ) -> Result<(), Error> {
        let ret = unsafe {
            hv_vm_map(
                host_start_addr as *mut core::ffi::c_void,
                guest_start_addr,
                size.try_into().unwrap(),
                (HV_MEMORY_READ | HV_MEMORY_WRITE | HV_MEMORY_EXEC).into(),
            )
        };
        if ret != HV_SUCCESS {
            Err(Error::MemoryMap)
        } else {
            Ok(())
        }
    }

    pub fn unmap_memory(&self, guest_start_addr: u64, size: u64) -> Result<(), Error> {
        let ret = unsafe { hv_vm_unmap(guest_start_addr, size.try_into().unwrap()) };
        if ret != HV_SUCCESS {
            Err(Error::MemoryUnmap)
        } else {
            Ok(())
        }
    }

    /// Changes the second-stage permissions for an existing guest-memory mapping.
    pub fn protect_memory(
        &self,
        guest_start_addr: u64,
        size: u64,
        writable: bool,
    ) -> Result<(), Error> {
        protect_memory(guest_start_addr, size, writable)
    }

    /// Captures the complete in-kernel GIC state into an opaque framework-owned byte stream.
    pub fn capture_gic_state(&self) -> Result<Vec<u8>, Error> {
        type StateCreate = unsafe extern "C" fn() -> hv_gic_state_t;
        type StateGetSize = unsafe extern "C" fn(hv_gic_state_t, *mut usize) -> hv_return_t;
        type StateGetData =
            unsafe extern "C" fn(hv_gic_state_t, *mut core::ffi::c_void) -> hv_return_t;

        let create: libloading::Symbol<'static, StateCreate> =
            unsafe { HVF.get(b"hv_gic_state_create") }.map_err(Error::FindSymbol)?;
        let get_size: libloading::Symbol<'static, StateGetSize> =
            unsafe { HVF.get(b"hv_gic_state_get_size") }.map_err(Error::FindSymbol)?;
        let get_data: libloading::Symbol<'static, StateGetData> =
            unsafe { HVF.get(b"hv_gic_state_get_data") }.map_err(Error::FindSymbol)?;

        let state = unsafe { create() };
        if state.is_null() {
            return Err(Error::GicStateCreate);
        }
        let mut size = 0usize;
        let size_result = unsafe { get_size(state, &mut size) };
        if size_result != HV_SUCCESS || size == 0 {
            unsafe { os_release(state.cast()) };
            return Err(Error::GicStateRead);
        }
        let mut bytes = vec![0_u8; size];
        let data_result = unsafe { get_data(state, bytes.as_mut_ptr().cast()) };
        unsafe { os_release(state.cast()) };
        if data_result != HV_SUCCESS {
            return Err(Error::GicStateRead);
        }
        Ok(bytes)
    }

    /// Restores a byte stream previously returned by [`Self::capture_gic_state`].
    pub fn restore_gic_state(&self, bytes: &[u8]) -> Result<(), Error> {
        type SetState = unsafe extern "C" fn(*const core::ffi::c_void, usize) -> hv_return_t;
        let set_state: libloading::Symbol<'static, SetState> =
            unsafe { HVF.get(b"hv_gic_set_state") }.map_err(Error::FindSymbol)?;
        let result = unsafe { set_state(bytes.as_ptr().cast(), bytes.len()) };
        if result == HV_SUCCESS {
            Ok(())
        } else {
            Err(Error::GicStateWrite)
        }
    }
}

/// Changes the second-stage permissions for an existing guest-memory mapping.
///
/// This free form is used by vCPU threads servicing first-write faults; Hypervisor.framework owns
/// one process-wide VM, so no host-side `HvfVm` handle is required by the API.
pub fn protect_memory(guest_start_addr: u64, size: u64, writable: bool) -> Result<(), Error> {
    let mut flags = HV_MEMORY_READ | HV_MEMORY_EXEC;
    if writable {
        flags |= HV_MEMORY_WRITE;
    }
    let ret = unsafe {
        hv_vm_protect(
            guest_start_addr,
            size.try_into().map_err(|_| Error::MemoryProtect)?,
            flags.into(),
        )
    };
    if ret != HV_SUCCESS {
        Err(Error::MemoryProtect)
    } else {
        Ok(())
    }
}

#[derive(Debug)]
pub enum VcpuExit<'a> {
    AffinityInfo(u64),
    Breakpoint,
    Canceled,
    CpuOff,
    CpuOn(u64, u64, u64),
    HypervisorCall,
    MmioRead(u64, &'a mut [u8]),
    MmioWrite(u64, &'a [u8]),
    PsciHandled,
    SecureMonitorCall,
    Shutdown,
    SystemRegister,
    VtimerActivated,
    WaitForEvent,
    WaitForEventExpired,
    WaitForEventTimeout(Duration),
}

struct MmioRead {
    addr: u64,
    len: usize,
    srt: u32,
}

/// Complete architectural and framework state for one HVF vCPU.
///
/// Register identifiers are kept in the payload to make decoding reject a build whose explicit
/// register contract differs, instead of applying values to a shifted positional list.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct HvfVcpuState {
    general: Vec<(hv_reg_t, u64)>,
    simd: Vec<(hv_simd_fp_reg_t, u128)>,
    system: Vec<(hv_sys_reg_t, u64)>,
    icc: Vec<(hv_gic_icc_reg_t, u64)>,
    ich: Vec<(hv_gic_ich_reg_t, u64)>,
    pending_irq: bool,
    pending_fiq: bool,
    vtimer_mask: bool,
    vtimer_offset: u64,
    pending_advance_pc: bool,
    nested_enabled: bool,
}

impl HvfVcpuState {
    /// Rebase the saved virtual counter by the VM-wide host-counter displacement.
    /// Every vCPU must receive the same displacement before any of them resumes.
    pub fn rebase_timer_offset(&mut self, displacement: u64) {
        self.vtimer_offset = self.vtimer_offset.wrapping_add(displacement);
    }

    fn has_register_topology(&self, nested_enabled: bool) -> bool {
        self.nested_enabled == nested_enabled
            && self
                .general
                .iter()
                .map(|(register, _)| *register)
                .eq(0..GENERAL_REGISTER_COUNT as hv_reg_t)
            && self
                .simd
                .iter()
                .map(|(register, _)| *register)
                .eq(0..SIMD_REGISTER_COUNT as hv_simd_fp_reg_t)
            && self
                .system
                .iter()
                .map(|(register, _)| *register)
                .eq(state_system_registers(nested_enabled))
            && self
                .icc
                .iter()
                .map(|(register, _)| *register)
                .eq(state_icc_registers(nested_enabled))
            && self
                .ich
                .iter()
                .map(|(register, _)| *register)
                .eq(state_ich_registers(nested_enabled))
    }
}

pub struct HvfVcpu<'a> {
    vcpuid: hv_vcpu_t,
    vcpu_exit: &'a hv_vcpu_exit_t,
    cntfrq: u64,
    mmio_buf: [u8; 8],
    pending_mmio_read: Option<MmioRead>,
    pending_advance_pc: bool,
    vtimer_masked: bool,
    nested_enabled: bool,
}

impl HvfVcpu<'_> {
    pub fn new(mpidr: u64, nested_enabled: bool) -> Result<Self, Error> {
        let mut vcpuid: hv_vcpu_t = 0;
        let vcpu_exit_ptr: *mut hv_vcpu_exit_t = std::ptr::null_mut();

        #[cfg(target_arch = "aarch64")]
        let cntfrq = {
            let cntfrq: u64;
            unsafe { asm!("mrs {}, cntfrq_el0", out(reg) cntfrq) };
            cntfrq
        };
        #[cfg(target_arch = "x86_64")]
        let cntfrq = 0u64;
        #[cfg(target_arch = "riscv64")]
        let cntfrq = 0u64;

        let ret = unsafe {
            hv_vcpu_create(
                &mut vcpuid,
                &vcpu_exit_ptr as *const _ as *mut *mut _,
                std::ptr::null_mut(),
            )
        };
        if ret != HV_SUCCESS {
            return Err(Error::VcpuCreate);
        }

        // We write vcpuid to Aff1 as otherwise it won't match the redistributor ID
        // when using HVF in-kernel GICv3.
        let ret = unsafe { hv_vcpu_set_sys_reg(vcpuid, hv_sys_reg_t_HV_SYS_REG_MPIDR_EL1, mpidr) };
        if ret != HV_SUCCESS {
            return Err(Error::VcpuCreate);
        }

        let vcpu_exit: &hv_vcpu_exit_t = unsafe { vcpu_exit_ptr.as_mut().unwrap() };

        Ok(Self {
            vcpuid,
            vcpu_exit,
            cntfrq,
            mmio_buf: [0; 8],
            pending_mmio_read: None,
            pending_advance_pc: false,
            vtimer_masked: false,
            nested_enabled,
        })
    }

    pub fn set_initial_state(&self, entry_addr: u64, fdt_addr: u64) -> Result<(), Error> {
        if self.nested_enabled {
            let ret = unsafe {
                hv_vcpu_set_reg(self.vcpuid, hv_reg_t_HV_REG_CPSR, PSTATE_EL2_FAULT_BITS_64)
            };
            if ret != HV_SUCCESS {
                return Err(Error::VcpuInitialRegisters);
            }

            let ret = unsafe {
                hv_vcpu_set_sys_reg(self.vcpuid, hv_sys_reg_t_HV_SYS_REG_HCR_EL2, HCR_EL2_BITS)
            };
            if ret != HV_SUCCESS {
                return Err(Error::VcpuInitialRegisters);
            }

            let ret = unsafe {
                hv_vcpu_set_sys_reg(
                    self.vcpuid,
                    hv_sys_reg_t_HV_SYS_REG_CNTHCTL_EL2,
                    CNTHCTL_EL2_BITS,
                )
            };
            if ret != HV_SUCCESS {
                return Err(Error::VcpuInitialRegisters);
            }

            // Enable EL2 and GICv3 in ID_AA64PFR0_EL1
            let val: u64 = 0;
            let ret = unsafe {
                hv_vcpu_get_sys_reg(
                    self.vcpuid,
                    hv_sys_reg_t_HV_SYS_REG_ID_AA64PFR0_EL1,
                    &val as *const _ as *mut _,
                )
            };
            if ret != HV_SUCCESS {
                return Err(Error::VcpuInitialRegisters);
            }
            let ret = unsafe {
                hv_vcpu_set_sys_reg(
                    self.vcpuid,
                    hv_sys_reg_t_HV_SYS_REG_ID_AA64PFR0_EL1,
                    val | AA64PFR0_EL1_EL2EN | AA64PFR0_EL1_GIC3EN,
                )
            };
            if ret != HV_SUCCESS {
                return Err(Error::VcpuInitialRegisters);
            }

            // If SME is enabled in ID_AA64PFR1_EL1 in the VM, the guest will
            // break after enabling the MMU. Mask it out.
            let val: u64 = 0;
            let ret = unsafe {
                hv_vcpu_get_sys_reg(
                    self.vcpuid,
                    hv_sys_reg_t_HV_SYS_REG_ID_AA64PFR1_EL1,
                    &val as *const _ as *mut _,
                )
            };
            if ret != HV_SUCCESS {
                return Err(Error::VcpuInitialRegisters);
            }
            let ret = unsafe {
                hv_vcpu_set_sys_reg(
                    self.vcpuid,
                    hv_sys_reg_t_HV_SYS_REG_ID_AA64PFR1_EL1,
                    val & !AA64PFR1_EL1_SMEMASK,
                )
            };
            if ret != HV_SUCCESS {
                return Err(Error::VcpuInitialRegisters);
            }
        } else {
            let ret = unsafe {
                hv_vcpu_set_reg(self.vcpuid, hv_reg_t_HV_REG_CPSR, PSTATE_EL1_FAULT_BITS_64)
            };
            if ret != HV_SUCCESS {
                return Err(Error::VcpuInitialRegisters);
            }

            // Restore SCTLR_EL1 to its architectural reset value (RES1 bits only, so
            // MMU/caches off). A vCPU restarted by PSCI CPU_ON after a CPU_OFF still
            // carries the MMU state of its previous life, but the guest enters at a
            // physical entry address and expects translation disabled.
            let ret = unsafe {
                hv_vcpu_set_sys_reg(
                    self.vcpuid,
                    hv_sys_reg_t_HV_SYS_REG_SCTLR_EL1,
                    SCTLR_EL1_RES1,
                )
            };
            if ret != HV_SUCCESS {
                return Err(Error::VcpuInitialRegisters);
            }
        }

        let ret = unsafe { hv_vcpu_set_reg(self.vcpuid, hv_reg_t_HV_REG_PC, entry_addr) };
        if ret != HV_SUCCESS {
            return Err(Error::VcpuInitialRegisters);
        }

        let ret = unsafe { hv_vcpu_set_reg(self.vcpuid, hv_reg_t_HV_REG_X0, fdt_addr) };
        if ret != HV_SUCCESS {
            return Err(Error::VcpuInitialRegisters);
        }

        Ok(())
    }

    pub fn id(&self) -> u64 {
        self.vcpuid
    }

    pub fn exec_time_ns(&self) -> Option<u64> {
        let mut time = 0;
        let ret = unsafe { hv_vcpu_get_exec_time(self.vcpuid, &mut time) };
        if ret == HV_SUCCESS {
            Some(mach_absolute_time_to_ns(time))
        } else {
            None
        }
    }

    fn read_reg(&self, reg: u32) -> Result<u64, Error> {
        let val: u64 = 0;
        let ret = unsafe { hv_vcpu_get_reg(self.vcpuid, reg, &val as *const _ as *mut _) };
        if ret != HV_SUCCESS {
            Err(Error::VcpuReadRegister)
        } else {
            Ok(val)
        }
    }

    /// Captures this vCPU on its owning thread at a stopped execution boundary.
    pub fn capture_state(&self) -> Result<HvfVcpuState, Error> {
        if self.pending_mmio_read.is_some() {
            // An MMIO read is completed on the next run entry. Capturing only the architectural
            // registers here would lose both the destination register and the pending bus value.
            return Err(Error::VcpuStatePendingMmio);
        }

        let mut general = Vec::with_capacity(GENERAL_REGISTER_COUNT);
        for register in 0..GENERAL_REGISTER_COUNT as hv_reg_t {
            general.push((register, self.read_reg(register)?));
        }

        let mut simd = Vec::with_capacity(SIMD_REGISTER_COUNT);
        for register in 0..SIMD_REGISTER_COUNT as hv_simd_fp_reg_t {
            let mut value = 0_u128;
            let result = unsafe { hv_vcpu_get_simd_fp_reg(self.vcpuid, register, &mut value) };
            if result != HV_SUCCESS {
                return Err(Error::VcpuReadSimdRegister);
            }
            simd.push((register, value));
        }

        let system_registers = state_system_registers(self.nested_enabled);
        let mut system = Vec::with_capacity(system_registers.len());
        for register in system_registers {
            system.push((register, self.read_sys_reg(register)?));
        }

        let icc_registers = state_icc_registers(self.nested_enabled);
        let mut icc = Vec::with_capacity(icc_registers.len());
        for register in icc_registers {
            icc.push((register, self.read_icc_reg(register)?));
        }
        let ich_registers = state_ich_registers(self.nested_enabled);
        let mut ich = Vec::with_capacity(ich_registers.len());
        for register in ich_registers {
            ich.push((register, self.read_ich_reg(register)?));
        }

        let pending_irq = self.read_pending_interrupt(hv_interrupt_type_t_HV_INTERRUPT_TYPE_IRQ)?;
        let pending_fiq = self.read_pending_interrupt(hv_interrupt_type_t_HV_INTERRUPT_TYPE_FIQ)?;
        let mut vtimer_mask = false;
        let mut vtimer_offset = 0_u64;
        if unsafe { hv_vcpu_get_vtimer_mask(self.vcpuid, &mut vtimer_mask) } != HV_SUCCESS
            || unsafe { hv_vcpu_get_vtimer_offset(self.vcpuid, &mut vtimer_offset) } != HV_SUCCESS
        {
            return Err(Error::VcpuReadVtimer);
        }

        Ok(HvfVcpuState {
            general,
            simd,
            system,
            icc,
            ich,
            pending_irq,
            pending_fiq,
            vtimer_mask,
            vtimer_offset,
            pending_advance_pc: self.pending_advance_pc,
            nested_enabled: self.nested_enabled,
        })
    }

    /// Restores this vCPU on its owning thread before the first guest instruction executes.
    pub fn restore_state(&mut self, state: &HvfVcpuState) -> Result<(), Error> {
        if !state.has_register_topology(self.nested_enabled) {
            return Err(Error::VcpuStateTopology);
        }

        for &(register, value) in &state.general {
            self.write_reg(register, value)?;
        }
        for &(register, value) in &state.simd {
            let result = unsafe { hv_vcpu_set_simd_fp_reg(self.vcpuid, register, value) };
            if result != HV_SUCCESS {
                return Err(Error::VcpuSetSimdRegister);
            }
        }
        for &(register, value) in &state.system {
            let result = unsafe { hv_vcpu_set_sys_reg(self.vcpuid, register, value) };
            if result != HV_SUCCESS {
                return Err(Error::VcpuSetSystemRegister(register, value));
            }
        }
        for &(register, value) in &state.icc {
            self.write_icc_reg(register, value)?;
        }
        for &(register, value) in &state.ich {
            self.write_ich_reg(register, value)?;
        }
        if unsafe { hv_vcpu_set_vtimer_offset(self.vcpuid, state.vtimer_offset) } != HV_SUCCESS {
            return Err(Error::VcpuSetVtimerOffset);
        }
        vcpu_set_pending_irq(self.vcpuid, InterruptType::Irq, state.pending_irq)?;
        vcpu_set_pending_irq(self.vcpuid, InterruptType::Fiq, state.pending_fiq)?;
        vcpu_set_vtimer_mask(self.vcpuid, state.vtimer_mask)?;

        self.pending_mmio_read = None;
        self.pending_advance_pc = state.pending_advance_pc;
        self.vtimer_masked = state.vtimer_mask;
        Ok(())
    }

    fn read_pending_interrupt(&self, interrupt: hv_interrupt_type_t) -> Result<bool, Error> {
        let mut pending = false;
        let result = unsafe { hv_vcpu_get_pending_interrupt(self.vcpuid, interrupt, &mut pending) };
        if result == HV_SUCCESS {
            Ok(pending)
        } else {
            Err(Error::VcpuReadPendingInterrupt)
        }
    }

    fn read_icc_reg(&self, register: hv_gic_icc_reg_t) -> Result<u64, Error> {
        let mut value = 0_u64;
        let result = unsafe { hv_gic_get_icc_reg(self.vcpuid, register, &mut value) };
        if result == HV_SUCCESS {
            Ok(value)
        } else {
            Err(Error::VcpuReadGicRegister("ICC", register))
        }
    }

    fn write_icc_reg(&self, register: hv_gic_icc_reg_t, value: u64) -> Result<(), Error> {
        let result = unsafe { hv_gic_set_icc_reg(self.vcpuid, register, value) };
        if result == HV_SUCCESS {
            Ok(())
        } else {
            Err(Error::VcpuSetGicRegister("ICC", register, value))
        }
    }

    fn read_ich_reg(&self, register: hv_gic_ich_reg_t) -> Result<u64, Error> {
        let mut value = 0_u64;
        let result = unsafe { hv_gic_get_ich_reg(self.vcpuid, register, &mut value) };
        if result == HV_SUCCESS {
            Ok(value)
        } else {
            Err(Error::VcpuReadGicRegister("ICH", register))
        }
    }

    fn write_ich_reg(&self, register: hv_gic_ich_reg_t, value: u64) -> Result<(), Error> {
        let result = unsafe { hv_gic_set_ich_reg(self.vcpuid, register, value) };
        if result == HV_SUCCESS {
            Ok(())
        } else {
            Err(Error::VcpuSetGicRegister("ICH", register, value))
        }
    }

    pub fn write_reg(&self, rt: u32, val: u64) -> Result<(), Error> {
        let ret = unsafe { hv_vcpu_set_reg(self.vcpuid, rt, val) };
        if ret != HV_SUCCESS {
            Err(Error::VcpuSetRegister)
        } else {
            Ok(())
        }
    }

    /// Retries the instruction that caused the current emulation exit.
    ///
    /// Ordinary MMIO advances after emulation. A write-protection fault instead removes write
    /// protection and must execute the original guest instruction again.
    pub fn retry_current_instruction(&mut self) {
        self.pending_advance_pc = false;
    }

    fn read_sys_reg(&self, reg: u16) -> Result<u64, Error> {
        let val: u64 = 0;
        let ret = unsafe { hv_vcpu_get_sys_reg(self.vcpuid, reg, &val as *const _ as *mut _) };
        if ret != HV_SUCCESS {
            Err(Error::VcpuReadSystemRegister)
        } else {
            Ok(val)
        }
    }

    fn hvf_sync_vtimer(&mut self, vcpu_list: Arc<dyn Vcpus>) {
        if !self.vtimer_masked {
            return;
        }

        let ctl = self
            .read_sys_reg(hv_sys_reg_t_HV_SYS_REG_CNTV_CTL_EL0)
            .unwrap();
        let irq_state = (ctl & (TMR_CTL_ENABLE | TMR_CTL_IMASK | TMR_CTL_ISTATUS))
            == (TMR_CTL_ENABLE | TMR_CTL_ISTATUS);
        vcpu_list.set_vtimer_irq(self.vcpuid);
        if !irq_state {
            vcpu_set_vtimer_mask(self.vcpuid, false).unwrap();
            self.vtimer_masked = false;
        }
    }

    /// Write a PSCI return value into the guest's X0. Used by exits like
    /// `AffinityInfo` whose result the VMM computes outside this crate.
    pub fn write_psci_result(&self, val: u64) -> Result<(), Error> {
        self.write_reg(hv_reg_t_HV_REG_X0, val)
    }

    fn handle_psci_request(&self) -> Result<VcpuExit<'_>, Error> {
        match self.read_reg(hv_reg_t_HV_REG_X0)? {
            0x8400_0000 /* QEMU_PSCI_0_2_FN_PSCI_VERSION */ => {
                self.write_reg(hv_reg_t_HV_REG_X0, 2)?;
                Ok(VcpuExit::PsciHandled)
            },
            0x8400_0002 /* QEMU_PSCI_0_2_FN_CPU_OFF */ => {
                // Success does not return to the caller: the vCPU parks until a
                // later CPU_ON targets it again.
                Ok(VcpuExit::CpuOff)
            },
            0x8400_0004 /* QEMU_PSCI_0_2_FN_AFFINITY_INFO */ |
            0xc400_0004 /* QEMU_PSCI_0_2_FN64_AFFINITY_INFO */ => {
                let mpidr = self.read_reg(hv_reg_t_HV_REG_X1)?;
                // The VMM answers ON/OFF through `write_psci_result`.
                Ok(VcpuExit::AffinityInfo(mpidr))
            },
            0x8400_0006 /* QEMU_PSCI_0_2_FN_MIGRATE_INFO_TYPE */ => {
                self.write_reg(hv_reg_t_HV_REG_X0, 2)?;
                Ok(VcpuExit::PsciHandled)
            },
            0x8400_0008 /* QEMU_PSCI_0_2_FN_SYSTEM_OFF */ => {
                debug!("PSCI SYSTEM_OFF on vcpu {}", self.vcpuid);
                Ok(VcpuExit::Shutdown)
            },
            0x8400_0009 /* QEMU_PSCI_0_2_FN_SYSTEM_RESET */ => {
                debug!("PSCI SYSTEM_RESET on vcpu {}", self.vcpuid);
                Ok(VcpuExit::Shutdown)
            },
            0xc400_0003 /* QEMU_PSCI_0_2_FN64_CPU_ON */ => {
                let mpidr = self.read_reg(hv_reg_t_HV_REG_X1)?;
                let entry = self.read_reg(hv_reg_t_HV_REG_X2)?;
                let context_id = self.read_reg(hv_reg_t_HV_REG_X3)?;
                self.write_reg(hv_reg_t_HV_REG_X0, 0)?;
                Ok(VcpuExit::CpuOn(mpidr, entry, context_id))
            }
            val => panic!("Unexpected val={val}")
        }
    }

    pub fn run(&mut self, vcpu_list: Arc<dyn Vcpus>) -> Result<VcpuExit<'_>, Error> {
        let pending_irq = vcpu_list.has_pending_irq(self.vcpuid);

        if let Some(mmio_read) = self.pending_mmio_read.take() {
            if mmio_read.srt < 31 {
                let val = match mmio_read.len {
                    1 => u8::from_le_bytes(self.mmio_buf[0..1].try_into().unwrap()) as u64,
                    2 => u16::from_le_bytes(self.mmio_buf[0..2].try_into().unwrap()) as u64,
                    4 => u32::from_le_bytes(self.mmio_buf[0..4].try_into().unwrap()) as u64,
                    8 => u64::from_le_bytes(self.mmio_buf[0..8].try_into().unwrap()),
                    _ => panic!(
                        "unsupported mmio pa={} len={}",
                        mmio_read.addr, mmio_read.len
                    ),
                };

                self.write_reg(mmio_read.srt, val)?;
            }
        }

        if self.pending_advance_pc {
            let pc = self.read_reg(hv_reg_t_HV_REG_PC)?;
            self.write_reg(hv_reg_t_HV_REG_PC, pc + 4)?;
            self.pending_advance_pc = false;
        }

        if pending_irq {
            vcpu_set_pending_irq(self.vcpuid, InterruptType::Irq, true)?;
        }

        let ret = unsafe { hv_vcpu_run(self.vcpuid) };
        if ret != HV_SUCCESS {
            return Err(Error::VcpuRun);
        }

        match self.vcpu_exit.reason {
            HV_EXIT_REASON_EXCEPTION => { /* This is the main one, handle below. */ }
            HV_EXIT_REASON_VTIMER_ACTIVATED => {
                self.vtimer_masked = true;
                return Ok(VcpuExit::VtimerActivated);
            }
            HV_EXIT_REASON_CANCELED => return Ok(VcpuExit::Canceled),
            _ => {
                let pc = self.read_reg(hv_reg_t_HV_REG_PC)?;
                panic!(
                    "unexpected exit reason: vcpuid={} 0x{:x} at pc=0x{:x}",
                    self.id(),
                    self.vcpu_exit.reason,
                    pc
                );
            }
        }

        self.hvf_sync_vtimer(vcpu_list.clone());

        let syndrome = self.vcpu_exit.exception.syndrome;
        let ec = (syndrome >> 26) & 0x3f;
        match ec {
            EC_AA64_BKPT => {
                debug!("vcpu[{}]: BRK exit", self.vcpuid);
                Ok(VcpuExit::Breakpoint)
            }
            EC_DATAABORT => {
                let isv: bool = (syndrome & (1 << 24)) != 0;
                let iswrite: bool = ((syndrome >> 6) & 1) != 0;
                let s1ptw: bool = ((syndrome >> 7) & 1) != 0;
                let sas: u32 = ((syndrome >> 22) & 3) as u32;
                let len: usize = (1 << sas) as usize;
                let srt: u32 = ((syndrome >> 16) & 0x1f) as u32;
                let cm: u32 = ((syndrome >> 8) & 0x1) as u32;

                debug!(
                    "EC_DATAABORT {} {} {} {} {} {} {} {}",
                    syndrome, isv as u8, iswrite as u8, s1ptw as u8, sas, len, srt, cm
                );

                let pa = self.vcpu_exit.exception.physical_address;
                self.pending_advance_pc = true;

                if iswrite {
                    let val = if srt < 31 {
                        self.read_reg(hv_reg_t_HV_REG_X0 + srt)?
                    } else {
                        0
                    };

                    match len {
                        1 => self.mmio_buf[0..1].copy_from_slice(&(val as u8).to_le_bytes()),
                        2 => self.mmio_buf[0..2].copy_from_slice(&(val as u16).to_le_bytes()),
                        4 => self.mmio_buf[0..4].copy_from_slice(&(val as u32).to_le_bytes()),
                        8 => self.mmio_buf[0..8].copy_from_slice(&val.to_le_bytes()),
                        _ => panic!("unsupported mmio len={len}"),
                    };

                    Ok(VcpuExit::MmioWrite(pa, &self.mmio_buf[0..len]))
                } else {
                    self.pending_mmio_read = Some(MmioRead { addr: pa, srt, len });
                    Ok(VcpuExit::MmioRead(pa, &mut self.mmio_buf[0..len]))
                }
            }
            #[cfg(all(target_arch = "aarch64", target_os = "macos"))]
            EC_SYSTEMREGISTERTRAP => {
                let isread: bool = (syndrome & 1) != 0;
                let rt: u32 = ((syndrome >> 5) & 0x1f) as u32;
                let reg: u32 = syndrome as u32 & SYSREG_MASK;
                debug!(
                    "EC_SYSTEMREGISTERTRAP isread={}, syndrome={}, rt={}, reg={}, reg_name={}",
                    isread as u32,
                    syndrome,
                    rt,
                    reg,
                    sys_reg_name(reg).unwrap_or("unknown sysreg")
                );

                self.pending_advance_pc = true;

                if isread {
                    assert!(rt < 32);

                    // See https://developer.arm.com/documentation/dui0801/l/Overview-of-AArch64-state/Registers-in-AArch64-state
                    if rt == 31 {
                        return Ok(VcpuExit::SystemRegister);
                    }

                    match vcpu_list.handle_sysreg_read(self.vcpuid, reg) {
                        Some(val) => {
                            self.write_reg(rt, val)?;
                            Ok(VcpuExit::SystemRegister)
                        }
                        None => panic!(
                            "UNKNOWN rt={}, reg={} name={}",
                            rt,
                            reg,
                            sys_reg_name(reg).unwrap_or("unknown sysreg")
                        ),
                    }
                } else {
                    assert!(rt < 32);

                    // See https://developer.arm.com/documentation/dui0801/l/Overview-of-AArch64-state/Registers-in-AArch64-state
                    let val = if rt == 31 { 0u64 } else { self.read_reg(rt)? };

                    if vcpu_list.handle_sysreg_write(self.vcpuid, reg, val) {
                        Ok(VcpuExit::SystemRegister)
                    } else {
                        panic!(
                            "unexpected write: {} name={}",
                            reg,
                            sys_reg_name(reg).unwrap_or("unknown sysreg")
                        );
                    }
                }
            }
            EC_WFX_TRAP => {
                let ctl = self.read_sys_reg(hv_sys_reg_t_HV_SYS_REG_CNTV_CTL_EL0)?;

                self.pending_advance_pc = true;
                if ((ctl & 1) == 0) || (ctl & 2) != 0 {
                    return Ok(VcpuExit::WaitForEvent);
                }

                // Also CNTV_CVAL & CNTV_CVAL_EL0
                let cval = self.read_sys_reg(hv_sys_reg_t_HV_SYS_REG_CNTV_CVAL_EL0)?;
                let now = unsafe { mach_absolute_time() };
                if now > cval {
                    return Ok(VcpuExit::WaitForEventExpired);
                }

                let timeout = Duration::from_nanos((cval - now) * (1_000_000_000 / self.cntfrq));
                Ok(VcpuExit::WaitForEventTimeout(timeout))
            }
            EC_AA64_HVC => self.handle_psci_request(),
            EC_AA64_SMC => {
                self.pending_advance_pc = true;
                self.handle_psci_request()
            }
            _ => panic!("unexpected exception: 0x{ec:x}"),
        }
    }
}

fn state_system_registers(nested_enabled: bool) -> Vec<hv_sys_reg_t> {
    let mut registers = Vec::with_capacity(
        EL1_SYSTEM_REGISTERS.len()
            + if nested_enabled {
                EL2_SYSTEM_REGISTERS.len()
            } else {
                0
            },
    );
    registers.extend_from_slice(EL1_SYSTEM_REGISTERS);
    if nested_enabled {
        registers.extend_from_slice(EL2_SYSTEM_REGISTERS);
    }
    registers
}

fn state_icc_registers(nested_enabled: bool) -> Vec<hv_gic_icc_reg_t> {
    let mut registers = Vec::with_capacity(
        ICC_REGISTERS.len()
            + if nested_enabled {
                NESTED_ICC_REGISTERS.len()
            } else {
                0
            },
    );
    if nested_enabled {
        registers.extend_from_slice(NESTED_ICC_REGISTERS);
    }
    registers.extend_from_slice(ICC_REGISTERS);
    registers
}

fn state_ich_registers(nested_enabled: bool) -> Vec<hv_gic_ich_reg_t> {
    if nested_enabled {
        NESTED_ICH_REGISTERS.to_vec()
    } else {
        Vec::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state(nested_enabled: bool) -> HvfVcpuState {
        HvfVcpuState {
            general: (0..GENERAL_REGISTER_COUNT as hv_reg_t)
                .map(|register| (register, 0))
                .collect(),
            simd: (0..SIMD_REGISTER_COUNT as hv_simd_fp_reg_t)
                .map(|register| (register, 0))
                .collect(),
            system: state_system_registers(nested_enabled)
                .into_iter()
                .map(|register| (register, 0))
                .collect(),
            icc: state_icc_registers(nested_enabled)
                .into_iter()
                .map(|register| (register, 0))
                .collect(),
            ich: state_ich_registers(nested_enabled)
                .into_iter()
                .map(|register| (register, 0))
                .collect(),
            pending_irq: false,
            pending_fiq: false,
            vtimer_mask: false,
            vtimer_offset: 0,
            pending_advance_pc: false,
            nested_enabled,
        }
    }

    #[test]
    fn timer_rebase_uses_the_same_wrapping_displacement_for_every_vcpu() {
        let mut first = state(false);
        let mut second = state(false);
        first.vtimer_offset = 9;
        second.vtimer_offset = 9;
        let displacement = u64::MAX - 3;
        first.rebase_timer_offset(displacement);
        second.rebase_timer_offset(displacement);
        assert_eq!(first.vtimer_offset, 5);
        assert_eq!(first.vtimer_offset, second.vtimer_offset);
    }

    #[test]
    fn physical_gic_cpu_interface_topology_is_exact() {
        let state = state(false);

        assert!(state.has_register_topology(false));
        assert_eq!(state.icc.len(), ICC_REGISTERS.len());
        assert!(state.ich.is_empty());
        assert!(!state
            .icc
            .iter()
            .any(|(register, _)| *register == hv_gic_icc_reg_t_HV_GIC_ICC_REG_RPR_EL1));
        assert_eq!(
            state
                .icc
                .iter()
                .rev()
                .take(2)
                .map(|(register, _)| *register)
                .collect::<Vec<_>>(),
            [
                hv_gic_icc_reg_t_HV_GIC_ICC_REG_IGRPEN1_EL1,
                hv_gic_icc_reg_t_HV_GIC_ICC_REG_IGRPEN0_EL1,
            ]
        );
    }

    #[test]
    fn nested_gic_cpu_interface_topology_is_explicit() {
        let state = state(true);

        assert!(state.has_register_topology(true));
        assert_eq!(
            state.icc.len(),
            ICC_REGISTERS.len() + NESTED_ICC_REGISTERS.len()
        );
        assert_eq!(state.ich.len(), NESTED_ICH_REGISTERS.len());
        assert_eq!(
            state.ich.last().map(|(register, _)| *register),
            Some(hv_gic_ich_reg_t_HV_GIC_ICH_REG_HCR_EL2)
        );
    }

    #[test]
    fn gic_cpu_interface_topology_rejects_missing_or_reordered_state() {
        let mut missing = state(false);
        missing.icc.pop();
        assert!(!missing.has_register_topology(false));

        let mut reordered = state(false);
        reordered.icc.swap(0, 1);
        assert!(!reordered.has_register_topology(false));

        assert!(!state(true).has_register_topology(false));
    }
}
