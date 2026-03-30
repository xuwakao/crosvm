// macOS EventAsync — pipe-backed event for async notification.
//
// On macOS, Event is backed by a pipe (not eventfd). signal() writes 1 byte,
// so next_val() reads 1 byte and returns a count of 1. This differs from
// Linux where eventfd provides an 8-byte counter.

use base::Event;

use crate::AsyncError;
use crate::AsyncResult;
use crate::EventAsync;
use crate::Executor;

impl EventAsync {
    pub fn new(event: Event, ex: &Executor) -> AsyncResult<EventAsync> {
        ex.async_from(event)
            .map(|io_source| EventAsync { io_source })
    }

    /// Gets the next value from the event.
    /// On macOS (pipe-backed), reads 1 byte and returns 1.
    /// On Linux (eventfd), reads 8 bytes and returns the counter value.
    pub async fn next_val(&self) -> AsyncResult<u64> {
        // Read 1 byte — pipe signal.
        let (n, _v) = self
            .io_source
            .read_to_vec(None, vec![0u8; 1])
            .await?;
        if n == 0 {
            return Err(AsyncError::EventAsync(base::Error::new(libc::ENODATA)));
        }
        Ok(1)
    }

    /// Gets the next value from the event and resets.
    pub async fn next_val_reset(&self) -> AsyncResult<u64> {
        self.next_val().await
    }
}
