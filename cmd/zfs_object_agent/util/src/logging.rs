use crate::tunable::log_tunable_config;
use crate::{get_tunable, with_alloctag_hf};
use backtrace::Backtrace;
use lazy_static::lazy_static;
use log::*;
use log4rs::append::console::ConsoleAppender;
use log4rs::append::file::FileAppender;
use log4rs::append::Append;
use log4rs::config::{Appender, Config, Root};
use log4rs::config::{Deserialize, Deserializers, Logger};
use log4rs::encode::pattern::PatternEncoder;
use log4rs::filter::threshold::ThresholdFilter;
use std::collections::VecDeque;
use std::fs::OpenOptions;
use std::io::Write;
use std::panic::PanicInfo;
use std::sync::atomic::{AtomicBool, AtomicPtr, Ordering};
use std::{panic, process, ptr, thread};

static LOG_MESSAGES_PTR: AtomicPtr<std::sync::Mutex<VecDeque<String>>> =
    AtomicPtr::new(ptr::null_mut());

type PanicHook = Box<dyn Fn(&panic::PanicInfo) + Sync + Send>;

lazy_static! {
    static ref LOG_PATTERN: String = "[{d(%Y-%m-%d %H:%M:%S%.3f)}][{t}][{l}] {m}{n}".to_string();
    static ref LOG_MESSAGES: std::sync::Mutex<VecDeque<String>> = {
        let mut inner = Default::default();
        LOG_MESSAGES_PTR.store(&mut inner, Ordering::Relaxed);
        inner
    };
    static ref MAX_LOG_MESSAGES: usize = get_tunable("max_log_messages", 100_000);
    static ref PANIC_LOG_FOLDER: String =
        get_tunable("panic_log_folder", "/var/log/zoa".to_string());
    static ref DEFAULT_HOOK: std::sync::Mutex<Option<PanicHook>> = Default::default();
    pub static ref SUPER_EXPENSIVE_TRACE: AtomicBool =
        AtomicBool::new(get_tunable("super_expensive_trace", false));
}

#[macro_export]
macro_rules! super_trace {
    ($($arg:tt)+) => ({
        if $crate::SUPER_EXPENSIVE_TRACE.load(std::sync::atomic::Ordering::Relaxed) {
            log!(log::Level::Trace, $($arg)+)
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
        let str = with_alloctag_hf("logging BufferAppender", || {
            format!(
                "[{}][{}][{}] {}",
                chrono::Local::now().format("%Y-%m-%d %H:%M:%S%.3f"),
                record.target(),
                record.level(),
                record.args()
            )
        });

        if let Ok(mut messages) = LOG_MESSAGES.lock() {
            while messages.len() >= *MAX_LOG_MESSAGES {
                messages.pop_front();
            }
            messages.push_back(str);
        }

        Ok(())
    }

    fn flush(&self) {}
}

impl BufferAppender {
    fn get_writer() -> Box<dyn Write> {
        match OpenOptions::new().append(true).create(true).open(format!(
            "{}/panic_{}.log",
            PANIC_LOG_FOLDER.as_str(),
            process::id()
        )) {
            Ok(file) => Box::new(file) as Box<dyn Write>,
            Err(_) => Box::new(std::io::stderr()),
        }
    }

    /// Dump log messages in memory to a file or stderr.
    pub fn dump(info: &PanicInfo) {
        let mut output = Self::get_writer();
        if let Ok(messages) = LOG_MESSAGES.lock() {
            for message in messages.iter() {
                writeln!(output, "{}", message).unwrap();
            }
        }

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
        // rusoto_core::request is very chatty when set to debug. So, set it to info.
        .logger(Logger::builder().build("rusoto_core::request", LevelFilter::Info))
        .build(
            Root::builder()
                .appender("memory")
                .appender("stdout")
                .build(get_logging_level(verbosity)),
        )
        .unwrap();

    log4rs::init_config(config).unwrap();
}

fn setup_logfile(verbosity: u64, logfile: &str) {
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
        // rusoto_core::request is very chatty when set to debug. So, set it to info.
        .logger(Logger::builder().build("rusoto_core::request", LevelFilter::Info))
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
    file_name: Option<&str>,
    log_config: Option<&str>,
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
        log_tunable_config();
    }
}
