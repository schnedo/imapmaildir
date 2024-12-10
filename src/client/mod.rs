mod parser;

use parser::parse_greeting;
use tokio::{
    io::{split, AsyncBufReadExt, BufReader, BufWriter, ReadHalf, WriteHalf},
    net::TcpStream,
};
use tokio_native_tls::{native_tls, TlsConnector, TlsStream};

use crate::config::Config;

type Reader = BufReader<ReadHalf<TlsStream<TcpStream>>>;
type Writer = BufWriter<WriteHalf<TlsStream<TcpStream>>>;

pub struct Client {
    reader: Reader,
    writer: Writer,
}

impl Client {
    pub async fn new(config: &Config) -> Self {
        let tls = native_tls::TlsConnector::new().expect("native tls should be available");
        let tls = TlsConnector::from(tls);
        let stream = (TcpStream::connect((config.host(), config.port)).await)
            .expect("connection to server should succeed");
        let stream =
            (tls.connect(config.host(), stream).await).expect("upgrading to tls should succeed");

        let (reader, writer) = split(stream);
        let mut reader = BufReader::new(reader);
        let writer = BufWriter::new(writer);

        let mut res = String::new();
        (reader.read_line(&mut res).await).expect("greeting should be readable");
        dbg!(&res);
        let greeting_response = parse_greeting(&res).expect("greeting should be parseable");
        dbg!(greeting_response);

        Client { reader, writer }
    }
}

fn get_capabilities(reader: &Reader, writer: &Writer) {}
