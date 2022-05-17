use std::collections::VecDeque;
use std::fmt::Write as FmtWrite;
use std::fs::OpenOptions;
use std::io::BufWriter;
use std::io::Write;
use std::panic;
use std::panic::PanicInfo;
use std::path::Path;
use std::process;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::thread;

use atomic_counter::AtomicCounter;
use atomic_counter::RelaxedCounter;
use backtrace::Backtrace;
use chrono::DateTime;
use chrono::Local;
use chrono::Utc;
use lazy_static::lazy_static;
pub use log::log;
use log::*;
use log4rs::append::console::ConsoleAppender;
use log4rs::append::file::FileAppender;
use log4rs::append::Append;
use log4rs::config::Appender;
use log4rs::config::Config;
use log4rs::config::Deserialize;
use log4rs::config::Deserializers;
use log4rs::config::Logger;
use log4rs::config::Root;
use log4rs::encode::pattern::PatternEncoder;
use log4rs::filter::threshold::ThresholdFilter;
use signal_hook::consts::SIGUSR1;
use signal_hook::iterator::exfiltrator::SignalOnly;
use signal_hook::iterator::SignalsInfo;
use signal_hook::low_level::emulate_default_handler;

use crate::lazy_static_ptr;
use crate::measure;
use crate::tunable;
use crate::with_alloctag_hf;
use crate::writeln_stderr;
use crate::TrackingAllocator;
use crate::ALLOCATOR_PRINT_MIN_ALLOCS;
use crate::ALLOCATOR_PRINT_MIN_BYTES;

type PanicHook = Box<dyn Fn(&panic::PanicInfo) + Sync + Send>;

lazy_static_ptr! {
    static ref LOG_MESSAGES: std::sync::Mutex<VecDeque<LogMessage>> = Default::default();
}

tunable! {
    static ref MAX_LOG_MESSAGES: usize = 100_000;
    static ref PANIC_LOG_FOLDER: String = "/var/log/zoa".to_string();
    pub static ref SUPER_EXPENSIVE_TRACE: AtomicBool = AtomicBool::new(false);
    static ref MIN_MESSAGE_CAPACITY: usize = 64;
    static ref MAX_REUSEABLE_MESSAGE_CAPACITY: usize = 256;
}

lazy_static! {
    static ref LOG_PATTERN: String = "[{d(%Y-%m-%d %H:%M:%S%.3f)}][{t}][{l}] {m}{n}".to_string();
    static ref DEFAULT_HOOK: std::sync::Mutex<Option<PanicHook>> = Default::default();
    static ref PANIC_COUNTER: RelaxedCounter = RelaxedCounter::new(0);
}

struct LogMessage {
    date: DateTime<Utc>,
    level: Level,
    message: String,
}

#[macro_export]
macro_rules! super_trace {
    ($($arg:tt)+) => ({
        if $crate::SUPER_EXPENSIVE_TRACE.load(std::sync::atomic::Ordering::Relaxed) {
            $crate::log!(log::Level::Trace, $($arg)+)
        }
    })
}

pub fn get_logging_level(verbosity: u64) -> LevelFilter {
    match verbosity {
        0 => LevelFilter::Warn,
        1 => LevelFilter::Info,
        2 => LevelFilter::Debug,
        _ => LevelFilter::Trace,
    }
}

/// An appender which logs to a memory buffer.
#[derive(Debug)]
pub struct BufferAppender {}

impl Append for BufferAppender {
    fn append(&self, record: &Record) -> anyhow::Result<()> {
        const TAG: &str = "logging BufferAppender";
        if let Ok(mut messages) = LOG_MESSAGES.lock() {
            let mut reuse = None;
            while messages.len() >= *MAX_LOG_MESSAGES {
                reuse = Some(messages.pop_front().unwrap().message);
            }
            let mut message = match reuse {
                Some(reuse) if reuse.capacity() <= *MAX_REUSEABLE_MESSAGE_CAPACITY => reuse,
                _ => with_alloctag_hf(TAG, || String::with_capacity(*MIN_MESSAGE_CAPACITY)),
            };
            message.truncate(0);
            with_alloctag_hf(TAG, || {
                write!(message, "[{}] {}", record.target(), record.args())
            })?;

            messages.push_back(LogMessage {
                date: chrono::Utc::now(),
                level: record.level(),
                message,
            });
        }

        Ok(())
    }

    fn flush(&self) {}
}

impl BufferAppender {
    fn get_writer(filename: String) -> Box<dyn Write> {
        let writer_path = format!("{}/{}", *PANIC_LOG_FOLDER, filename);
        match OpenOptions::new()
            .append(true)
            .create(true)
            .open(&writer_path)
        {
            Ok(file) => {
                info!("dumping info to {}", writer_path);
                Box::new(BufWriter::new(file)) as Box<dyn Write>
            }
            Err(_) => Box::new(std::io::stderr()),
        }
    }

    fn dump_log_messages<W>(mut writer: W)
    where
        W: Write,
    {
        if let Ok(messages) = LOG_MESSAGES.lock() {
            for message in messages.iter() {
                writeln!(
                    writer,
                    "[{}][{}]{}",
                    message
                        .date
                        .with_timezone(&Local)
                        .format("%Y-%m-%d %H:%M:%S%.3f"),
                    message.level,
                    message.message,
                )
                .unwrap();
            }
        }
    }

    /// Dump log messages in memory to a file or stderr.
    pub fn dump(info: &PanicInfo) {
        let mut output = Self::get_writer(format!(
            "panic_pid{}_{}.out",
            process::id(),
            PANIC_COUNTER.inc()
        ));
        Self::dump_log_messages(&mut output);

        let location = info.location().unwrap();
        let msg = match info.payload().downcast_ref::<&'static str>() {
            Some(s) => *s,
            None => match info.payload().downcast_ref::<String>() {
                Some(s) => &s[..],
                None => "Box<dyn Any>",
            },
        };
        let current_thread = thread::current();
        let name = current_thread.name().unwrap_or("<unnamed>");
        writeln!(
            output,
            "thread '{}' panicked at '{}', {}",
            name, msg, location
        )
        .unwrap();
        writeln!(output, "stack backtrace:\n{:?}", Backtrace::new()).unwrap();
        output.flush().ok();
    }
}

#[derive(Debug, serde::Deserialize)]
pub struct BufferAppenderConfig {}

pub struct BufferAppenderDeserializer;

impl Deserialize for BufferAppenderDeserializer {
    type Trait = dyn Append;
    type Config = BufferAppenderConfig;

    fn deserialize(
        &self,
        _config: BufferAppenderConfig,
        _deserializers: &Deserializers,
    ) -> anyhow::Result<Box<dyn Append>> {
        Ok(Box::new(BufferAppender {}))
    }
}

fn setup_console_logging(verbosity: u64) {
    SUPER_EXPENSIVE_TRACE.store(verbosity > 3, Ordering::Relaxed);
    let config = Config::builder()
        .appender(
            Appender::builder()
                .filter(Box::new(ThresholdFilter::new(get_logging_level(verbosity))))
                .build("memory", Box::new(BufferAppender {})),
        )
        .appender(
            Appender::builder()
                .filter(Box::new(ThresholdFilter::new(get_logging_level(verbosity))))
                .build(
                    "stdout",
                    Box::new(
                        ConsoleAppender::builder()
                            .encoder(Box::new(PatternEncoder::new(&*LOG_PATTERN)))
                            .build(),
                    ),
                ),
        )
        // These are too chatty by default, so increase their minimum log level.
        .logger(Logger::builder().build("rusoto_core::request", LevelFilter::Info))
        .logger(Logger::builder().build("want", LevelFilter::Debug))
        .build(
            Root::builder()
                .appender("memory")
                .appender("stdout")
                .build(get_logging_level(verbosity)),
        )
        .unwrap();

    log4rs::init_config(config).unwrap();
}

fn setup_logfile(verbosity: u64, logfile: &Path) {
    let config = Config::builder()
        .appender(
            Appender::builder()
                .filter(Box::new(ThresholdFilter::new(LevelFilter::Trace)))
                .build("memory", Box::new(BufferAppender {})),
        )
        .appender(
            Appender::builder()
                .filter(Box::new(ThresholdFilter::new(get_logging_level(verbosity))))
                .build(
                    "logfile",
                    Box::new(
                        FileAppender::builder()
                            .encoder(Box::new(PatternEncoder::new(&*LOG_PATTERN)))
                            .build(logfile)
                            .unwrap(),
                    ),
                ),
        )
        // These are too chatty by default, so increase their minimum log level.
        .logger(Logger::builder().build("rusoto_core::request", LevelFilter::Info))
        .logger(Logger::builder().build("want", LevelFilter::Debug))
        .build(
            Root::builder()
                .appender("memory")
                .appender("logfile")
                .build(LevelFilter::Trace),
        )
        .unwrap();

    log4rs::init_config(config).unwrap();
}

pub fn setup_logging(
    verbosity: u64,
    file_name: Option<&Path>,
    log_config: Option<&Path>,
    quiet_start: bool,
) {
    /*
     * Panic hook to dump trace logs to a file, in case of a panic.
     */
    let default_hook = panic::take_hook();
    panic::set_hook(Box::new(move |info| {
        // Call the default panic hook to print out the panic info.
        default_hook(info);

        BufferAppender::dump(info);
    }));

    match log_config {
        Some(config) => {
            let mut deserializers = log4rs::config::Deserializers::new();
            deserializers.insert("buffer", BufferAppenderDeserializer);
            log4rs::init_file(config, deserializers).unwrap();
        }
        None => {
            match file_name {
                Some(logfile) => {
                    setup_logfile(verbosity, logfile);
                }
                None => {
                    /*
                     * When neither the log_config nor a log file is specified
                     * log to console.
                     */
                    setup_console_logging(verbosity);
                }
            }
        }
    };

    if !quiet_start {
        // error!() should be used when an invalid state is encountered; the related
        // operation will fail and the program may exit.  E.g. an invalid request
        // was received from the client (kernel).
        error!("logging level ERROR enabled");

        // warn!() should be used when something unexpected has happened, but it can
        // be recovered from.
        warn!("logging level WARN enabled");

        // info!() should be used for very high level operations which are expected
        // to happen infrequently (no more than once per minute in typical
        // operation).  e.g. opening/closing a pool, long-lived background tasks,
        // things that might be in `zpool history -i`.
        info!("logging level INFO enabled");

        // debug!() can be used for all but the most frequent operations.
        // e.g. not every single read/write/free operation, but perhaps for every
        // call to S3.
        debug!("logging level DEBUG enabled");

        // trace!() can be used for frequent operation. But note that we evaluate
        // all the trace! statements in prod and there is some cost involved in
        // string processing, memory allocation, global lock etc.
        trace!("logging level TRACE enabled");

        // super_trace!() can be used for log statements that are very frequent.
        // There is a very high performance penalty for enabling these statements.
        super_trace!("logging super expensive TRACE enabled");

        // Log all the tunables.
        tunable::log_config();
    }
}

/// Dump trace logs and memory tracking stats when receiving SIGUSR1
pub fn register_siguser1_to_dump_tracing() -> Result<(), std::io::Error> {
    let mut signals = SignalsInfo::<SignalOnly>::new(&[SIGUSR1])?;
    std::thread::spawn(move || {
        for signum in &mut signals {
            match signum {
                SIGUSR1 => {
                    let info_path = format!(
                        "SIGUSR1_pid{}_{}.out",
                        process::id(),
                        chrono::Local::now().format("%Y-%m-%d-%H:%M:%S%.3f"),
                    );
                    let mut out = BufferAppender::get_writer(info_path);

                    writeln!(out, "=== Log Traces").ok();
                    BufferAppender::dump_log_messages(&mut out);

                    writeln!(out, "\n=== Memory Statistics").ok();
                    writeln!(
                        out,
                        "{}",
                        TrackingAllocator::format(
                            *ALLOCATOR_PRINT_MIN_ALLOCS,
                            *ALLOCATOR_PRINT_MIN_BYTES
                        )
                    )
                    .ok();

                    writeln!(out, "\n=== Measurements").ok();
                    writeln!(out, "{}", measure::dump()).ok();

                    out.flush().ok();
                }
                _ => {
                    // This should never be executed as we are registered for
                    // SIGUSER1 only.
                    writeln_stderr!("Got an unexpected signal: {:?}", signum);
                    emulate_default_handler(signum).unwrap();
                }
            }
        }
    });
    Ok(())
}
