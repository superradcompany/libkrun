// Copyright 2026 The Microsandbox Authors. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

use std::fs::File;
use std::io;
use std::os::fd::AsRawFd;
use std::sync::Arc;

use log::warn;

pub(crate) const MINIMUM_RANGE_BYTES: u64 = 64 * 1024 * 1024;
const HARD_LIMIT_MULTIPLIER: u64 = 2;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct DirtyRange {
    start: u64,
    end: u64,
}

impl DirtyRange {
    fn new(offset: u64, length: u64) -> Option<Self> {
        if length == 0 {
            return None;
        }

        Some(Self {
            start: offset,
            end: offset.saturating_add(length),
        })
    }

    fn merge(self, other: Self) -> Self {
        Self {
            start: self.start.min(other.start),
            end: self.end.max(other.end),
        }
    }

    fn len(self) -> u64 {
        self.end.saturating_sub(self.start)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum WritebackAction {
    Advisory(DirtyRange),
    Drain(DirtyRange),
}

#[derive(Debug)]
struct WritebackWindow {
    trigger_bytes: u64,
    hard_limit_bytes: u64,
    batch_logical_bytes: u64,
    batch_range: Option<DirtyRange>,
    outstanding_logical_bytes: u64,
    outstanding_range: Option<DirtyRange>,
}

impl WritebackWindow {
    fn new(trigger_bytes: u64) -> Self {
        Self {
            trigger_bytes,
            hard_limit_bytes: trigger_bytes.saturating_mul(HARD_LIMIT_MULTIPLIER),
            batch_logical_bytes: 0,
            batch_range: None,
            outstanding_logical_bytes: 0,
            outstanding_range: None,
        }
    }

    fn prepare_write(&self, length: u64) -> Option<DirtyRange> {
        let projected_bytes = self.outstanding_logical_bytes.saturating_add(length);
        (projected_bytes > self.hard_limit_bytes)
            .then_some(self.outstanding_range)
            .flatten()
    }

    fn record_write(&mut self, offset: u64, length: u64) -> Option<WritebackAction> {
        let range = DirtyRange::new(offset, length)?;

        self.batch_logical_bytes = self.batch_logical_bytes.saturating_add(length);
        self.batch_range = Some(
            self.batch_range
                .map_or(range, |existing| existing.merge(range)),
        );

        self.outstanding_logical_bytes = self.outstanding_logical_bytes.saturating_add(length);
        self.outstanding_range = Some(
            self.outstanding_range
                .map_or(range, |existing| existing.merge(range)),
        );

        if self.outstanding_logical_bytes >= self.hard_limit_bytes {
            return self.outstanding_range.map(WritebackAction::Drain);
        }

        let batch_range = self.batch_range?;
        if self.batch_logical_bytes >= self.trigger_bytes
            && batch_range.len() >= MINIMUM_RANGE_BYTES
        {
            return Some(WritebackAction::Advisory(batch_range));
        }

        None
    }

    fn complete_advisory(&mut self) {
        self.batch_logical_bytes = 0;
        self.batch_range = None;
    }

    fn complete_drain(&mut self) {
        self.complete_advisory();
        self.outstanding_logical_bytes = 0;
        self.outstanding_range = None;
    }

    fn complete_flush(&mut self) {
        self.complete_drain();
    }
}

/// Immutable inputs used to create a fresh controller when a block device is reactivated.
#[derive(Clone)]
pub(crate) struct BufferedWritebackConfig {
    file: Arc<File>,
    trigger_bytes: u64,
}

impl BufferedWritebackConfig {
    pub(crate) fn new(file: Arc<File>, trigger_bytes: u64) -> Self {
        Self {
            file,
            trigger_bytes,
        }
    }

    pub(crate) fn controller(&self) -> BufferedWritebackController {
        BufferedWritebackController {
            file: Arc::clone(&self.file),
            window: WritebackWindow::new(self.trigger_bytes),
        }
    }
}

/// Applies rolling advisory writeback and hard backpressure to one buffered raw-file block device.
///
/// The configured threshold starts asynchronous range writeback. Twice that threshold is the hard
/// logical-byte watermark: the device worker synchronously drains the accumulated range before it
/// accepts more dirtying writes. The hard drain constrains host page-cache exposure even when the
/// guest never issues `FLUSH`.
///
/// Neither operation acknowledges guest durability. Guest `FLUSH` completion still depends on the
/// existing full Imago sync after every earlier write has been processed by the block worker.
pub(crate) struct BufferedWritebackController {
    file: Arc<File>,
    window: WritebackWindow,
}

impl BufferedWritebackController {
    pub(crate) fn hard_limit_bytes(&self) -> u64 {
        self.window.hard_limit_bytes
    }

    pub(crate) fn prepare_write(&mut self, length: u64) -> io::Result<()> {
        if length > self.window.hard_limit_bytes {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "buffered block write exceeds the hard writeback watermark",
            ));
        }

        if let Some(range) = self.window.prepare_write(length) {
            self.drain(range)?;
            self.window.complete_drain();
        }
        Ok(())
    }

    pub(crate) fn record_write(&mut self, offset: u64, length: u64) -> io::Result<()> {
        match self.window.record_write(offset, length) {
            Some(WritebackAction::Advisory(range)) => {
                match self.sync_range(range, libc::SYNC_FILE_RANGE_WRITE) {
                    Ok(()) => self.window.complete_advisory(),
                    Err(error) => {
                        // Keep the range and counters intact. A later write retries the advisory
                        // operation, while the hard watermark still prevents unbounded dirtying.
                        warn!(
                            "Buffered block advisory writeback failed; retaining hard backpressure: {error}"
                        );
                    }
                }
                Ok(())
            }
            Some(WritebackAction::Drain(range)) => {
                self.drain(range)?;
                self.window.complete_drain();
                Ok(())
            }
            None => Ok(()),
        }
    }

    pub(crate) fn complete_flush(&mut self) {
        self.window.complete_flush();
    }

    fn drain(&self, range: DirtyRange) -> io::Result<()> {
        let flags = libc::SYNC_FILE_RANGE_WAIT_BEFORE
            | libc::SYNC_FILE_RANGE_WRITE
            | libc::SYNC_FILE_RANGE_WAIT_AFTER;
        self.sync_range(range, flags).map_err(|error| {
            warn!("Buffered block hard writeback drain failed; blocking further writes: {error}");
            error
        })
    }

    fn sync_range(&self, range: DirtyRange, flags: libc::c_uint) -> io::Result<()> {
        let offset = libc::off64_t::try_from(range.start).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "writeback range offset exceeds sync_file_range limits",
            )
        })?;
        let length = libc::off64_t::try_from(range.len()).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "writeback range length exceeds sync_file_range limits",
            )
        })?;

        // Safe: the controller owns a live descriptor for the exact backing inode reopened by
        // Imago, and the remaining arguments are validated integers plus Linux-defined flags.
        let result = unsafe { libc::sync_file_range(self.file.as_raw_fd(), offset, length, flags) };
        if result == 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }
}

//--------------------------------------------------------------------------------------------------
// Tests
//--------------------------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use std::fs::OpenOptions;
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::*;

    #[test]
    fn dirty_range_merges_out_of_order_writes() {
        let first = DirtyRange::new(128, 64).unwrap();
        let second = DirtyRange::new(32, 48).unwrap();
        assert_eq!(
            first.merge(second),
            DirtyRange {
                start: 32,
                end: 192
            }
        );
    }

    #[test]
    fn rolling_window_rearms_without_a_guest_flush() {
        let trigger = MINIMUM_RANGE_BYTES;
        let mut window = WritebackWindow::new(trigger);

        assert_eq!(
            window.record_write(0, trigger),
            Some(WritebackAction::Advisory(DirtyRange {
                start: 0,
                end: trigger,
            }))
        );
        window.complete_advisory();

        assert_eq!(
            window.record_write(trigger, trigger),
            Some(WritebackAction::Drain(DirtyRange {
                start: 0,
                end: trigger * 2,
            }))
        );
        window.complete_drain();

        assert_eq!(
            window.record_write(trigger * 2, trigger),
            Some(WritebackAction::Advisory(DirtyRange {
                start: trigger * 2,
                end: trigger * 3,
            }))
        );
    }

    #[test]
    fn projected_hard_limit_drains_before_another_write() {
        let trigger = MINIMUM_RANGE_BYTES;
        let mut window = WritebackWindow::new(trigger);
        assert!(matches!(
            window.record_write(0, trigger),
            Some(WritebackAction::Advisory(_))
        ));
        window.complete_advisory();

        assert_eq!(
            window.prepare_write(trigger + 1),
            Some(DirtyRange {
                start: 0,
                end: trigger,
            })
        );
    }

    #[test]
    fn successful_guest_flush_resets_both_watermarks() {
        let trigger = MINIMUM_RANGE_BYTES;
        let mut window = WritebackWindow::new(trigger);
        assert!(window.record_write(0, trigger).is_some());

        window.complete_flush();

        assert_eq!(window.batch_logical_bytes, 0);
        assert_eq!(window.batch_range, None);
        assert_eq!(window.outstanding_logical_bytes, 0);
        assert_eq!(window.outstanding_range, None);
        assert_eq!(window.prepare_write(trigger * 2), None);
    }

    #[test]
    fn empty_write_does_not_change_the_window() {
        let mut window = WritebackWindow::new(MINIMUM_RANGE_BYTES);
        assert_eq!(window.record_write(1024, 0), None);
        assert_eq!(window.batch_range, None);
        assert_eq!(window.outstanding_range, None);
    }

    #[test]
    fn oversized_request_cannot_bypass_the_hard_watermark() {
        let path = temporary_file_path("oversized");
        let file = OpenOptions::new()
            .create_new(true)
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();
        let mut controller = BufferedWritebackController {
            file: Arc::new(file),
            window: WritebackWindow::new(MINIMUM_RANGE_BYTES),
        };

        let error = controller
            .prepare_write(MINIMUM_RANGE_BYTES * HARD_LIMIT_MULTIPLIER + 1)
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);

        drop(controller);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn regular_file_supports_advisory_and_hard_writeback() {
        let path = temporary_file_path("regular");
        let file = OpenOptions::new()
            .create_new(true)
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();
        file.set_len(MINIMUM_RANGE_BYTES * 2).unwrap();

        let mut controller = BufferedWritebackController {
            file: Arc::new(file),
            window: WritebackWindow::new(MINIMUM_RANGE_BYTES),
        };
        controller.record_write(0, MINIMUM_RANGE_BYTES).unwrap();
        controller
            .record_write(MINIMUM_RANGE_BYTES, MINIMUM_RANGE_BYTES)
            .unwrap();
        assert_eq!(controller.window.outstanding_range, None);

        drop(controller);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn unsupported_file_cannot_bypass_the_hard_watermark() {
        let unsupported = OpenOptions::new().write(true).open("/dev/null").unwrap();
        let mut controller = BufferedWritebackController {
            file: Arc::new(unsupported),
            window: WritebackWindow::new(MINIMUM_RANGE_BYTES),
        };

        // The advisory failure is tolerated, but the accumulated range remains accounted.
        controller.record_write(0, MINIMUM_RANGE_BYTES).unwrap();
        assert!(controller
            .record_write(MINIMUM_RANGE_BYTES, MINIMUM_RANGE_BYTES)
            .is_err());
        assert!(controller.prepare_write(1).is_err());
    }

    fn temporary_file_path(label: &str) -> std::path::PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "libkrun-writeback-{label}-{}-{nonce}",
            std::process::id()
        ))
    }
}
