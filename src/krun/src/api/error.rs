use std::fmt;
use std::io;

//--------------------------------------------------------------------------------------------------
// Types
//--------------------------------------------------------------------------------------------------

/// Result type for libkrun operations.
pub type Result<T> = std::result::Result<T, Error>;

/// Top-level error type for libkrun.
#[derive(Debug)]
pub enum Error {
    /// Configuration error.
    Config(ConfigError),

    /// VM build error.
    Build(BuildError),

    /// Runtime error.
    Runtime(RuntimeError),

    /// I/O error.
    Io(io::Error),
}

/// Configuration-related errors.
#[derive(Debug)]
pub enum ConfigError {
    /// Invalid vCPU count.
    InvalidVcpuCount(u8),

    /// Invalid memory size.
    InvalidMemorySize(usize),

    /// Missing kernel configuration.
    MissingKernel,

    /// Invalid kernel bundle.
    InvalidKernelBundle(String),

    /// Network configuration error.
    Network(String),

    /// Filesystem configuration error.
    Filesystem(String),

    /// Block device configuration error.
    Block(String),

    /// Console configuration error.
    Console(String),

    /// Vsock configuration error.
    Vsock(String),

    /// Memory balloon configuration error.
    Balloon(String),
}

/// VM build errors.
#[derive(Debug)]
pub enum BuildError {
    /// Failed to create guest memory.
    GuestMemory(String),

    /// Failed to register a device.
    DeviceRegistration(String),

    /// Failed to start the microvm.
    Start(String),

    /// libkrunfw error.
    Krunfw(String),
}

/// Runtime errors.
#[derive(Debug)]
pub enum RuntimeError {
    /// Event loop error.
    EventLoop(String),

    /// VM is already running.
    AlreadyRunning,

    /// VM has not been started.
    NotStarted,

    /// Shutdown error.
    Shutdown(String),
}

//--------------------------------------------------------------------------------------------------
// Trait Implementations
//--------------------------------------------------------------------------------------------------

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Config(e) => write!(f, "configuration error: {}", e),
            Error::Build(e) => write!(f, "build error: {}", e),
            Error::Runtime(e) => write!(f, "runtime error: {}", e),
            Error::Io(e) => write!(f, "I/O error: {}", e),
        }
    }
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ConfigError::InvalidVcpuCount(n) => write!(f, "invalid vCPU count: {}", n),
            ConfigError::InvalidMemorySize(n) => write!(f, "invalid memory size: {} MiB", n),
            ConfigError::MissingKernel => write!(f, "missing kernel configuration"),
            ConfigError::InvalidKernelBundle(s) => write!(f, "invalid kernel bundle: {}", s),
            ConfigError::Network(s) => write!(f, "network: {}", s),
            ConfigError::Filesystem(s) => write!(f, "filesystem: {}", s),
            ConfigError::Block(s) => write!(f, "block device: {}", s),
            ConfigError::Console(s) => write!(f, "console: {}", s),
            ConfigError::Vsock(s) => write!(f, "vsock: {}", s),
            ConfigError::Balloon(s) => write!(f, "balloon: {}", s),
        }
    }
}

impl fmt::Display for BuildError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            BuildError::GuestMemory(s) => write!(f, "guest memory: {}", s),
            BuildError::DeviceRegistration(s) => write!(f, "device registration: {}", s),
            BuildError::Start(s) => write!(f, "start: {}", s),
            BuildError::Krunfw(s) => write!(f, "libkrunfw: {}", s),
        }
    }
}

impl fmt::Display for RuntimeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RuntimeError::EventLoop(s) => write!(f, "event loop: {}", s),
            RuntimeError::AlreadyRunning => write!(f, "VM is already running"),
            RuntimeError::NotStarted => write!(f, "VM has not been started"),
            RuntimeError::Shutdown(s) => write!(f, "shutdown: {}", s),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl std::error::Error for ConfigError {}
impl std::error::Error for BuildError {}
impl std::error::Error for RuntimeError {}

impl From<io::Error> for Error {
    fn from(err: io::Error) -> Self {
        Error::Io(err)
    }
}

impl From<ConfigError> for Error {
    fn from(err: ConfigError) -> Self {
        Error::Config(err)
    }
}

impl From<BuildError> for Error {
    fn from(err: BuildError) -> Self {
        Error::Build(err)
    }
}

impl From<RuntimeError> for Error {
    fn from(err: RuntimeError) -> Self {
        Error::Runtime(err)
    }
}
