use std::fs::File;
use std::io::{self, BufReader, Read, Seek, SeekFrom};
use std::ops::Range;
use std::sync::Mutex;

const MAX_DEPTH: usize = 8;

/// A motion-JPEG AVI read from a file, with only where each frame sits held
/// in memory.
pub struct Video {
    file: Mutex<File>,
    frames: Vec<Range<u64>>,
    pub interval_us: u32,
}

impl Video {
    pub fn parse(mut file: File) -> Result<Video, String> {
        let len = file.seek(SeekFrom::End(0)).map_err(|e| e.to_string())?;
        let mut reader = Reader {
            inner: BufReader::new(file),
            pos: len,
        };
        let mut head = [0u8; 12];
        if len < 12
            || reader.read_at(0, &mut head).is_err()
            || &head[0..4] != b"RIFF"
            || &head[8..12] != b"AVI "
        {
            return Err("not an AVI file".into());
        }
        let (mut interval_us, mut frames) = (0, Vec::new());
        walk(&mut reader, 0, len, 0, &mut interval_us, &mut frames).map_err(|e| e.to_string())?;
        if interval_us == 0 {
            return Err("the AVI has no main header".into());
        }
        if frames.is_empty() {
            return Err("the AVI holds no JPEG frames".into());
        }
        Ok(Video {
            file: Mutex::new(reader.inner.into_inner()),
            frames,
            interval_us,
        })
    }

    pub fn len(&self) -> usize {
        self.frames.len()
    }

    pub fn frame(&self, i: usize) -> io::Result<Option<Vec<u8>>> {
        let Some(range) = self.frames.get(i) else {
            return Ok(None);
        };
        let mut jpeg = vec![0; (range.end - range.start) as usize];
        let mut file = self.file.lock().unwrap_or_else(|e| e.into_inner());
        file.seek(SeekFrom::Start(range.start))?;
        file.read_exact(&mut jpeg)?;
        Ok(Some(jpeg))
    }
}

/// Reads at absolute offsets, keeping the buffer across the short forward
/// hops of a walk through the chunks.
struct Reader {
    inner: BufReader<File>,
    pos: u64,
}

impl Reader {
    fn read_at(&mut self, at: u64, buf: &mut [u8]) -> io::Result<()> {
        if at != self.pos {
            match i64::try_from(at as i128 - self.pos as i128) {
                Ok(hop) => self.inner.seek_relative(hop)?,
                Err(_) => {
                    self.inner.seek(SeekFrom::Start(at))?;
                }
            }
        }
        self.pos = at + buf.len() as u64;
        self.inner.read_exact(buf)
    }
}

fn walk(
    r: &mut Reader,
    mut at: u64,
    end: u64,
    depth: usize,
    interval_us: &mut u32,
    frames: &mut Vec<Range<u64>>,
) -> io::Result<()> {
    while at + 8 <= end {
        let mut head = [0u8; 8];
        r.read_at(at, &mut head)?;
        let id = &head[0..4];
        let size = u32::from_le_bytes(head[4..8].try_into().unwrap()) as u64;
        let body = at + 8;
        let stop = body.saturating_add(size).min(end);
        match id {
            b"RIFF" | b"LIST" if stop >= body + 4 && depth < MAX_DEPTH => {
                walk(r, body + 4, stop, depth + 1, interval_us, frames)?
            }
            b"avih" if stop >= body + 4 => {
                let mut value = [0u8; 4];
                r.read_at(body, &mut value)?;
                *interval_us = u32::from_le_bytes(value)
            }
            _ if (&id[2..4] == b"dc" || &id[2..4] == b"db") && stop - body == size && size >= 2 => {
                let mut magic = [0u8; 2];
                r.read_at(body, &mut magic)?;
                if magic == [0xFF, 0xD8] {
                    frames.push(body..stop)
                }
            }
            _ => {}
        }
        at = body.saturating_add(size).saturating_add(size & 1);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::printer::{PIXEL_JPEG, avi};
    use std::io::Write;

    fn parse(data: Vec<u8>) -> Result<Video, String> {
        let mut file = tempfile::tempfile().unwrap();
        file.write_all(&data).unwrap();
        Video::parse(file)
    }

    fn frame(video: &Video, i: usize) -> Option<Vec<u8>> {
        video.frame(i).unwrap()
    }

    #[test]
    fn frames_come_out_in_order() {
        let odd = [PIXEL_JPEG, &[0]].concat();
        let video = parse(avi(&[PIXEL_JPEG, &odd, PIXEL_JPEG], 41_666)).unwrap();
        assert_eq!(video.len(), 3);
        assert_eq!(video.interval_us, 41_666);
        assert_eq!(frame(&video, 0).as_deref(), Some(PIXEL_JPEG));
        assert_eq!(frame(&video, 1), Some(odd));
        assert_eq!(frame(&video, 2).as_deref(), Some(PIXEL_JPEG));
        assert_eq!(frame(&video, 3), None);
    }

    #[test]
    fn a_truncated_file_keeps_its_whole_frames() {
        let mut data = avi(&[PIXEL_JPEG, PIXEL_JPEG], 100_000);
        data.truncate(data.len() - 10);
        assert_eq!(parse(data).unwrap().len(), 1);
    }

    #[test]
    fn deep_nesting_is_not_followed() {
        let levels = 100_000u32;
        let inner = avi(&[PIXEL_JPEG], 100_000);
        let mut data = Vec::with_capacity(inner.len() + 12 * levels as usize);
        for i in (0..levels).rev() {
            data.extend_from_slice(b"LIST");
            data.extend_from_slice(&(inner.len() as u32 + 4 + 12 * i).to_le_bytes());
            data.extend_from_slice(b"rec ");
        }
        data.extend(inner);
        let mut riff = b"RIFF".to_vec();
        riff.extend_from_slice(&(data.len() as u32 + 4).to_le_bytes());
        riff.extend_from_slice(b"AVI ");
        riff.extend(data);
        assert!(parse(riff).is_err());
    }

    #[test]
    fn other_files_are_refused() {
        assert_eq!(
            parse(b"RIFF\0\0\0\0WAVE".to_vec()).err().unwrap(),
            "not an AVI file"
        );
        assert_eq!(
            parse(avi(&[], 100_000)).err().unwrap(),
            "the AVI holds no JPEG frames"
        );
    }
}
