use std::{
    io::{BufRead, BufReader},
    net::TcpStream,
};

use native_tls::TlsConnector;

use crate::config::Config;

pub struct Client {}

impl Client {
    pub fn new(config: &Config) -> Self {
        let tls = TlsConnector::new().expect("native tls should be available");
        let stream = TcpStream::connect((config.host(), config.port))
            .expect("connection to server should succeed");
        let stream = tls
            .connect(config.host(), stream)
            .expect("upgrading to tls should succeed");

        let mut reader = BufReader::new(stream);
        let mut res = String::new();
        reader
            .read_line(&mut res)
            .expect("greeting should be readable");
        dbg!(res);

        Client {}
    }
}
