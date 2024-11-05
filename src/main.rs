use config::Config;

mod config;

fn main() {
    let config = Config::load_from_file();
}
