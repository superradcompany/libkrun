//! Linux PID 1 for the architecture-specific running nested VM fixture.
#[cfg(target_arch = "aarch64")]
mod arm64;
#[cfg(target_arch = "x86_64")]
mod x86_64;

#[cfg(not(any(target_arch = "aarch64", target_arch = "x86_64")))]
compile_error!("the nested counter guest supports only ARM64 and x86_64");

fn main() {
    #[cfg(target_arch = "aarch64")]
    arm64::main();
    #[cfg(target_arch = "x86_64")]
    x86_64::main();
}
