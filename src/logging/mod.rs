mod connected_to_journal;

use std::{io::Write as _, time::SystemTime};

use anstyle::{AnsiColor, Effects};
use connected_to_journal::connected_to_journal;
use env_logger::Env;

pub fn init(mailbox: Option<&str>) {
    let mut builder = env_logger::Builder::from_env(Env::default().default_filter_or("trace"));
    let mailbox = if let Some(mailbox) = mailbox {
        format!("{mailbox} ")
    } else {
        String::new()
    };
    if connected_to_journal() {
        builder.format(move |buf, record| {
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
    }
    builder.init();
}
