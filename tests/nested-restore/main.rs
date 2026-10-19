//! Each capture or restore runs in a fresh process. No public admission bypass.
use msb_krun::{
    GuestMemoryRange, MemoryCaptureOptions, MemoryCaptureSink, VmBuilder, VmMemoryRestoreSource,
    VmMemoryRestoreTarget,
};
use std::{
    fs,
    io::{self, Read, Write},
    path::PathBuf,
    thread,
    time::{Duration, Instant},
};
type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;
struct Memory(fs::File);
impl MemoryCaptureSink for Memory {
    fn write_bytes(&mut self, range: GuestMemoryRange, bytes: &[u8]) -> io::Result<()> {
        self.0.write_all(&range.start().to_le_bytes())?;
        self.0.write_all(&range.length().to_le_bytes())?;
        self.0.write_all(bytes)
    }
    fn write_zero(&mut self, range: GuestMemoryRange) -> io::Result<()> {
        self.write_bytes(range, &vec![0; range.length() as usize])
    }
}
impl VmMemoryRestoreSource for Memory {
    fn restore(&mut self, target: &mut dyn VmMemoryRestoreTarget) -> io::Result<()> {
        loop {
            let mut start = [0; 8];
            if self.0.read(&mut start[..1])? == 0 {
                return Ok(());
            }
            self.0.read_exact(&mut start[1..])?;
            let mut size = [0; 8];
            self.0.read_exact(&mut size)?;
            let start = u64::from_le_bytes(start);
            let size = u64::from_le_bytes(size);
            if size > 512 * 1024 * 1024 {
                return Err(io::Error::other("oversized memory record"));
            }
            let mut bytes = vec![0; size as usize];
            self.0.read_exact(&mut bytes)?;
            target.write_bytes(
                GuestMemoryRange::new(start, size).map_err(io::Error::other)?,
                &bytes,
            )?;
        }
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Progress {
    count: u32,
    ticks: u32,
    identity: u64,
    workers: Option<[u64; 2]>,
}
impl Progress {
    fn advanced_from(self, old: Self) -> bool {
        self.count != old.count
            && self.ticks > old.ticks
            && self.ticks > 2
            && match (old.workers, self.workers) {
                (None, None) => true,
                (Some(a), Some(b)) => b[0] > a[0] && b[1] > a[1],
                _ => false,
            }
    }
}
fn progress(path: &std::path::Path) -> Option<Progress> {
    let text = fs::read_to_string(path).ok()?;
    // Ignore unfinished console records, including a split workload identity.
    let (text, _) = text.rsplit_once('\n')?;
    let busy = text.lines().any(|line| line == "NESTED_BUSY workers=2")
        || text
            .lines()
            .any(|line| line.starts_with("NESTED_BUSY_PROGRESS "));
    let lines: Vec<_> = text.lines().collect();
    lines.iter().enumerate().rev().find_map(|(index, line)| {
        let mut fields = line.strip_prefix("NESTED_PROGRESS ")?.split_whitespace();
        let workers = if busy {
            let mut values = lines
                .get(index.checked_sub(1)?)?
                .strip_prefix("NESTED_BUSY_PROGRESS ")?
                .split_whitespace();
            Some([values.next()?.parse().ok()?, values.next()?.parse().ok()?])
        } else {
            None
        };
        Some(Progress {
            count: fields.next()?.parse().ok()?,
            ticks: fields.next()?.parse().ok()?,
            identity: fields.next()?.parse().ok()?,
            workers,
        })
    })
}

// The macOS backend payload starts with HvfVcpuState.general. Read that
// serialized register inventory without adding a production-only test accessor.
#[cfg(target_os = "macos")]
fn captured_at_el1(state: &vmm::execution_state::ExecutionState) -> Result<bool> {
    if state.backend() != vmm::execution_state::ExecutionBackend::Hvf
        || state.backend_state_abi() != 2
    {
        return Err("EL1 capture check requires the HVF ABI 2 register layout".into());
    }
    let mut all_at_el1 = !state.vcpus().is_empty();
    for cpu in state.vcpus() {
        let (general, _): (Vec<(u32, u64)>, usize) =
            bincode::serde::decode_from_slice(cpu.bytes(), bincode::config::standard())?;
        let cpsr = general
            .iter()
            .find(|(register, _)| *register == hvf::bindings::hv_reg_t_HV_REG_CPSR)
            .ok_or("captured CPU has no CPSR")?
            .1;
        // M[3:0] distinguishes EL1t (4) and EL1h (5) from EL0 and EL2.
        all_at_el1 &= matches!(cpsr & 0xf, 4 | 5);
        eprintln!("captured vCPU {} CPSR={cpsr:#x}", cpu.id());
    }
    Ok(all_at_el1)
}

fn main() -> Result<()> {
    let mode = std::env::args()
        .nth(1)
        .ok_or("capture or restore required")?;
    let root = PathBuf::from(std::env::var("TEST_ROOT")?);
    let snapshot = PathBuf::from(std::env::var("TEST_SNAPSHOT")?);
    let saved = if mode == "restore" {
        Some(progress(&snapshot.join("progress")).ok_or("missing saved progress")?)
    } else {
        None
    };
    let memory_mib = std::env::var("TEST_MEMORY_MIB")
        .ok()
        .map(|value| value.parse::<usize>())
        .transpose()?
        .unwrap_or(256);
    let progress_timeout = Duration::from_secs(
        std::env::var("TEST_PROGRESS_TIMEOUT_SECONDS")
            .ok()
            .map(|value| value.parse::<u64>())
            .transpose()?
            .unwrap_or(30),
    );
    // The full-OS fixture reports CLOCK_MONOTONIC milliseconds. The bare-metal
    // fixture uses timer counts, so opt in only for the full-OS workload.
    let check_clock = std::env::var("TEST_CLOCK_MILLISECONDS").as_deref() == Ok("true");
    let split_irqchip = std::env::var("TEST_SPLIT_IRQCHIP")
        .ok()
        .map(|value| value.parse::<bool>())
        .transpose()?
        .unwrap_or(false);
    if split_irqchip && !cfg!(all(target_os = "linux", target_arch = "x86_64")) {
        return Err("TEST_SPLIT_IRQCHIP requires a Linux x86_64 host".into());
    }
    eprintln!("outer split irqchip: {split_irqchip}");
    fs::write(root.join("console.log"), "")?;
    let mut vm = VmBuilder::new()
        .on_exit(|code| {
            eprintln!("FAIL: guest exited with code {code} before the probe completed");
            std::process::exit(1);
        })
        .machine(|m| {
            m.vcpus(2)
                .memory_mib(memory_mib)
                .nested_virt(true)
                .split_irqchip(split_irqchip)
                .balloon(false)
                .rng(false)
        })
        .kernel(|k| {
            k.krunfw_path(std::env::var("KRUNFW_PATH").unwrap())
                .initramfs_path(std::env::var("TEST_INITRAMFS").unwrap())
        })
        .console(|c| {
            #[cfg(target_os = "windows")]
            {
                c.disable_implicit().virtio_output(root.join("console.log"))
            }
            #[cfg(not(target_os = "windows"))]
            {
                c.output(root.join("console.log"))
            }
        })
        .build()?;
    if mode == "restore" {
        let execution =
            vmm::execution_state::ExecutionState::decode(&fs::read(snapshot.join("execution"))?)?;
        #[cfg(target_os = "macos")]
        if !captured_at_el1(&execution)? {
            return Err("macOS regression requires every parent vCPU captured at EL1".into());
        }
        vm.set_execution_restore(execution);
        vm.set_memory_restore(Memory(fs::File::open(snapshot.join("memory"))?));
        for entry in fs::read_dir(&snapshot)? {
            let entry = entry?;
            if entry.file_name().to_string_lossy().starts_with("device-") {
                vm.add_virtio_device_restore(vmm::device_state::VirtioDeviceState::decode(
                    &fs::read(entry.path())?,
                )?);
            }
        }
    } else if mode != "capture" {
        return Err("invalid mode".into());
    }
    let output_snapshot = std::env::var_os("TEST_OUTPUT_SNAPSHOT").map(PathBuf::from);
    let control = vm.control_handle();
    thread::spawn(move || {
        let result = (|| -> Result<()> {
            control.wait_until_running(Duration::from_secs(30))?;
            let begin = Instant::now();
            let mut previous = None;
            let mut progress_started = None;
            let mut clock_sample = None;
            loop {
                if begin.elapsed() > progress_timeout {
                    return Err("inner counter/timer did not progress".into());
                }
                if let Some(current) = progress(&root.join("console.log")) {
                    if let Some(saved) = saved {
                        if current.identity != saved.identity {
                            return Err("inner workload cold-booted instead of restoring".into());
                        }
                        if current.ticks <= saved.ticks {
                            thread::sleep(Duration::from_millis(200));
                            continue;
                        }
                    }
                    if let Some(old) = previous {
                        let old: Progress = old;
                        if current.advanced_from(old) {
                            let started = progress_started.get_or_insert_with(Instant::now);
                            let (sample_time, sample_ticks) = *clock_sample
                                .get_or_insert_with(|| (Instant::now(), current.ticks));
                            if started.elapsed() >= Duration::from_secs(10) {
                                if check_clock {
                                    let ratio = f64::from(current.ticks - sample_ticks)
                                        / sample_time.elapsed().as_secs_f64()
                                        / 1000.0;
                                    println!("guest/host elapsed-time ratio: {ratio:.4}");
                                    if !(0.8..=1.2).contains(&ratio) {
                                        return Err("nested guest clock rate changed".into());
                                    }
                                }
                                println!("{mode}: inner progress={current:?} (previous={old:?})");
                                break;
                            }
                        } else {
                            progress_started = None;
                            clock_sample = None;
                        }
                    }
                    previous = Some(current);
                } else {
                    progress_started = None;
                    clock_sample = None;
                }
                thread::sleep(Duration::from_millis(200));
            }
            #[allow(unused_mut, unused_variables)]
            let mut pause = control.pause()?;
            if mode == "capture" || output_snapshot.is_some() {
                #[allow(unused_mut)]
                let mut execution = control.capture_execution_state()?;
                #[cfg(target_os = "macos")]
                {
                    let deadline = Instant::now() + Duration::from_secs(30);
                    while !captured_at_el1(&execution)? {
                        if Instant::now() >= deadline {
                            return Err("could not capture every parent vCPU at EL1".into());
                        }
                        control.resume(pause)?;
                        thread::sleep(Duration::from_millis(10));
                        pause = control.pause()?;
                        execution = control.capture_execution_state()?;
                    }
                }
                let snapshot = output_snapshot.as_ref().unwrap_or(&snapshot);
                fs::create_dir_all(snapshot)?;
                fs::write(snapshot.join("execution"), execution.encode()?)?;
                let plan = control.plan_full_memory_capture()?;
                control.capture_memory(
                    &plan,
                    MemoryCaptureOptions::new(4096, false)?,
                    &mut Memory(fs::File::create(snapshot.join("memory"))?),
                )?;
                for (kind, id) in control.virtio_device_inventory()? {
                    let state = control.capture_virtio_device_state(kind, &id)?;
                    fs::write(
                        snapshot.join(format!("device-{kind}-{id}")),
                        state.encode()?,
                    )?;
                }
                fs::write(
                    snapshot.join("progress"),
                    fs::read(root.join("console.log"))?,
                )?;
            }
            Ok(())
        })();
        if let Err(error) = result {
            eprintln!("FAIL: {error}");
            std::process::exit(1);
        }
        println!("PASS");
        std::process::exit(0);
    });
    vm.enter()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn progress_waits_for_a_complete_console_record() {
        let path = std::env::temp_dir().join(format!(
            "nested-progress-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        fs::write(
            &path,
            "NESTED_PROGRESS 1 10 123456\nNESTED_PROGRESS 2 20 123",
        )
        .unwrap();
        let partial = progress(&path);
        fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(b"456\n")
            .unwrap();
        let complete = progress(&path);
        fs::write(
            &path,
            "NESTED_BUSY workers=2\nNESTED_BUSY_PROGRESS 10 20\nNESTED_PROGRESS 1 10 123456\n",
        )
        .unwrap();
        let busy = progress(&path).unwrap();
        for (workers, advances) in [("11 20", false), ("10 21", false), ("11 21", true)] {
            fs::write(
                &path,
                format!("NESTED_BUSY_PROGRESS {workers}\nNESTED_PROGRESS 2 20 123456\n"),
            )
            .unwrap();
            assert_eq!(progress(&path).unwrap().advanced_from(busy), advances);
        }
        fs::remove_file(path).unwrap();

        assert_eq!(
            partial,
            Some(Progress {
                count: 1,
                ticks: 10,
                identity: 123456,
                workers: None
            })
        );
        assert_eq!(
            complete,
            Some(Progress {
                count: 2,
                ticks: 20,
                identity: 123456,
                workers: None
            })
        );
    }
}
