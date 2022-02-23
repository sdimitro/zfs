/*!
Alternative to `println!` macros suitable for use in commands.

The `println!` flavors of macros are good for debugging output but not suitable for
production code since they panic when their output is not fully consumed (i.e. write
to a stream that has closed).

The println replacement macros in this module will terminate the process if writing
to stdout/stderr returns an error (like `EPIPE`) rather than cause a panic.

These replacement macros also introduce buffering when stdout is not pointing to
tty, significantly improving the performance of utilities that redirect their
output to files.
!*/

use lazy_static::lazy_static;
use libc::atexit;
use std::{io::BufWriter, sync::Mutex};

lazy_static! {
    pub static ref BUFFERED_STDOUT_HANDLE: Mutex<BufWriter<std::io::Stdout>> = {
        // Ensure that any leftover data in the buffer are flushed before
        // terminating the process.
        unsafe { atexit(flush_stdout) };
        Mutex::new(BufWriter::new(std::io::stdout()))
    };
}

extern "C" fn flush_stdout() {
    use std::io::Write;
    match BUFFERED_STDOUT_HANDLE.try_lock() {
        Ok(mut hdl) => {
            if let Err(e) = hdl.flush() {
                crate::writeln_stderr!("{}", e);
                std::process::exit(0);
            }
        }
        Err(_) => {
            crate::writeln_stderr!(
                "CANNOT FLUSH STDOUT BUFFER BECAUSE ITS LOCK IS HELD BY ANOTHER THREAD"
            );
            std::process::exit(0);
        }
    };
}

pub extern crate atty;

/// Similar to `print!` macro, except it terminates the process on write errors (does not panic).
/// The output to stdout is also buffered if stdout is not pointing to tty.
#[macro_export]
macro_rules! write_stdout {
    ($($arg:tt)*) => {{
        use std::io::Write;
        let res = if $crate::write_stdout::atty::is($crate::write_stdout::atty::Stream::Stdout) {
            write!(std::io::stdout(), $($arg)*)
        } else {
            let mut hdl = $crate::write_stdout::BUFFERED_STDOUT_HANDLE.lock().unwrap();
            write!(hdl, $($arg)*)
        };
        if res.is_err() {
            std::process::exit(0)
        }
    }}
}

/// Similar to `println!` macro, except it terminates the process on write errors (does not panic).
/// The output to stdout is also buffered if stdout is not pointing to tty.
#[macro_export]
macro_rules! writeln_stdout {
	($($arg:tt)*) => {{
        use std::io::Write;
        let res = if $crate::write_stdout::atty::is($crate::write_stdout::atty::Stream::Stdout) {
            writeln!(std::io::stdout(), $($arg)*)
        } else {
            let mut hdl = $crate::write_stdout::BUFFERED_STDOUT_HANDLE.lock().unwrap();
            writeln!(hdl, $($arg)*)
        };
        if res.is_err() {
            std::process::exit(0)
        }
	}}
}

/// Similar to `eprint!` macro, except it terminates the process on write errors (does not panic).
#[macro_export]
macro_rules! write_stderr {
	($($arg:tt)*) => ({
        use std::io::Write;
        if let Err(_) = write!(std::io::stderr(), $($arg)*) {
            std::process::exit(0)
        }
	})
}

/// Similar to `eprintln!` macro, except it terminates the process on write errors (does not panic).
#[macro_export]
macro_rules! writeln_stderr {
	($($arg:tt)*) => ({
        use std::io::Write;
        if let Err(_) = writeln!(std::io::stderr(), $($arg)*) {
            std::process::exit(0)
        }
	})
}
