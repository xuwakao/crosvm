// macOS panic hook — reuses the Linux panic hook since it's POSIX-compatible.

use std::env;
use std::fs::File;
use std::io::stderr;
use std::io::Read;
use std::panic;
use std::panic::PanicHookInfo;
use std::process::abort;
use std::string::String;

use base::error;
use base::FromRawDescriptor;
use base::IntoRawDescriptor;
use libc::close;
use libc::dup;
use libc::dup2;
use libc::STDERR_FILENO;

fn redirect_stderr() -> Option<(File, File)> {
    let mut fds = [0i32; 2];
    // SAFETY: pipe creates a pipe, dup/dup2 duplicate file descriptors.
    unsafe {
        let old_stderr = dup(STDERR_FILENO);
        if old_stderr == -1 {
            return None;
        }
        // macOS: use pipe() instead of pipe2 (no pipe2 on macOS).
        let ret = libc::pipe(fds.as_mut_ptr());
        if ret != 0 {
            return None;
        }
        // Set non-blocking on read end.
        libc::fcntl(fds[0], libc::F_SETFL, libc::O_NONBLOCK);
        let ret = dup2(fds[1], STDERR_FILENO);
        if ret == -1 {
            return None;
        }
        close(fds[1]);
        Some((
            File::from_raw_descriptor(fds[0]),
            File::from_raw_descriptor(old_stderr),
        ))
    }
}

fn restore_stderr(stderr_file: File) -> bool {
    let descriptor = stderr_file.into_raw_descriptor();
    // SAFETY: dup2 is safe with a valid descriptor.
    unsafe { dup2(descriptor, STDERR_FILENO) != -1 }
}

fn log_panic_info(
    default_panic: &(dyn Fn(&PanicHookInfo) + Sync + Send + 'static),
    info: &PanicHookInfo,
) {
    let stderr_handle = stderr();
    let _stderr_lock = stderr_handle.lock();

    let (mut read_file, old_stderr) = match redirect_stderr() {
        Some(f) => f,
        None => {
            error!(
                "failed to capture stderr during panic: {}",
                std::io::Error::last_os_error()
            );
            env::set_var("RUST_BACKTRACE", "1");
            default_panic(info);
            return;
        }
    };
    env::set_var("RUST_BACKTRACE", "1");
    default_panic(info);

    if !restore_stderr(old_stderr) {
        error!("failed to restore stderr during panic");
        return;
    }
    drop(_stderr_lock);

    let mut panic_output = String::new();
    let _ = read_file.read_to_string(&mut panic_output);
    for line in panic_output.lines() {
        error!("{}", line);
    }
}

pub fn set_panic_hook() {
    let default_panic = panic::take_hook();
    panic::set_hook(Box::new(move |info| {
        log_panic_info(default_panic.as_ref(), info);
        abort();
    }));
}
