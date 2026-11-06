//! RIFF chunks, WAV files (16-bit with a `smpl` chunk, 32-bit float).

use std::path::Path;

use anyhow::{Context, Result};

pub fn chunk(id: &[u8; 4], data: &[u8]) -> Vec<u8> {
    let mut v = Vec::with_capacity(data.len() + 9);
    v.extend_from_slice(id);
    v.extend_from_slice(&(data.len() as u32).to_le_bytes());
    v.extend_from_slice(data);
    if data.len() % 2 == 1 {
        v.push(0);
    }
    v
}

pub fn list(kind: &[u8; 4], parts: &[Vec<u8>]) -> Vec<u8> {
    let mut d = kind.to_vec();
    for p in parts {
        d.extend_from_slice(p);
    }
    chunk(b"LIST", &d)
}

/// A zero-terminated string padded to an even length.
pub fn zstr(s: &str) -> Vec<u8> {
    let mut v = s.as_bytes().to_vec();
    v.push(0);
    if v.len() % 2 == 1 {
        v.push(0);
    }
    v
}

pub fn write_riff(path: &Path, kind: &[u8; 4], parts: &[Vec<u8>]) -> Result<()> {
    let mut d = kind.to_vec();
    for p in parts {
        d.extend_from_slice(p);
    }
    std::fs::write(path, chunk(b"RIFF", &d)).with_context(|| path.display().to_string())
}

fn fmt(format: u16, channels: u16, rate: u32, bits: u16) -> Vec<u8> {
    let mut f = Vec::new();
    let align = channels * bits / 8;
    f.extend_from_slice(&format.to_le_bytes());
    f.extend_from_slice(&channels.to_le_bytes());
    f.extend_from_slice(&rate.to_le_bytes());
    f.extend_from_slice(&(rate * u32::from(align)).to_le_bytes());
    f.extend_from_slice(&align.to_le_bytes());
    f.extend_from_slice(&bits.to_le_bytes());
    chunk(b"fmt ", &f)
}

/// A 16-bit WAV (interleaved `pcm`); `smpl`: the root key and a forward
/// loop (start, end exclusive, in frames).
pub fn write_wav_i16(path: &Path, rate: u32, channels: u16, pcm: &[i16], smpl: Option<(u8, Option<(usize, usize)>)>) -> Result<()> {
    let mut parts = vec![fmt(1, channels, rate, 16)];
    if let Some((root, lp)) = smpl {
        let mut s = Vec::new();
        let period = (1e9 / f64::from(rate)).round() as u32;
        for v in [0u32, 0, period, u32::from(root), 0, 0, 0, u32::from(lp.is_some()), 0] {
            s.extend_from_slice(&v.to_le_bytes());
        }
        if let Some((a, b)) = lp {
            // the smpl loop end is the last sample played (inclusive)
            for v in [0u32, 0, a as u32, (b - 1) as u32, 0, 0] {
                s.extend_from_slice(&v.to_le_bytes());
            }
        }
        parts.push(chunk(b"smpl", &s));
    }
    let mut d = Vec::with_capacity(pcm.len() * 2);
    for v in pcm {
        d.extend_from_slice(&v.to_le_bytes());
    }
    parts.push(chunk(b"data", &d));
    write_riff(path, b"WAVE", &parts)
}

/// A 32-bit float stereo WAV.
pub fn write_wav_f32(path: &Path, rate: u32, frames: &[[f32; 2]]) -> Result<()> {
    let mut d = Vec::with_capacity(frames.len() * 8);
    for f in frames {
        d.extend_from_slice(&f[0].to_le_bytes());
        d.extend_from_slice(&f[1].to_le_bytes());
    }
    write_riff(path, b"WAVE", &[fmt(3, 2, rate, 32), chunk(b"data", &d)])
}
