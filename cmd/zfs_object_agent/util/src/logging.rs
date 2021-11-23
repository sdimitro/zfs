use lazy_static::lazy_static;
use log::*;
use log4rs::append::console::ConsoleAppender;
use log4rs::append::file::FileAppender;
use log4rs::config::Logger;
use log4rs::config::{Appender, Config, Root};
use log4rs::encode::pattern::PatternEncoder;

lazy_static! {
    static ref LOG_PATTERN: String = "[{d(%Y-%m-%d %H:%M:%S%.3f)}][{t}][{l}] {m}{n}".to_string();
}

pub fn get_logging_level(verbosity: u64) -> LevelFilter {
    match verbosity {
        0 => LevelFilter::Warn,
        1 => LevelFilter::Info,
        2 => LevelFilter::Debug,
        _ => LevelFilter::Trace,
    }
}

fn setup_console_logging(verbosity: u64) {
    let config = Config::builder()
        .appender(
            Appender::builder().build(
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
                .appender("stdout")
                .build(get_logging_level(verbosity)),
        )
        .unwrap();

    log4rs::init_config(config).unwrap();
}

fn setup_logfile(verbosity: u64, logfile: &str) {
    let config = Config::builder()
        .appender(
            Appender::builder().build(
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
                .appender("logfile")
                .build(get_logging_level(verbosity)),
        )
        .unwrap();

    log4rs::init_config(config).unwrap();
}

pub fn setup_logging(verbosity: u64, file_name: Option<&str>, log_config: Option<&str>) {
    match log_config {
        Some(config) => {
            log4rs::init_file(config, Default::default()).unwrap();
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
}
