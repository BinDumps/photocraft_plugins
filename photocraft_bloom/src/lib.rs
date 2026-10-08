#![no_std]

#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    core::arch::wasm32::unreachable()
}

// 1. Declare UI parameters in the JSON manifest so the host builds UI sliders
const MANIFEST: &str = r#"{
  "id": "org.photocraft.filter.bloom",
  "name": "Bloom (WebAssembly)",
  "version": "1.0.0",
  "kind": "filter",
  "author": "User",
  "description": "Luminance threshold bloom filter.",
  "params": {
    "threshold": {
      "type": "number",
      "default": 0.75,
      "min": 0.0,
      "max": 1.0,
      "label": "Threshold"
    },
    "radius": {
      "type": "int",
      "default": 6,
      "min": 1,
      "max": 30,
      "label": "Blur Radius"
    },
    "intensity": {
      "type": "number",
      "default": 1.2,
      "min": 0.0,
      "max": 5.0,
      "label": "Intensity"
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

/// Dynamic allocator using WASM intrinsic memory_grow without hardcoded heap offsets
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

/// Helper to parse simple float/int key values out of JSON bytes without pulling in serde
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

    // Extract dynamic parameter values sent by host
    let param_bytes = if !params.is_null() && params_len > 0 {
        unsafe { core::slice::from_raw_parts(params, params_len as usize) }
    } else {
        &[]
    };

    let threshold = parse_param_f32(param_bytes, b"threshold", 0.75);
    let radius = parse_param_f32(param_bytes, b"radius", 6.0) as usize;
    let intensity = parse_param_f32(param_bytes, b"intensity", 1.2);

    // Allocate temp scratch space cleanly via pc_alloc to prevent memory overlap
    let bytes_needed = (total_samples * 4) as u32;
    let h_offset = pc_alloc(bytes_needed);
    let b_offset = pc_alloc(bytes_needed);

    if h_offset == 0 || b_offset == 0 {
        return 1;
    }

    let h_ptr = h_offset as *mut f32;
    let b_ptr = b_offset as *mut f32;

    let highlights = unsafe { core::slice::from_raw_parts_mut(h_ptr, total_samples) };
    let blurred = unsafe { core::slice::from_raw_parts_mut(b_ptr, total_samples) };

    highlights.fill(0.0);
    blurred.fill(0.0);

    // Step 1: Extract bright highlights above threshold
    for p in px.chunks_exact(ch).zip(highlights.chunks_exact_mut(ch)) {
        let (src, dst) = p;
        if has_alpha && src[ch - 1] <= 0.0 {
            continue;
        }

        let luma = 0.2126 * src[0] + 0.7152 * src[1] + 0.0722 * src[2];
        if luma >= threshold {
            dst[0..3].copy_from_slice(&src[0..3]);
        }
        if has_alpha {
            dst[ch - 1] = src[ch - 1];
        }
    }

    // Step 2: Separate 2D Box Blur
    box_blur(highlights, blurred, w, h, ch, radius, has_alpha);

    // Step 3: Additive blend onto original image (preserving original alpha)
    for p in px.chunks_exact_mut(ch).zip(blurred.chunks_exact(ch)) {
        let (src, glow) = p;
        if has_alpha && src[ch - 1] <= 0.0 {
            continue;
        }

        src[0] = (src[0] + glow[0] * intensity).min(1.0);
        src[1] = (src[1] + glow[1] * intensity).min(1.0);
        src[2] = (src[2] + glow[2] * intensity).min(1.0);
    }

    0
}

fn box_blur(src: &[f32], dst: &mut [f32], w: usize, h: usize, ch: usize, r: usize, has_alpha: bool) {
    if r == 0 {
        dst.copy_from_slice(src);
        return;
    }

    for y in 0..h {
        for x in 0..w {
            let mut acc = [0.0f32; 3];
            let mut count = 0.0f32;

            let x_min = x.saturating_sub(r);
            let x_max = (x + r).min(w - 1);

            for ix in x_min..=x_max {
                let idx = (y * w + ix) * ch;
                acc[0] += src[idx];
                acc[1] += src[idx + 1];
                acc[2] += src[idx + 2];
                count += 1.0;
            }

            let idx = (y * w + x) * ch;
            dst[idx] = acc[0] / count;
            dst[idx + 1] = acc[1] / count;
            dst[idx + 2] = acc[2] / count;
            if has_alpha {
                dst[idx + ch - 1] = src[idx + ch - 1];
            }
        }
    }
}
