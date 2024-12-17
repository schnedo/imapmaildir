mod parser;

use parser::parse_greeting;
use tokio::{
    io::{split, AsyncBufReadExt, AsyncWriteExt, BufReader, BufWriter, ReadHalf, WriteHalf},
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
        let mut writer = BufWriter::new(writer);

        let mut res = String::new();
        (reader.read_line(&mut res).await).expect("greeting should be readable");
        dbg!(&res);
        let greeting_response = parse_greeting(&res).expect("greeting should be parseable");
        dbg!(greeting_response);
        get_capabilities(&mut reader, &mut writer).await;

        Client { reader, writer }
    }
}

async fn get_capabilities(reader: &mut Reader, writer: &mut Writer) {
    (writer.write_all(b"abcd CAPABILITY\r\n"))
        .await
        .expect("writing capability command to buffer should succeed");
    (writer.flush())
        .await
        .expect("sending capability command should succeed");
    let mut res = String::new();
    (reader.read_line(&mut res).await).expect("greeting should be readable");
    dbg!(&res);
}
