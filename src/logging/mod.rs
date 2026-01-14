mod connected_to_journal;

use std::{
    backtrace::{Backtrace, BacktraceStatus},
    io::Write as _,
    panic::{self, PanicHookInfo},
    thread,
    time::SystemTime,
};

use anstyle::{AnsiColor, Effects};
use connected_to_journal::connected_to_journal;
use env_logger::Builder;
use log::{LevelFilter, error};

fn get_thread_name() -> String {
    if let Some(name) = thread::current().name() {
        format!("{name} ")
    } else {
        String::new()
    }
}

fn format_panic_info(info: &PanicHookInfo) -> String {
    let thread = get_thread_name();
    let location = info
        .location()
        .map_or("unknown location".to_string(), |location| {
            format!(
                "{}:{}:{}",
                location.file(),
                location.line(),
                location.column()
            )
        });
    let payload = info
        .payload_as_str()
        .map_or(String::new(), |payload| format!("\n{payload}"));
    let backtrace = Backtrace::capture();
    let backtrace = match backtrace.status() {
        BacktraceStatus::Unsupported => {
            "\nnote: backtraces unsupported on this machine".to_string()
        }
        BacktraceStatus::Disabled => {
            "\nnote: run with `RUST_BACKTRACE=1` environment variable to display a backtrace"
                .to_string()
        }
        BacktraceStatus::Captured => format!("\n{backtrace}"),
        _ => String::new(),
    };
    format!("{thread}panicked at {location}:{payload}{backtrace}")
}

pub fn init(level: LevelFilter) {
    let mut builder = Builder::new();
    builder.filter_level(level);
    if connected_to_journal() {
        builder.format(move |buf, record| {
            let mailbox = get_thread_name();
            writeln!(
                buf,
                "<{}>{}{}: {}",
                match record.level() {
                    log::Level::Error => 3,
                    log::Level::Warn => 4,
                    log::Level::Info => 6,
                    log::Level::Debug | log::Level::Trace => 7,
                },
                mailbox,
                record.target(),
                record.args()
            )
        });
        panic::set_hook(Box::new(|info| {
            for line in format_panic_info(info).lines() {
                eprintln!("<2>{line}");
            }
        }));
    } else {
        let subtle = AnsiColor::BrightBlack.on_default();
        builder.format(move |buf, record| {
            let now = SystemTime::now();
            write!(buf, "{subtle}[{subtle:#}").expect("logging buffer should be writable");
            if let Ok(now) = jiff::Timestamp::try_from(now) {
                write!(buf, "{now:.3} ").expect("logging buffer should be writable");
            } else {
                write!(buf, "timestamp_error ").expect("logging buffer should be writable");
            }
            let level_style = match record.level() {
                log::Level::Error => AnsiColor::Red.on_default().effects(Effects::BOLD),
                log::Level::Warn => AnsiColor::Yellow.on_default(),
                log::Level::Info => AnsiColor::Green.on_default(),
                log::Level::Debug => AnsiColor::Blue.on_default(),
                log::Level::Trace => AnsiColor::Cyan.on_default(),
            };
            let mailbox = if let Some(mailbox) = thread::current().name() {
                format!("{mailbox} ")
            } else {
                String::new()
            };
            write!(
                buf,
                "{level_style}{}{level_style:#} {mailbox}{}",
                record.level(),
                record.target(),
            )
            .expect("logging buffer should be writable");
            if let Some(line) = record.line() {
                write!(buf, ":{line}").expect("logging buffer should be writable");
            }
            write!(buf, "{subtle}]{subtle:#} ").expect("logging buffer should be writable");
            writeln!(buf, "{}", record.args())
        });
        panic::set_hook(Box::new(|info| {
            error!("{}", format_panic_info(info));
        }));
    }
    builder.init();
}
