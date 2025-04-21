mod connected_to_journal;

use std::io::Write as _;

use connected_to_journal::connected_to_journal;
use env_logger::Env;

pub fn init() {
    if connected_to_journal() {
        env_logger::Builder::from_env(Env::default().default_filter_or("debug"))
            .format(|buf, record| {
                writeln!(
                    buf,
                    "<{}>{}: {}",
                    match record.level() {
                        log::Level::Error => 3,
                        log::Level::Warn => 4,
                        log::Level::Info => 6,
                        log::Level::Debug => 7,
                        log::Level::Trace => 7,
                    },
                    record.target(),
                    record.args()
                )
            })
            .init()
    } else {
        env_logger::Builder::from_env(Env::default().default_filter_or("trace")).init();
    }
}
