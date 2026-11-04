//! Small native EL2 state-transfer probe. This is not full inner-VM qualification.
use hvf::{bindings::*, HvfVcpu, HvfVm};
use std::time::Duration;

#[repr(align(16384))]
struct Ram([u8; 65536]);

fn create(ram: &mut Ram) -> (HvfVm, HvfVcpu<'static>) {
    let vm = HvfVm::new(true).unwrap();
    unsafe {
        let config = hv_gic_config_create();
        assert_eq!(
            hv_gic_config_set_distributor_base(config, 0x10000000),
            HV_SUCCESS
        );
        assert_eq!(
            hv_gic_config_set_redistributor_base(config, 0x10020000),
            HV_SUCCESS
        );
        assert_eq!(hv_gic_create(config), HV_SUCCESS);
        os_release(config.cast());
    }
    vm.map_memory(ram.0.as_mut_ptr() as u64, 0x80000000, ram.0.len() as u64)
        .unwrap();
    let cpu = HvfVcpu::new(0, true).unwrap();
    assert_eq!(
        unsafe { hv_vcpu_set_trap_debug_exceptions(cpu.id(), true) },
        HV_SUCCESS
    );
    (vm, cpu)
}

fn run(cpu: &HvfVcpu) {
    // Exit a stuck guest without relying on its instruction stream.
    let (stop, wait) = std::sync::mpsc::channel();
    let id = cpu.id();
    let watchdog = std::thread::spawn(move || {
        let timed_out = wait.recv_timeout(Duration::from_secs(2)).is_err();
        if timed_out {
            hvf::vcpu_request_exit(id).unwrap();
        }
        timed_out
    });
    let result = unsafe { hv_vcpu_run(cpu.id()) };
    stop.send(()).ok();
    assert!(
        !watchdog.join().unwrap(),
        "EL2 run exceeded watchdog deadline"
    );
    assert_eq!(result, HV_SUCCESS);
    let mut pc = 0;
    assert_eq!(
        unsafe { hv_vcpu_get_reg(cpu.id(), hv_reg_t_HV_REG_PC, &mut pc) },
        HV_SUCCESS
    );
    assert_eq!(pc, 0x8000000c);
}

fn destroy(cpu: HvfVcpu, vm: HvfVm) {
    unsafe {
        assert_eq!(hv_vcpu_destroy(cpu.id()), HV_SUCCESS);
        assert_eq!(hv_vm_destroy(), HV_SUCCESS);
    }
    drop((cpu, vm));
}

fn main() {
    let mut ram = Box::new(Ram([0; 65536]));
    // ldr x0,[x1]; add x0,x0,#1; str x0,[x1]; brk #0; b start
    for (slot, instruction) in ram.0.chunks_exact_mut(4).zip([
        0xf9400020u32,
        0x91000400,
        0xf9000020,
        0xd4200000,
        0x17fffffc,
    ]) {
        slot.copy_from_slice(&instruction.to_le_bytes());
    }
    let (vm, cpu) = create(&mut ram);
    cpu.set_initial_state(0x80000000, 0).unwrap();
    cpu.write_reg(hv_reg_t_HV_REG_X1, 0x80004000).unwrap();
    run(&cpu);
    assert_eq!(
        u64::from_le_bytes(ram.0[16384..16392].try_into().unwrap()),
        1
    );
    assert_eq!(
        unsafe { hv_vcpu_set_trap_debug_exceptions(cpu.id(), false) },
        HV_SUCCESS
    );
    let mut mdcr = 0;
    println!("MDCR read result={:#x}", unsafe {
        hv_vcpu_get_sys_reg(cpu.id(), hv_sys_reg_t_HV_SYS_REG_MDCR_EL2, &mut mdcr)
    });
    let state = match cpu.capture_state() {
        Ok(state) => state,
        Err(error) => {
            eprintln!("EL2 execution passed; snapshot capture failed: {error}");
            destroy(cpu, vm);
            std::process::exit(1);
        }
    };
    let gic = vm.capture_gic_state().expect("capture GIC");
    println!("captured EL2 counter=1 GIC bytes={}", gic.len());
    destroy(cpu, vm);

    let mut second_generation = None;
    for generation in 0..2 {
        let mut restored_ram = Box::new(Ram(ram.0));
        let (vm, mut cpu) = create(&mut restored_ram);
        vm.restore_gic_state(&gic).expect("restore GIC");
        cpu.restore_state(&state).expect("restore EL2 CPU state");
        cpu.write_reg(hv_reg_t_HV_REG_PC, 0x80000010).unwrap();
        run(&cpu);
        assert_eq!(
            u64::from_le_bytes(restored_ram.0[16384..16392].try_into().unwrap()),
            2
        );
        assert_eq!(
            u64::from_le_bytes(ram.0[16384..16392].try_into().unwrap()),
            1
        );
        println!("fresh EL2 restore {generation}: counter=2; baseline unchanged");
        assert_eq!(
            unsafe { hv_vcpu_set_trap_debug_exceptions(cpu.id(), false) },
            HV_SUCCESS
        );
        second_generation = Some((
            cpu.capture_state().expect("recapture CPU"),
            vm.capture_gic_state().expect("recapture GIC"),
            restored_ram,
        ));
        destroy(cpu, vm);
    }

    let (state, gic, mut ram) = second_generation.unwrap();
    let (vm, mut cpu) = create(&mut ram);
    vm.restore_gic_state(&gic).expect("restore recaptured GIC");
    cpu.restore_state(&state).expect("restore recaptured CPU");
    cpu.write_reg(hv_reg_t_HV_REG_PC, 0x80000010).unwrap();
    run(&cpu);
    assert_eq!(
        u64::from_le_bytes(ram.0[16384..16392].try_into().unwrap()),
        3
    );
    println!("second-generation EL2 restore: counter=3");
    destroy(cpu, vm);
}
