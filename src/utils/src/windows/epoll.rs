// Copyright 2026 Super Rad Company.
// SPDX-License-Identifier: Apache-2.0

//! Windows waitable-handle backend exposed through libkrun's epoll-shaped API.

use std::collections::HashMap;
use std::io;
use std::os::windows::io::{AsRawHandle, RawHandle};
use std::sync::Mutex;

use bitflags::bitflags;

use crate::event::{EventSource, WaitContext, WaitEvent};

#[repr(i32)]
pub enum ControlOperation {
    Add,
    Modify,
    Delete,
}

bitflags! {
    pub struct EventSet: u32 {
        const IN = 0b0000_0001;
        const OUT = 0b0000_0010;
        const ERROR = 0b0000_0100;
        const READ_HANG_UP = 0b0000_1000;
        const EDGE_TRIGGERED = 0b0001_0000;
        const HANG_UP = 0b0010_0000;
        const PRIORITY = 0b0100_0000;
        const WAKE_UP = 0b1000_0000;
        const ONE_SHOT = 0b0001_0000_0000;
        const EXCLUSIVE = 0b0010_0000_0000;
    }
}

#[derive(Clone, Copy, Default)]
pub struct EpollEvent {
    events: u32,
    data: u64,
}

#[derive(Debug)]
pub struct Epoll {
    context: Mutex<WaitContext>,
    // Store opaque handle values as integers so the synchronized registry is
    // safely movable with an event loop thread.
    registrations: Mutex<HashMap<usize, u64>>,
}

impl EpollEvent {
    pub fn new(events: EventSet, data: u64) -> Self {
        Self {
            events: events.bits(),
            data,
        }
    }

    pub fn events(&self) -> u32 {
        self.events
    }

    pub fn event_set(&self) -> EventSet {
        EventSet::from_bits_truncate(self.events)
    }

    pub fn data(&self) -> u64 {
        self.data
    }

    pub fn fd(&self) -> RawHandle {
        self.data as usize as RawHandle
    }
}

impl std::fmt::Debug for EpollEvent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{{ events: {}, data: {} }}", self.events(), self.data())
    }
}

impl Epoll {
    pub fn new() -> io::Result<Self> {
        Ok(Self {
            context: Mutex::new(WaitContext::new()),
            registrations: Mutex::new(HashMap::new()),
        })
    }

    pub fn ctl(
        &self,
        operation: ControlOperation,
        handle: RawHandle,
        event: &EpollEvent,
    ) -> io::Result<()> {
        let mut context = self.context.lock().unwrap();
        let mut registrations = self.registrations.lock().unwrap();

        match operation {
            ControlOperation::Add => {
                context.add(
                    EventSource::waitable_handle(handle, event.data),
                    event_set_to_wait_event_set(event.event_set()),
                )?;
                registrations.insert(handle as usize, event.data);
                Ok(())
            }
            ControlOperation::Modify => {
                let token = registrations
                    .get(&(handle as usize))
                    .copied()
                    .ok_or_else(|| {
                        io::Error::new(io::ErrorKind::NotFound, "handle is not registered")
                    })?;
                context.modify(token, event_set_to_wait_event_set(event.event_set()))
            }
            ControlOperation::Delete => {
                let token = registrations.remove(&(handle as usize)).ok_or_else(|| {
                    io::Error::new(io::ErrorKind::NotFound, "handle is not registered")
                })?;
                context.delete(token)
            }
        }
    }

    pub fn wait(
        &self,
        max_events: usize,
        timeout: i32,
        events: &mut [EpollEvent],
    ) -> io::Result<usize> {
        let mut wait_events = vec![WaitEvent::default(); max_events.min(events.len())];
        let count = self
            .context
            .lock()
            .unwrap()
            .wait(timeout, &mut wait_events)?;

        for (index, event) in wait_events.into_iter().take(count).enumerate() {
            events[index] =
                EpollEvent::new(wait_event_set_to_event_set(event.events()), event.token());
        }

        Ok(count)
    }
}

impl AsRawHandle for Epoll {
    fn as_raw_handle(&self) -> RawHandle {
        std::ptr::null_mut()
    }
}

fn event_set_to_wait_event_set(events: EventSet) -> crate::event::EventSet {
    let mut wait_events = crate::event::EventSet::empty();
    if events.contains(EventSet::IN) {
        wait_events |= crate::event::EventSet::IN;
    }
    if events.contains(EventSet::OUT) {
        wait_events |= crate::event::EventSet::OUT;
    }
    wait_events
}

fn wait_event_set_to_event_set(events: crate::event::EventSet) -> EventSet {
    let mut epoll_events = EventSet::empty();
    if events.contains(crate::event::EventSet::IN) {
        epoll_events |= EventSet::IN;
    }
    if events.contains(crate::event::EventSet::OUT) {
        epoll_events |= EventSet::OUT;
    }
    if events.contains(crate::event::EventSet::ERROR) {
        epoll_events |= EventSet::ERROR;
    }
    if events.contains(crate::event::EventSet::HANG_UP) {
        epoll_events |= EventSet::HANG_UP;
    }
    if events.contains(crate::event::EventSet::READ_HANG_UP) {
        epoll_events |= EventSet::READ_HANG_UP;
    }
    epoll_events
}
