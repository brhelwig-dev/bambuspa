//! The chamber camera on :6000: a TLS stream of JPEG frames.

use std::time::Duration;

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;
use tokio_rustls::rustls::pki_types::ServerName;

/// Chamber JPEGs are tens of KB; anything near this means corruption.
const MAX_FRAME_SIZE: u32 = 8 << 20;

/// The 80-byte handshake: LE 0x40, LE 0x3000, eight zero bytes, then the
/// username and access code each zero-padded to 32 bytes. Layout matches
/// ha-bambulab's ChamberImageThread.
pub fn auth_packet(username: &str, access_code: &str) -> Result<[u8; 80], String> {
    if username.len() > 32 || access_code.len() > 32 {
        return Err("username/access code longer than 32 bytes".into());
    }
    let mut p = [0u8; 80];
    p[0..4].copy_from_slice(&0x40u32.to_le_bytes());
    p[4..8].copy_from_slice(&0x3000u32.to_le_bytes());
    p[16..16 + username.len()].copy_from_slice(username.as_bytes());
    p[48..48 + access_code.len()].copy_from_slice(access_code.as_bytes());
    Ok(p)
}

/// Reads one frame: a 16-byte header whose first four bytes are the
/// little-endian JPEG size, then the JPEG itself. `Ok(None)` is a clean end of
/// stream before a header.
pub async fn read_frame<R: AsyncRead + Unpin>(r: &mut R) -> std::io::Result<Option<Vec<u8>>> {
    let mut header = [0u8; 16];
    let first = r.read(&mut header).await?;
    if first == 0 {
        return Ok(None);
    }
    r.read_exact(&mut header[first..]).await?;
    let size = u32::from_le_bytes(header[0..4].try_into().unwrap());
    if !(4..=MAX_FRAME_SIZE).contains(&size) {
        return Err(invalid(format!("implausible frame size {size}")));
    }
    let mut img = vec![0u8; size as usize];
    r.read_exact(&mut img)
        .await
        .map_err(|e| invalid(format!("truncated frame: {e}")))?;
    if img[..2] != [0xFF, 0xD8] {
        return Err(invalid("frame missing JPEG start marker".into()));
    }
    if img[img.len() - 2..] != [0xFF, 0xD9] {
        return Err(invalid("frame missing JPEG end marker".into()));
    }
    Ok(Some(img))
}

fn invalid(msg: String) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidData, msg)
}

/// Dials the camera, authenticates, and passes each JPEG to `frame` until the
/// connection breaks. Cancel by dropping the future.
pub async fn stream_frames(
    host: &str,
    port: u16,
    username: &str,
    access_code: &str,
    mut frame: impl FnMut(Vec<u8>),
) -> std::io::Result<()> {
    let auth = auth_packet(username, access_code).map_err(invalid)?;
    let tcp = tokio::time::timeout(Duration::from_secs(10), TcpStream::connect((host, port)))
        .await
        .map_err(|_| {
            std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "connecting to the camera timed out",
            )
        })??;
    let name = ServerName::try_from(host.to_string()).map_err(|e| invalid(e.to_string()))?;
    let mut conn = TlsConnector::from(super::tls::client_config())
        .connect(name, tcp)
        .await?;
    conn.write_all(&auth).await?;
    conn.flush().await?;
    loop {
        match read_frame(&mut conn).await? {
            Some(img) => frame(img),
            None => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "camera closed the stream",
                ));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    pub fn encode(jpeg: &[u8]) -> Vec<u8> {
        let mut out = vec![0u8; 16];
        out[0..4].copy_from_slice(&(jpeg.len() as u32).to_le_bytes());
        out.extend_from_slice(jpeg);
        out
    }

    const JPEG: &[u8] = &[0xFF, 0xD8, 1, 2, 3, 0xFF, 0xD9];

    #[test]
    fn auth_packet_layout() {
        let p = auth_packet("bblp", "12345678").unwrap();
        assert_eq!(&p[0..8], &[0x40, 0, 0, 0, 0, 0x30, 0, 0]);
        assert_eq!(&p[8..16], &[0; 8]);
        assert_eq!(&p[16..20], b"bblp");
        assert!(p[20..48].iter().all(|&b| b == 0));
        assert_eq!(&p[48..56], b"12345678");
        assert!(auth_packet(&"x".repeat(33), "a").is_err());
        assert!(auth_packet("a", &"x".repeat(33)).is_err());
        assert!(auth_packet("a", &"x".repeat(32)).is_ok());
    }

    #[tokio::test]
    async fn read_frame_round_trip_and_rejections() {
        let bytes = encode(JPEG);
        assert_eq!(read_frame(&mut &bytes[..]).await.unwrap().unwrap(), JPEG);

        let bad_magic = encode(&[0, 0, 1, 0xFF, 0xD9]);
        assert!(read_frame(&mut &bad_magic[..]).await.is_err());
        let bad_end = encode(&[0xFF, 0xD8, 1, 0, 0]);
        assert!(read_frame(&mut &bad_end[..]).await.is_err());

        let mut huge = [0u8; 16];
        huge[0..4].copy_from_slice(&100_000_000u32.to_le_bytes());
        let err = read_frame(&mut &huge[..]).await.unwrap_err();
        assert!(err.to_string().contains("implausible"));

        let truncated = &bytes[..bytes.len() - 2];
        assert!(
            read_frame(&mut &truncated[..])
                .await
                .unwrap_err()
                .to_string()
                .contains("truncated")
        );

        assert!(read_frame(&mut &[][..]).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn streams_from_a_tls_camera() {
        let camera = crate::testing::FakeCamera::start(vec![JPEG.to_vec(), JPEG.to_vec()]).await;
        let mut got = Vec::new();
        let result =
            stream_frames("127.0.0.1", camera.port, "bblp", "secret", |f| got.push(f)).await;
        assert!(result.is_err(), "the stream ends when the camera hangs up");
        assert_eq!(got.len(), 2);
        assert_eq!(
            camera.auth().await,
            auth_packet("bblp", "secret").unwrap().to_vec()
        );
    }
}
