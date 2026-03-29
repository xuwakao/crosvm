// Copyright 2026 The Aetheria Authors
// SPDX-License-Identifier: BSD-3-Clause
//
// macOS console input thread — real implementation using kqueue-based WaitContext.

use std::collections::VecDeque;
use std::io;
use std::sync::Arc;
use std::time::Duration;
use std::time::Instant;

use anyhow::Context;
use base::error;
use base::Event;
use base::EventToken;
use base::WaitContext;
use base::WorkerThread;
use sync::Mutex;

use crate::serial::sys::InStreamType;
use crate::serial_device::SerialInput;

/// Starts a thread that reads input and sends the input back via the provided buffer.
///
/// The caller should listen on `in_avail_evt` for events. When `in_avail_evt` signals that data
/// is available, the caller should lock `input_buffer` and read data out of the inner
/// `VecDeque`.
pub(in crate::virtio::console) fn spawn_input_thread(
    mut input: InStreamType,
    in_avail_evt: Event,
    input_buffer: Arc<Mutex<VecDeque<u8>>>,
) -> WorkerThread<InStreamType> {
    WorkerThread::start("v_console_input", move |kill_evt| {
        // If there is already data, signal immediately.
        if !input_buffer.lock().is_empty() {
            in_avail_evt.signal().unwrap();
        }
        if let Err(e) = read_input(&mut input, &in_avail_evt, input_buffer, kill_evt) {
            error!("console input thread exited with error: {:#}", e);
        }
        input
    })
}

fn read_input(
    input: &mut InStreamType,
    thread_in_avail_evt: &Event,
    buffer: Arc<Mutex<VecDeque<u8>>>,
    kill_evt: Event,
) -> anyhow::Result<()> {
    #[derive(EventToken)]
    enum Token {
        ConsoleEvent,
        Kill,
    }

    let wait_ctx: WaitContext<Token> = WaitContext::build_with(&[
        (&kill_evt, Token::Kill),
        (input.get_read_notifier(), Token::ConsoleEvent),
    ])
    .context("failed creating WaitContext")?;

    let mut kill_timeout = None;
    let mut rx_buf = [0u8; 1 << 12];
    'wait: loop {
        let events = wait_ctx.wait().context("Failed to wait for events")?;
        for event in events.iter() {
            match event.token {
                Token::Kill => {
                    // Ignore the kill event until there are no other events to process so that
                    // we drain `input` as much as possible.
                    if events.iter().all(|e| matches!(e.token, Token::Kill)) {
                        break 'wait;
                    }
                    const TIMEOUT_DURATION: Duration = Duration::from_millis(500);
                    match kill_timeout {
                        None => {
                            kill_timeout = Some(Instant::now() + TIMEOUT_DURATION);
                        }
                        Some(t) => {
                            if Instant::now() >= t {
                                error!(
                                    "failed to drain console input within {:?}, giving up",
                                    TIMEOUT_DURATION
                                );
                                break 'wait;
                            }
                        }
                    }
                }
                Token::ConsoleEvent => {
                    match input.read(&mut rx_buf) {
                        Ok(0) => break 'wait, // Assume the stream of input has ended.
                        Ok(size) => {
                            buffer.lock().extend(&rx_buf[0..size]);
                            thread_in_avail_evt.signal().unwrap();
                        }
                        // Being interrupted is not an error, but everything else is.
                        Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                        Err(e) => {
                            return Err(e).context("failed to read console input");
                        }
                    }
                }
            }
        }
    }

    Ok(())
}
