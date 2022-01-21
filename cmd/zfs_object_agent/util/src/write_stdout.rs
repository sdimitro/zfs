/*!
Alternative to `println!` macros suitable for use in commands.

The `println!` flavors of macros are good for debugging output but not suitable for
production code since they panic when their output is not fully consumed (i.e. write
to a stream that has closed).

The println replacement macros in this module will terminate the process if writing
to stdout/stderr returns an error (like `EPIPE`) rather than cause a panic.
!*/

/// Similar to `print!` macro, except it terminates the process on write errors (does not panic).
#[macro_export]
macro_rules! write_stdout {
	($($arg:tt)*) => ({
        use std::io::Write;
        if let Err(_) = write!(std::io::stdout(), $($arg)*) {
            std::process::exit(0)
        }
	})
}

/// Similar to `println!` macro, except it terminates the process on write errors (does not panic).
#[macro_export]
macro_rules! writeln_stdout {
	($($arg:tt)*) => ({
        use std::io::Write;
        if let Err(_) = writeln!(std::io::stdout(), $($arg)*) {
            std::process::exit(0)
        }
	})
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
