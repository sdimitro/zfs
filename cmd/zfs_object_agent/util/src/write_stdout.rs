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
use std::io::BufWriter;
use std::io::Write;
use std::sync::Mutex;

lazy_static! {
    pub static ref STDOUT: Mutex<Box<dyn std::io::Write + Send>> = {
        if atty::is(atty::Stream::Stdout) {
            Mutex::new(Box::new(std::io::stdout()))
        } else {
            // Ensure that any leftover data in the buffer are flushed before
            // terminating the process.
            unsafe { atexit(flush_stdout_atexit) };
            Mutex::new(Box::new(BufWriter::new(std::io::stdout())))
        }
    };
}

extern "C" fn flush_stdout_atexit() {
    match STDOUT.try_lock() {
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
        if let Err(_) = write!($crate::write_stdout::STDOUT.lock().unwrap(), $($arg)*) {
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
        if let Err(_) = writeln!($crate::write_stdout::STDOUT.lock().unwrap(), $($arg)*) {
            std::process::exit(0)
        }
	}}
}

pub fn flush_stdout() -> Result<(), std::io::Error> {
    crate::write_stdout::STDOUT.lock().unwrap().flush()
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
