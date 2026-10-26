//! Reads the JPEG frames out of the printer's motion-JPEG AVI timelapses.
//! Chunks are walked in order rather than through the `idx1` index, which
//! some of the printer's files lack.

use std::ops::Range;

pub struct Video {
    data: Vec<u8>,
    frames: Vec<Range<usize>>,
    pub interval_us: u32,
}

impl Video {
    pub fn parse(data: Vec<u8>) -> Result<Video, String> {
        if data.len() < 12 || &data[0..4] != b"RIFF" || &data[8..12] != b"AVI " {
            return Err("not an AVI file".into());
        }
        let (mut interval_us, mut frames) = (0, Vec::new());
        walk(&data, 0, data.len(), &mut interval_us, &mut frames);
        if interval_us == 0 {
            return Err("the AVI has no main header".into());
        }
        if frames.is_empty() {
            return Err("the AVI holds no JPEG frames".into());
        }
        Ok(Video {
            data,
            frames,
            interval_us,
        })
    }

    pub fn len(&self) -> usize {
        self.frames.len()
    }

    pub fn frame(&self, i: usize) -> Option<&[u8]> {
        self.frames.get(i).map(|r| &self.data[r.clone()])
    }
}

fn walk(
    data: &[u8],
    mut at: usize,
    end: usize,
    interval_us: &mut u32,
    frames: &mut Vec<Range<usize>>,
) {
    while at + 8 <= end {
        let id = &data[at..at + 4];
        let size = u32::from_le_bytes(data[at + 4..at + 8].try_into().unwrap()) as usize;
        let body = at + 8;
        let stop = body.saturating_add(size).min(end);
        match id {
            b"RIFF" | b"LIST" if stop >= body + 4 => {
                walk(data, body + 4, stop, interval_us, frames)
            }
            b"avih" if stop >= body + 4 => {
                *interval_us = u32::from_le_bytes(data[body..body + 4].try_into().unwrap())
            }
            _ if (&id[2..4] == b"dc" || &id[2..4] == b"db")
                && stop - body == size
                && data[body..stop].starts_with(&[0xFF, 0xD8]) =>
            {
                frames.push(body..stop)
            }
            _ => {}
        }
        at = body.saturating_add(size).saturating_add(size & 1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::printer::{PIXEL_JPEG, avi};

    #[test]
    fn frames_come_out_in_order() {
        let odd = [PIXEL_JPEG, &[0]].concat();
        let video = Video::parse(avi(&[PIXEL_JPEG, &odd, PIXEL_JPEG], 41_666)).unwrap();
        assert_eq!(video.len(), 3);
        assert_eq!(video.interval_us, 41_666);
        assert_eq!(video.frame(0), Some(PIXEL_JPEG));
        assert_eq!(video.frame(1), Some(&odd[..]));
        assert_eq!(video.frame(2), Some(PIXEL_JPEG));
        assert_eq!(video.frame(3), None);
    }

    #[test]
    fn a_truncated_file_keeps_its_whole_frames() {
        let mut data = avi(&[PIXEL_JPEG, PIXEL_JPEG], 100_000);
        data.truncate(data.len() - 10);
        assert_eq!(Video::parse(data).unwrap().len(), 1);
    }

    #[test]
    fn other_files_are_refused() {
        assert_eq!(
            Video::parse(b"RIFF\0\0\0\0WAVE".to_vec()).err().unwrap(),
            "not an AVI file"
        );
        assert_eq!(
            Video::parse(avi(&[], 100_000)).err().unwrap(),
            "the AVI holds no JPEG frames"
        );
    }
}
