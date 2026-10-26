use std::io;
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;
use tokio_rustls::client::TlsStream;
use tokio_rustls::rustls::pki_types::ServerName;

const TIMEOUT: Duration = Duration::from_secs(20);

#[derive(Debug, Clone, PartialEq)]
pub struct Entry {
    pub name: String,
    pub bytes: u64,
    pub dir: bool,
}

pub struct Ftp {
    control: BufReader<TlsStream<TcpStream>>,
    tls: TlsConnector,
    host: String,
}

impl Ftp {
    pub async fn connect(host: &str, port: u16, access_code: &str) -> io::Result<Ftp> {
        let tls = TlsConnector::from(super::tls::client_config());
        let tcp = within(TcpStream::connect((host, port))).await?;
        let control = BufReader::new(within(tls.connect(server_name(host)?, tcp)).await?);
        let mut ftp = Ftp {
            control,
            tls,
            host: host.to_string(),
        };
        ftp.expect(&[220]).await?;
        ftp.command("USER bblp", &[331]).await?;
        ftp.command(&format!("PASS {access_code}"), &[230]).await?;
        ftp.command("PBSZ 0", &[200]).await?;
        ftp.command("PROT P", &[200]).await?;
        ftp.command("TYPE I", &[200]).await?;
        Ok(ftp)
    }

    pub async fn list(&mut self, dir: &str) -> io::Result<Vec<Entry>> {
        let mut data = self.transfer(&format!("LIST {dir}")).await?;
        let text = read_all(&mut data, 1 << 20).await?;
        self.expect(&[226, 250]).await?;
        Ok(String::from_utf8_lossy(&text)
            .lines()
            .filter_map(parse_list_line)
            .collect())
    }

    pub async fn retrieve(&mut self, path: &str, max: u64) -> io::Result<Vec<u8>> {
        let mut data = self.transfer(&format!("RETR {path}")).await?;
        let body = read_all(&mut data, max).await?;
        self.expect(&[226, 250]).await?;
        Ok(body)
    }

    pub async fn delete(&mut self, path: &str) -> io::Result<()> {
        self.command(&format!("DELE {path}"), &[250]).await
    }

    async fn transfer(&mut self, cmd: &str) -> io::Result<TlsStream<TcpStream>> {
        self.send("PASV").await?;
        let (code, text) = self.reply().await?;
        if code != 227 {
            return Err(refused(code, &text));
        }
        let port = pasv_port(&text).ok_or_else(|| invalid(format!("unreadable reply {text:?}")))?;
        let tcp = within(TcpStream::connect((self.host.as_str(), port))).await?;
        self.send(cmd).await?;
        let (code, text) = self.reply().await?;
        if code != 125 && code != 150 {
            return Err(refused(code, &text));
        }
        within(self.tls.connect(server_name(&self.host)?, tcp)).await
    }

    async fn command(&mut self, cmd: &str, ok: &[u16]) -> io::Result<()> {
        self.send(cmd).await?;
        self.expect(ok).await
    }

    async fn expect(&mut self, ok: &[u16]) -> io::Result<()> {
        let (code, text) = self.reply().await?;
        if ok.contains(&code) {
            Ok(())
        } else {
            Err(refused(code, &text))
        }
    }

    async fn send(&mut self, cmd: &str) -> io::Result<()> {
        if cmd.contains(['\r', '\n']) {
            return Err(invalid("a line break in an FTP command".into()));
        }
        let conn = self.control.get_mut();
        within(conn.write_all(format!("{cmd}\r\n").as_bytes())).await?;
        within(conn.flush()).await
    }

    async fn reply(&mut self) -> io::Result<(u16, String)> {
        let mut line = String::new();
        if within(self.control.read_line(&mut line)).await? == 0 {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        let code: u16 = line
            .get(..3)
            .and_then(|c| c.parse().ok())
            .ok_or_else(|| invalid(format!("unreadable reply {line:?}")))?;
        if line.as_bytes().get(3) == Some(&b'-') {
            let end = format!("{code} ");
            loop {
                let mut more = String::new();
                if within(self.control.read_line(&mut more)).await? == 0 {
                    return Err(io::ErrorKind::UnexpectedEof.into());
                }
                if more.starts_with(&end) {
                    break;
                }
            }
        }
        Ok((code, line.trim_end().to_string()))
    }
}

async fn read_all<R: AsyncRead + Unpin>(r: &mut R, max: u64) -> io::Result<Vec<u8>> {
    let mut out = Vec::new();
    let mut buf = vec![0u8; 64 << 10];
    loop {
        let n = match within(r.read(&mut buf)).await {
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => 0,
            Err(e) => return Err(e),
        };
        if n == 0 {
            return Ok(out);
        }
        if (out.len() + n) as u64 > max {
            return Err(invalid(format!("longer than {max} bytes")));
        }
        out.extend_from_slice(&buf[..n]);
    }
}

async fn within<T>(f: impl Future<Output = io::Result<T>>) -> io::Result<T> {
    tokio::time::timeout(TIMEOUT, f).await.map_err(|_| {
        io::Error::new(
            io::ErrorKind::TimedOut,
            "the printer's FTP server stopped answering",
        )
    })?
}

fn server_name(host: &str) -> io::Result<ServerName<'static>> {
    ServerName::try_from(host.to_string()).map_err(|e| invalid(e.to_string()))
}

fn refused(code: u16, text: &str) -> io::Error {
    io::Error::other(format!("the printer's FTP server replied {code}: {text}"))
}

fn invalid(msg: String) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg)
}

fn pasv_port(text: &str) -> Option<u16> {
    let inside = text.get(text.find('(')? + 1..text.rfind(')')?)?;
    let numbers: Vec<u16> = inside
        .split(',')
        .map(|n| n.trim().parse().ok())
        .collect::<Option<_>>()?;
    match numbers[..] {
        [_, _, _, _, high, low] if high < 256 && low < 256 => Some(high << 8 | low),
        _ => None,
    }
}

fn parse_list_line(line: &str) -> Option<Entry> {
    let mut rest = line.trim_end();
    let mut fields = Vec::with_capacity(8);
    for _ in 0..8 {
        rest = rest.trim_start();
        let end = rest.find(char::is_whitespace)?;
        fields.push(&rest[..end]);
        rest = &rest[end..];
    }
    let name = rest.trim_start();
    if name.is_empty() || name == "." || name == ".." {
        return None;
    }
    Some(Entry {
        name: name.to_string(),
        bytes: fields[4].parse().ok()?,
        dir: fields[0].starts_with('d'),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::printer::FakePrinter;

    #[test]
    fn list_lines() {
        assert_eq!(
            parse_list_line(
                "-rw-rw-rw-   1 root  root   5505722 Jul 23 10:49 video_2026-07-23_08-45-39.avi"
            ),
            Some(Entry {
                name: "video_2026-07-23_08-45-39.avi".into(),
                bytes: 5505722,
                dir: false
            })
        );
        assert_eq!(
            parse_list_line("drwxrwxrwx 1 root root 0 Jul 23 10:49 thumbnail").map(|e| e.dir),
            Some(true)
        );
        assert_eq!(
            parse_list_line("-rw-rw-rw- 1 root root 4637734 Jul 21 02:21 Can dispenser.gcode.3mf")
                .map(|e| e.name),
            Some("Can dispenser.gcode.3mf".into())
        );
        assert_eq!(parse_list_line("total 12"), None);
    }

    #[test]
    fn passive_ports() {
        assert_eq!(
            pasv_port("227 Entering Passive Mode (10,0,0,7,195,80)"),
            Some(50000)
        );
        assert_eq!(
            pasv_port("227 Entering Passive Mode (10,0,0,7,300,80)"),
            None
        );
        assert_eq!(pasv_port("227 nonsense"), None);
    }

    #[tokio::test]
    async fn lists_retrieves_and_deletes() {
        let printer = FakePrinter::start("127.0.0.1", 0, 0, 0, "S", "code").await;
        printer.put_file("/timelapse/a.avi", b"hello".to_vec());
        printer.put_file("/timelapse/thumbnail/a.jpg", b"x".to_vec());
        let mut ftp = Ftp::connect("127.0.0.1", printer.ftp_port, "code")
            .await
            .unwrap();
        let entries = ftp.list("/timelapse").await.unwrap();
        assert_eq!(
            entries,
            [
                Entry {
                    name: "a.avi".into(),
                    bytes: 5,
                    dir: false
                },
                Entry {
                    name: "thumbnail".into(),
                    bytes: 0,
                    dir: true
                },
            ]
        );
        assert_eq!(ftp.retrieve("/timelapse/a.avi", 5).await.unwrap(), b"hello");
        assert!(ftp.retrieve("/timelapse/a.avi", 4).await.is_err());
        let mut ftp = Ftp::connect("127.0.0.1", printer.ftp_port, "code")
            .await
            .unwrap();
        ftp.delete("/timelapse/a.avi").await.unwrap();
        assert!(ftp.delete("/timelapse/a.avi").await.is_err());
        assert!(ftp.retrieve("/timelapse/a.avi", 5).await.is_err());
        assert_eq!(printer.files(), ["/timelapse/thumbnail/a.jpg"]);
    }

    #[tokio::test]
    async fn a_wrong_access_code_is_refused() {
        let printer = FakePrinter::start("127.0.0.1", 0, 0, 0, "S", "code").await;
        let err = Ftp::connect("127.0.0.1", printer.ftp_port, "nope")
            .await
            .err()
            .unwrap();
        assert!(err.to_string().contains("530"), "{err}");
    }
}
