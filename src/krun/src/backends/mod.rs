//! Custom backend support for libkrun.
//!
//! This module provides traits and types for implementing custom backends
//! for libkrun's virtio devices.
//!
//! # Console Backends
//!
//! The `console` module provides the [`ConsolePortBackend`] trait for
//! in-process virtio-console port backends. See [`console::ConsolePortBackend`]
//! for details.
//!
//! # Network Backends
//!
//! The `net` module re-exports the `NetBackend` trait which allows custom
//! network implementations. See [`net::NetBackend`] for details.
//!
//! # Filesystem Backends
//!
//! The `fs` module provides the `DynFileSystem` trait, an object-safe version
//! of the `FileSystem` trait. See [`fs::DynFileSystem`] for details.

//--------------------------------------------------------------------------------------------------
// Modules
//--------------------------------------------------------------------------------------------------

#[cfg(not(target_os = "windows"))]
pub mod console;

#[cfg(all(
    not(any(feature = "tee", feature = "aws-nitro")),
    not(target_os = "windows")
))]
pub mod fs;

#[cfg(feature = "net")]
pub mod net;

//--------------------------------------------------------------------------------------------------
// Re-Exports
//--------------------------------------------------------------------------------------------------

#[cfg(not(target_os = "windows"))]
pub use console::ConsolePortBackend;

#[cfg(all(
    not(any(feature = "tee", feature = "aws-nitro")),
    not(target_os = "windows")
))]
pub use fs::DynFileSystem;

#[cfg(feature = "net")]
pub use net::NetBackend;
