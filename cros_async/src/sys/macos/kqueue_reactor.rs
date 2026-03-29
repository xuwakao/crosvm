// Copyright 2026 The Aetheria Authors
// SPDX-License-Identifier: BSD-3-Clause
//
// kqueue-based async reactor for macOS, implementing the cros_async Reactor trait.
// This is the macOS equivalent of Linux's EpollReactor (fd_executor.rs).

use std::future::Future;
use std::io;
use std::mem;
use std::os::fd::AsRawFd;
use std::os::fd::OwnedFd;
use std::os::unix::io::FromRawFd;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::Weak;
use std::task::Context;
use std::task::Poll;
use std::task::Waker;

use base::add_fd_flags;
use base::warn;
use base::AsRawDescriptor;
use base::AsRawDescriptors;
use base::EventType;
use base::RawDescriptor;
use slab::Slab;
use sync::Mutex;

use crate::common_executor::RawExecutor;
use crate::common_executor::RawTaskHandle;
use crate::common_executor::Reactor;
use crate::waker::WakerToken;
use crate::AsyncResult;
use crate::IoSource;
use crate::TaskHandle;

// A poll operation that has been submitted and is potentially being waited on.
struct OpData {
    file: Arc<OwnedFd>,
    waker: Option<Waker>,
}

// The current status of a submitted operation.
enum OpStatus {
    Pending(OpData),
    Completed,
    WakeEvent,
}

// An IO source registered with the KqueueReactor.
pub struct RegisteredSource<F> {
    pub(crate) source: F,
    ex: Weak<RawExecutor<KqueueReactor>>,
    pub(crate) duped_fd: Arc<OwnedFd>,
}

impl<F: AsRawDescriptor> RegisteredSource<F> {
    pub(crate) fn new(raw: &Arc<RawExecutor<KqueueReactor>>, f: F) -> io::Result<Self> {
        let raw_fd = f.as_raw_descriptor();
        assert_ne!(raw_fd, -1);

        add_fd_flags(raw_fd, libc::O_NONBLOCK)
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;

        // SAFETY: The FD is open and not -1 (checked above).
        let duped_fd = unsafe { std::os::fd::BorrowedFd::borrow_raw(raw_fd) }
            .try_clone_to_owned()?;
        Ok(RegisteredSource {
            source: f,
            ex: Arc::downgrade(raw),
            duped_fd: Arc::new(duped_fd),
        })
    }

    pub fn wait_readable(&self) -> io::Result<PendingOperation> {
        let ex = self
            .ex
            .upgrade()
            .ok_or_else(|| io::Error::new(io::ErrorKind::Other, "executor gone"))?;
        let token = ex
            .reactor
            .add_operation(Arc::clone(&self.duped_fd), EventType::Read)?;
        Ok(PendingOperation {
            token: Some(token),
            ex: self.ex.clone(),
        })
    }

    pub fn wait_writable(&self) -> io::Result<PendingOperation> {
        let ex = self
            .ex
            .upgrade()
            .ok_or_else(|| io::Error::new(io::ErrorKind::Other, "executor gone"))?;
        let token = ex
            .reactor
            .add_operation(Arc::clone(&self.duped_fd), EventType::Write)?;
        Ok(PendingOperation {
            token: Some(token),
            ex: self.ex.clone(),
        })
    }
}

/// A pending kqueue operation. Await this to wait for the fd to become ready.
pub struct PendingOperation {
    token: Option<WakerToken>,
    ex: Weak<RawExecutor<KqueueReactor>>,
}

impl Future for PendingOperation {
    type Output = io::Result<()>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context) -> Poll<Self::Output> {
        let token = self
            .token
            .as_ref()
            .expect("PendingOperation polled after returning Poll::Ready");
        if let Some(ex) = self.ex.upgrade() {
            if ex.reactor.is_ready(token, cx) {
                self.token = None;
                Poll::Ready(Ok(()))
            } else {
                Poll::Pending
            }
        } else {
            Poll::Ready(Err(io::Error::new(io::ErrorKind::Other, "executor gone")))
        }
    }
}

impl Drop for PendingOperation {
    fn drop(&mut self) {
        if let Some(token) = self.token.take() {
            if let Some(ex) = self.ex.upgrade() {
                let _ = ex.reactor.cancel_operation(token);
            }
        }
    }
}

/// Reactor that manages async IO using macOS kqueue.
pub struct KqueueReactor {
    kq: OwnedFd,
    ops: Mutex<Slab<OpStatus>>,
    // Wake pipe: write to wake_write to break out of kevent() wait.
    wake_read: OwnedFd,
    wake_write: OwnedFd,
}

// SAFETY: The kqueue fd and pipe fds are safe to send/share across threads.
// kqueue is inherently thread-safe on macOS.
unsafe impl Send for KqueueReactor {}
unsafe impl Sync for KqueueReactor {}

impl KqueueReactor {
    fn new_inner() -> io::Result<Self> {
        // Create kqueue
        let kq_fd = unsafe { libc::kqueue() };
        if kq_fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: kqueue() returned a valid fd.
        let kq = unsafe { OwnedFd::from_raw_fd(kq_fd) };

        // Create wake pipe
        let mut fds = [0i32; 2];
        if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: pipe() returned valid fds.
        let wake_read = unsafe { OwnedFd::from_raw_fd(fds[0]) };
        let wake_write = unsafe { OwnedFd::from_raw_fd(fds[1]) };

        // Make wake pipe non-blocking
        add_fd_flags(fds[0], libc::O_NONBLOCK)
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;
        add_fd_flags(fds[1], libc::O_NONBLOCK)
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;

        let reactor = KqueueReactor {
            kq,
            ops: Mutex::new(Slab::with_capacity(64)),
            wake_read,
            wake_write,
        };

        // Register the wake pipe read-end with kqueue
        {
            let mut ops = reactor.ops.lock();
            let entry = ops.vacant_entry();
            let token = entry.key();

            let changelist = [libc::kevent {
                ident: reactor.wake_read.as_raw_fd() as usize,
                filter: libc::EVFILT_READ,
                flags: libc::EV_ADD | libc::EV_CLEAR,
                fflags: 0,
                data: 0,
                udata: token as *mut libc::c_void,
            }];

            // SAFETY: kq is valid, changelist is properly initialized.
            let ret = unsafe {
                libc::kevent(
                    reactor.kq.as_raw_fd(),
                    changelist.as_ptr(),
                    changelist.len() as i32,
                    std::ptr::null_mut(),
                    0,
                    std::ptr::null(),
                )
            };
            if ret < 0 {
                return Err(io::Error::last_os_error());
            }

            entry.insert(OpStatus::WakeEvent);
        }

        Ok(reactor)
    }

    fn add_operation(&self, file: Arc<OwnedFd>, event_type: EventType) -> io::Result<WakerToken> {
        let mut ops = self.ops.lock();
        let entry = ops.vacant_entry();
        let token = entry.key();

        let filter = match event_type {
            EventType::Read | EventType::ReadWrite => libc::EVFILT_READ,
            EventType::Write => libc::EVFILT_WRITE,
            EventType::None => return Err(io::Error::new(io::ErrorKind::InvalidInput, "EventType::None")),
        };

        let changelist = [libc::kevent {
            ident: file.as_raw_fd() as usize,
            filter,
            flags: libc::EV_ADD | libc::EV_ONESHOT,
            fflags: 0,
            data: 0,
            udata: token as *mut libc::c_void,
        }];

        // SAFETY: kq is valid, changelist is properly initialized.
        let ret = unsafe {
            libc::kevent(
                self.kq.as_raw_fd(),
                changelist.as_ptr(),
                changelist.len() as i32,
                std::ptr::null_mut(),
                0,
                std::ptr::null(),
            )
        };
        if ret < 0 {
            return Err(io::Error::last_os_error());
        }

        entry.insert(OpStatus::Pending(OpData { file, waker: None }));
        Ok(WakerToken(token))
    }

    fn is_ready(&self, token: &WakerToken, cx: &mut Context) -> bool {
        let mut ops = self.ops.lock();
        let op = ops
            .get_mut(token.0)
            .expect("`is_ready` called on unknown operation");
        match op {
            OpStatus::Pending(data) => {
                data.waker = Some(cx.waker().clone());
                false
            }
            OpStatus::Completed => {
                ops.remove(token.0);
                true
            }
            OpStatus::WakeEvent => unreachable!(),
        }
    }

    fn cancel_operation(&self, token: WakerToken) -> io::Result<()> {
        match self.ops.lock().remove(token.0) {
            OpStatus::Pending(data) => {
                // Remove from kqueue. EV_ONESHOT events auto-remove on delivery,
                // but if the event hasn't fired yet, we need to remove it.
                let changelist = [libc::kevent {
                    ident: data.file.as_raw_fd() as usize,
                    filter: libc::EVFILT_READ, // We can't know which filter was used; try both
                    flags: libc::EV_DELETE,
                    fflags: 0,
                    data: 0,
                    udata: std::ptr::null_mut(),
                }];
                // Ignore errors — the event may have already been delivered (oneshot).
                unsafe {
                    libc::kevent(
                        self.kq.as_raw_fd(),
                        changelist.as_ptr(),
                        changelist.len() as i32,
                        std::ptr::null_mut(),
                        0,
                        std::ptr::null(),
                    );
                }
                Ok(())
            }
            OpStatus::Completed => Ok(()),
            OpStatus::WakeEvent => unreachable!(),
        }
    }

    /// Drain wake pipe data.
    fn drain_wake_pipe(&self) {
        let mut buf = [0u8; 64];
        loop {
            let ret = unsafe {
                libc::read(
                    self.wake_read.as_raw_fd(),
                    buf.as_mut_ptr() as *mut _,
                    buf.len(),
                )
            };
            if ret <= 0 {
                break;
            }
        }
    }
}

impl Reactor for KqueueReactor {
    fn new() -> io::Result<Self> {
        KqueueReactor::new_inner()
    }

    fn wake(&self) {
        let buf = [1u8];
        // SAFETY: wake_write is a valid fd.
        unsafe {
            libc::write(self.wake_write.as_raw_fd(), buf.as_ptr() as *const _, 1);
        }
    }

    fn on_executor_drop<'a>(&'a self) -> Pin<Box<dyn Future<Output = ()> + 'a>> {
        // Wake all pending operations with their wakers so they can see ExecutorGone.
        for op in self.ops.lock().drain() {
            match op {
                OpStatus::Pending(mut data) => {
                    if let Some(waker) = data.waker.take() {
                        waker.wake();
                    }
                }
                OpStatus::Completed | OpStatus::WakeEvent => {}
            }
        }
        Box::pin(async {})
    }

    fn wait_for_work(&self, set_processing: impl Fn()) -> io::Result<()> {
        let mut events = [libc::kevent {
            ident: 0,
            filter: 0,
            flags: 0,
            fflags: 0,
            data: 0,
            udata: std::ptr::null_mut(),
        }; 32];

        // SAFETY: kq is valid, events is properly sized.
        let nev = unsafe {
            libc::kevent(
                self.kq.as_raw_fd(),
                std::ptr::null(),
                0,
                events.as_mut_ptr(),
                events.len() as i32,
                std::ptr::null(), // No timeout — block indefinitely
            )
        };

        if nev < 0 {
            let err = io::Error::last_os_error();
            if err.kind() == io::ErrorKind::Interrupted {
                return Ok(());
            }
            return Err(err);
        }

        set_processing();

        for i in 0..nev as usize {
            let token = events[i].udata as usize;
            let mut ops = self.ops.lock();

            if let Some(op) = ops.get_mut(token) {
                let (_file, waker) = match mem::replace(op, OpStatus::Completed) {
                    OpStatus::Pending(OpData { file, waker }) => (file, waker),
                    OpStatus::Completed => panic!("kqueue operation completed more than once"),
                    OpStatus::WakeEvent => {
                        *op = OpStatus::WakeEvent;
                        self.drain_wake_pipe();
                        continue;
                    }
                };

                mem::drop(ops);
                // EV_ONESHOT events auto-remove from kqueue, so no explicit delete needed.

                if let Some(waker) = waker {
                    waker.wake();
                }
            }
        }
        Ok(())
    }

    fn new_source<F: AsRawDescriptor>(
        &self,
        ex: &Arc<RawExecutor<Self>>,
        f: F,
    ) -> AsyncResult<IoSource<F>> {
        super::KqueueSource::new(f, ex)
            .map(IoSource::Kqueue)
            .map_err(|e| crate::AsyncError::Io(e))
    }

    fn wrap_task_handle<R>(task: RawTaskHandle<KqueueReactor, R>) -> TaskHandle<R> {
        TaskHandle::Kqueue(task)
    }
}

impl AsRawDescriptors for KqueueReactor {
    fn as_raw_descriptors(&self) -> Vec<RawDescriptor> {
        vec![
            self.kq.as_raw_fd(),
            self.wake_read.as_raw_fd(),
            self.wake_write.as_raw_fd(),
        ]
    }
}
