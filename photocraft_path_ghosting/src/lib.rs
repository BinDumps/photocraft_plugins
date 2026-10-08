#![no_std]

#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    core::arch::wasm32::unreachable()
}

const MANIFEST: &str = r#"{
  "id": "org.photocraft.filter.path_ghosting",
  "name": "Path Ghosting (WebAssembly)",
  "version": "1.0.0",
  "kind": "filter",
  "author": "User",
  "description": "Directional motion ghosting and sharp offset silhouettes with path controls.",
  "params": {
    "count": {
      "type": "int",
      "default": 5,
      "min": 1,
      "max": 20,
      "label": "Ghost Count"
    },
    "angle": {
      "type": "number",
      "default": 45.0,
      "min": 0.0,
      "max": 360.0,
      "label": "Angle (Deg)"
    },
    "distance": {
      "type": "number",
      "default": 30.0,
      "min": 1.0,
      "max": 200.0,
      "label": "Step Distance (px)"
    },
    "opacity": {
      "type": "number",
      "default": 0.6,
      "min": 0.0,
      "max": 1.0,
      "label": "Ghost Opacity"
    },
    "decay": {
      "type": "number",
      "default": 0.75,
      "min": 0.1,
      "max": 1.0,
      "label": "Trail Decay"
    },
    "mode": {
      "type": "choice",
      "default": "Screen",
      "options": ["Normal", "Screen", "Additive", "Silhouette"],
      "label": "Blend Mode"
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

// Minimal trigonometric math helpers for WASM target without libm
fn deg_to_rad(deg: f32) -> f32 {
    deg * (3.141592653589793f32 / 180.0f32)
}

fn cos_approx(mut x: f32) -> f32 {
    const TWO_PI: f32 = 6.283185307179586;
    const PI: f32 = 3.141592653589793;
    x = x % TWO_PI;
    if x < 0.0 { x += TWO_PI; }
    if x > PI { x -= TWO_PI; }

    let x2 = x * x;
    1.0 - (x2 / 2.0) + (x2 * x2 / 24.0) - (x2 * x2 * x2 / 720.0)
}

fn sin_approx(x: f32) -> f32 {
    cos_approx(x - (3.141592653589793 / 2.0))
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

fn parse_param_choice(json: &[u8], key: &[u8], default: u32) -> u32 {
    // 0: Normal, 1: Screen, 2: Additive, 3: Silhouette
    if json.is_empty() { return default; }
    
    // Check key presence
    for i in 0..json.len().saturating_sub(key.len()) {
        if &json[i..i + key.len()] == key {
            let slice = &json[i..];
            if contains_bytes(slice, b"Silhouette") { return 3; }
            if contains_bytes(slice, b"Additive") { return 2; }
            if contains_bytes(slice, b"Screen") { return 1; }
            if contains_bytes(slice, b"Normal") { return 0; }
        }
    }
    default
}

fn contains_bytes(haystack: &[u8], needle: &[u8]) -> bool {
    if needle.len() > haystack.len() { return false; }
    for i in 0..haystack.len() - needle.len() {
        if &haystack[i..i + needle.len()] == needle {
            return true;
        }
    }
    false
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

    let count = (parse_param_f32(param_bytes, b"count", 5.0) as usize).clamp(1, 20);
    let angle_deg = parse_param_f32(param_bytes, b"angle", 45.0);
    let step_dist = parse_param_f32(param_bytes, b"distance", 30.0);
    let base_opacity = parse_param_f32(param_bytes, b"opacity", 0.6);
    let decay = parse_param_f32(param_bytes, b"decay", 0.75);
    let blend_mode = parse_param_choice(param_bytes, b"mode", 1); // Default to Screen

    let rad = deg_to_rad(angle_deg);
    let dir_x = cos_approx(rad);
    let dir_y = sin_approx(rad);

    // Backup original pixels as reference for ghost samples
    let bytes_needed = (total_samples * 4) as u32;
    let src_offset = pc_alloc(bytes_needed);
    if src_offset == 0 {
        return 1;
    }

    let src_ptr = src_offset as *mut f32;
    let src = unsafe { core::slice::from_raw_parts_mut(src_ptr, total_samples) };
    src.copy_from_slice(px);

    // Process ghosts starting from the farthest trail back to the foreground
    for g in (1..=count).rev() {
        let step_f = g as f32;
        let offset_x = (dir_x * step_dist * step_f) as i32;
        let offset_y = (dir_y * step_dist * step_f) as i32;

        let mut current_opacity = base_opacity;
        for _ in 1..g {
            current_opacity *= decay;
        }

        for y in 0..h {
            let sx = y as i32 - offset_y;
            if sx < 0 || sx >= h as i32 {
                continue;
            }
            let sample_y = sx as usize;

            for x in 0..w {
                let sy = x as i32 - offset_x;
                if sy < 0 || sy >= w as i32 {
                    continue;
                }
                let sample_x = sy as usize;

                let dst_idx = (y * w + x) * ch;
                let src_idx = (sample_y * w + sample_x) * ch;

                let src_a = if has_alpha { src[src_idx + ch - 1] } else { 1.0 };
                if src_a <= 0.0 {
                    continue;
                }

                let alpha = current_opacity * src_a;

                match blend_mode {
                    // Screen Mode
                    1 => {
                        px[dst_idx] = 1.0 - (1.0 - px[dst_idx]) * (1.0 - src[src_idx] * alpha);
                        px[dst_idx + 1] = 1.0 - (1.0 - px[dst_idx + 1]) * (1.0 - src[src_idx + 1] * alpha);
                        px[dst_idx + 2] = 1.0 - (1.0 - px[dst_idx + 2]) * (1.0 - src[src_idx + 2] * alpha);
                    }
                    // Additive Mode
                    2 => {
                        px[dst_idx] = (px[dst_idx] + src[src_idx] * alpha).min(1.0);
                        px[dst_idx + 1] = (px[dst_idx + 1] + src[src_idx + 1] * alpha).min(1.0);
                        px[dst_idx + 2] = (px[dst_idx + 2] + src[src_idx + 2] * alpha).min(1.0);
                    }
                    // Sharp Silhouette Mode (Dark sharp contrast trail)
                    3 => {
                        let luma = 0.2126 * src[src_idx] + 0.7152 * src[src_idx + 1] + 0.0722 * src[src_idx + 2];
                        let sil_color = if luma > 0.5 { 0.0f32 } else { 1.0f32 };
                        
                        px[dst_idx] = px[dst_idx] * (1.0 - alpha) + sil_color * alpha;
                        px[dst_idx + 1] = px[dst_idx + 1] * (1.0 - alpha) + sil_color * alpha;
                        px[dst_idx + 2] = px[dst_idx + 2] * (1.0 - alpha) + sil_color * alpha;
                    }
                    // Normal Alpha Blend (0)
                    _ => {
                        px[dst_idx] = px[dst_idx] * (1.0 - alpha) + src[src_idx] * alpha;
                        px[dst_idx + 1] = px[dst_idx + 1] * (1.0 - alpha) + src[src_idx + 1] * alpha;
                        px[dst_idx + 2] = px[dst_idx + 2] * (1.0 - alpha) + src[src_idx + 2] * alpha;
                    }
                }
            }
        }
    }

    0
}
