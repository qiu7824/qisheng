//! Self-authored encoded audio for download and metadata preservation checks.

use md5::{Digest, Md5};

pub fn mono_verbatim_flac() -> Vec<u8> {
    mono_verbatim_flac_frames(1)
}

pub fn mono_verbatim_flac_frames(frames: u8) -> Vec<u8> {
    assert!((1..=127).contains(&frames));
    let samples: Vec<i16> = (0..512 * usize::from(frames))
        .map(|index| ((index as f64 * 0.17).sin() * 12000.0) as i16)
        .collect();
    let mut pcm = Vec::with_capacity(samples.len() * 2);
    for sample in &samples {
        pcm.extend_from_slice(&sample.to_le_bytes());
    }
    let mut stream_info = Vec::with_capacity(34);
    stream_info.extend_from_slice(&512u16.to_be_bytes());
    stream_info.extend_from_slice(&512u16.to_be_bytes());
    stream_info.extend_from_slice(&[0; 6]);
    let format = (8000u64 << 44) | (15u64 << 36) | samples.len() as u64;
    stream_info.extend_from_slice(&format.to_be_bytes());
    stream_info.extend_from_slice(&Md5::digest(&pcm));
    let mut file = b"fLaC\x80\x00\x00\x22".to_vec();
    file.extend_from_slice(&stream_info);
    for (number, samples) in samples.chunks_exact(512).enumerate() {
        let mut frame = vec![0xff, 0xf8, 0x74, 0x08, number as u8, 0x01, 0xff];
        frame.push(crc8(&frame));
        frame.push(0x02);
        for sample in samples {
            frame.extend_from_slice(&sample.to_be_bytes());
        }
        frame.extend_from_slice(&crc16(&frame).to_be_bytes());
        file.extend_from_slice(&frame);
    }
    file
}

/// MPEG-1 Layer III, mono, 44.1kHz, 128kbps, no CRC or bit reservoir.
pub fn silent_mpeg_layer3() -> Vec<u8> {
    let mut file = Vec::with_capacity(417 * 12);
    for _ in 0..12 {
        file.extend_from_slice(&[0xff, 0xfb, 0x90, 0xc0]);
        file.extend_from_slice(&[0; 413]);
    }
    file
}

pub fn mono_pcm32_wav(samples: &[i32]) -> Vec<u8> {
    let bytes = samples.len() as u32 * 4;
    let mut file = Vec::with_capacity(bytes as usize + 44);
    file.extend_from_slice(b"RIFF");
    file.extend_from_slice(&(bytes + 36).to_le_bytes());
    file.extend_from_slice(b"WAVEfmt ");
    file.extend_from_slice(&16u32.to_le_bytes());
    file.extend_from_slice(&1u16.to_le_bytes());
    file.extend_from_slice(&1u16.to_le_bytes());
    file.extend_from_slice(&8000u32.to_le_bytes());
    file.extend_from_slice(&32000u32.to_le_bytes());
    file.extend_from_slice(&4u16.to_le_bytes());
    file.extend_from_slice(&32u16.to_le_bytes());
    file.extend_from_slice(b"data");
    file.extend_from_slice(&bytes.to_le_bytes());
    for sample in samples {
        file.extend_from_slice(&sample.to_le_bytes());
    }
    file
}

fn crc8(bytes: &[u8]) -> u8 {
    let mut crc = 0u8;
    for byte in bytes {
        crc ^= byte;
        for _ in 0..8 {
            crc = if crc & 0x80 != 0 {
                (crc << 1) ^ 0x07
            } else {
                crc << 1
            };
        }
    }
    crc
}
fn crc16(bytes: &[u8]) -> u16 {
    let mut crc = 0u16;
    for byte in bytes {
        crc ^= (*byte as u16) << 8;
        for _ in 0..8 {
            crc = if crc & 0x8000 != 0 {
                (crc << 1) ^ 0x8005
            } else {
                crc << 1
            };
        }
    }
    crc
}
