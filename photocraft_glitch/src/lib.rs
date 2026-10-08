#![no_std]

#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    core::arch::wasm32::unreachable()
}

const MANIFEST: &str = r#"{
  "id": "org.photocraft.filter.glitch",
  "name": "Procedural Glitch (WebAssembly)",
  "version": "1.0.0",
  "kind": "filter",
  "author": "User",
  "description": "Procedural RGB channel shift, block displacement, and scanline corruption.",
  "params": {
    "intensity": {
      "type": "number",
      "default": 0.5,
      "min": 0.0,
      "max": 1.0,
      "label": "Glitch Intensity"
    },
    "channel_shift": {
      "type": "int",
      "default": 12,
      "min": 0,
      "max": 100,
      "label": "RGB Split (px)"
    },
    "block_size": {
      "type": "int",
      "default": 16,
      "min": 4,
      "max": 64,
      "label": "Block Height"
    },
    "seed": {
      "type": "int",
      "default": 42,
      "min": 1,
      "max": 9999,
      "label": "Random Seed"
    }
  }
}"#;

const HAS_ALPHA: u32 = 1 << 8;

#[unsafe(no_mangle)]
pub extern "C" fn pc_abi_version() -> u32 {
    1
}

#[unsafe(no_mangle)]
pub extern "C" fn pc_manifest() -> u64 {
    ((MANIFEST.len() as u64) << 32) | MANIFEST.as_ptr() as u64
}

#[unsafe(no_mangle)]
pub extern "C" fn pc_alloc(size: u32) -> u32 {
    let pages = (size as usize).div_ceil(65536);
    let old = core::arch::wasm32::memory_grow(0, pages);
    if old == usize::MAX {
        0
    } else {
        (old * 65536) as u32
    }
}

// Minimal deterministic PRNG (PCG-XSH-RR variant)
struct Lcg {
    state: u64,
}

impl Lcg {
    fn new(seed: u64) -> Self {
        Self { state: seed.wrapping_add(0xda3e39cb94b95bdb) }
    }

    fn next_u32(&mut self) -> u32 {
        let old = self.state;
        self.state = old.wrapping_mul(6364136223846793005).wrapping_add(1);
        let xorshifted = (((old >> 18) ^ old) >> 27) as u32;
        let rot = (old >> 59) as u32;
        (xorshifted >> rot) | (xorshifted << ((!rot).wrapping_add(1) & 31))
    }

    fn next_f32(&mut self) -> f32 {
        (self.next_u32() as f32) / (u32::MAX as f32)
    }

    fn range_i32(&mut self, min: i32, max: i32) -> i32 {
        if min >= max {
            return min;
        }
        let diff = (max - min + 1) as u32;
        min + (self.next_u32() % diff) as i32
    }
}

fn parse_param_f32(json: &[u8], key: &[u8], default: f32) -> f32 {
    if json.is_empty() || key.is_empty() {
        return default;
    }
    for i in 0..json.len().saturating_sub(key.len()) {
        if &json[i..i + key.len()] == key {
            let mut start = i + key.len();
            while start < json.len() && (json[start] == b':' || json[start] == b' ' || json[start] == b'"') {
                start += 1;
            }
            let mut end = start;
            while end < json.len() && ((json[end] >= b'0' && json[end] <= b'9') || json[end] == b'.') {
                end += 1;
            }
            if end > start {
                return parse_f32_slice(&json[start..end]).unwrap_or(default);
            }
        }
    }
    default
}

fn parse_f32_slice(bytes: &[u8]) -> Option<f32> {
    let mut val = 0.0f32;
    let mut decimal = false;
    let mut div = 10.0f32;

    for &b in bytes {
        if b == b'.' {
            decimal = true;
            continue;
        }
        if b < b'0' || b > b'9' {
            break;
        }
        let digit = (b - b'0') as f32;
        if !decimal {
            val = val * 10.0 + digit;
        } else {
            val += digit / div;
            div *= 10.0;
        }
    }
    Some(val)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn pc_filter(
    buf: *mut f32,
    len: u32,
    width: u32,
    height: u32,
    channels: u32,
    format: u32,
    params: *const u8,
    params_len: u32,
) -> i32 {
    let w = width as usize;
    let h = height as usize;
    let ch = channels as usize;

    if ch < 3 || w == 0 || h == 0 || buf.is_null() {
        return 1;
    }

    let total_samples = len as usize / 4;
    let px = unsafe { core::slice::from_raw_parts_mut(buf, total_samples) };
    let has_alpha = format & HAS_ALPHA != 0;

    let param_bytes = if !params.is_null() && params_len > 0 {
        unsafe { core::slice::from_raw_parts(params, params_len as usize) }
    } else {
        &[]
    };

    let intensity = parse_param_f32(param_bytes, b"intensity", 0.5);
    let shift_px = parse_param_f32(param_bytes, b"channel_shift", 12.0) as i32;
    let block_h = (parse_param_f32(param_bytes, b"block_size", 16.0) as usize).max(2);
    let seed_val = parse_param_f32(param_bytes, b"seed", 42.0) as u64;

    // Allocate backup buffer to read original pixels during block offsets & chromatic shifts
    let bytes_needed = (total_samples * 4) as u32;
    let src_offset = pc_alloc(bytes_needed);
    if src_offset == 0 {
        return 1;
    }

    let src_ptr = src_offset as *mut f32;
    let src = unsafe { core::slice::from_raw_parts_mut(src_ptr, total_samples) };
    src.copy_from_slice(px);

    let mut rng = Lcg::new(seed_val);
    let num_blocks = h / block_h;

    // Step 1: Horizontal Block Displacement & Scanlines
    for b in 0..=num_blocks {
        let y_start = b * block_h;
        if y_start >= h {
            break;
        }
        let y_end = (y_start + block_h).min(h);

        // Determine if this row/block gets displaced
        let is_corrupted = rng.next_f32() < (intensity * 0.65);
        let offset_x = if is_corrupted {
            let max_shift = ((w as f32) * intensity * 0.25) as i32;
            rng.range_i32(-max_shift, max_shift)
        } else {
            0
        };

        // Determine scanline brightness noise for corrupted blocks
        let scanline_darkening = if is_corrupted && rng.next_f32() < 0.4 {
            0.7f32 + (rng.next_f32() * 0.3)
        } else {
            1.0f32
        };

        for y in y_start..y_end {
            for x in 0..w {
                // Wrap horizontal coordinates safely
                let src_x = (x as i32 - offset_x).rem_euclid(w as i32) as usize;
                let dst_idx = (y * w + x) * ch;
                let src_idx = (y * w + src_x) * ch;

                px[dst_idx..dst_idx + ch].copy_from_slice(&src[src_idx..src_idx + ch]);

                if scanline_darkening < 1.0 {
                    px[dst_idx] *= scanline_darkening;
                    px[dst_idx + 1] *= scanline_darkening;
                    px[dst_idx + 2] *= scanline_darkening;
                }
            }
        }
    }

    // Step 2: Chromatic Aberration (RGB Channel Splitting)
    if shift_px > 0 {
        // Update snapshot after block shifts so channels shift relative to displaced blocks
        src.copy_from_slice(px);

        let r_shift = (shift_px as f32 * intensity) as i32;
        let b_shift = -r_shift;

        for y in 0..h {
            for x in 0..w {
                let dst_idx = (y * w + x) * ch;

                let rx = (x as i32 + r_shift).clamp(0, w as i32 - 1) as usize;
                let bx = (x as i32 + b_shift).clamp(0, w as i32 - 1) as usize;

                let r_src_idx = (y * w + rx) * ch;
                let b_src_idx = (y * w + bx) * ch;

                // Shift Red and Blue channels independently while keeping Green centered
                px[dst_idx] = src[r_src_idx];
                px[dst_idx + 2] = src[b_src_idx + 2];
            }
        }
    }

    // Step 3: Digital Noise / Color Inversion Artifacts
    if intensity > 0.2 {
        let noise_chance = (intensity - 0.2) * 0.05;
        for y in 0..h {
            if rng.next_f32() < noise_chance {
                let x_start = rng.range_i32(0, (w / 2) as i32) as usize;
                let x_len = rng.range_i32(10, (w / 3) as i32) as usize;
                let x_end = (x_start + x_len).min(w);

                for x in x_start..x_end {
                    let idx = (y * w + x) * ch;
                    if has_alpha && px[idx + ch - 1] <= 0.0 {
                        continue;
                    }

                    // Invert RGB channels for high-tech digital artifacts
                    px[idx] = 1.0 - px[idx];
                    px[idx + 1] = 1.0 - px[idx + 1];
                    px[idx + 2] = 1.0 - px[idx + 2];
                }
            }
        }
    }

    0
}
