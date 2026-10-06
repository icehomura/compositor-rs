//! Content-aware fill, ported from `references/Compositor/Compositor/Rendering/ContentFill.c`.

/// Advances a linear congruential state.
fn next_random(state: &mut u32) -> u32 {
    *state = state.wrapping_mul(1664525).wrapping_add(1013904223);
    *state
}

/// Mean squared difference between the patch at `p` and the patch at `q`, over the pixels of `p`'s
/// neighborhood that are `known`. Returns [`f64::MAX`] when no neighbor qualifies.
fn patch_match(
    pixels: &[u8],
    stride: usize,
    known: &[u8],
    w: i32,
    h: i32,
    p: i32,
    q: i32,
    radius: i32,
) -> f64 {
    let (px, py, qx, qy) = (p % w, p / w, q % w, q / w);
    let mut count = 0i32;
    let mut sum = 0.0f64;
    for dy in -radius..=radius {
        for dx in -radius..=radius {
            let x = px + dx;
            let y = py + dy;
            let sx = qx + dx;
            let sy = qy + dy;
            if x < 0
                || y < 0
                || x >= w
                || y >= h
                || sx < 0
                || sy < 0
                || sx >= w
                || sy >= h
                || known[(y * w + x) as usize] == 0
            {
                continue;
            }
            let a = y as usize * stride + x as usize * 4;
            let b = sy as usize * stride + sx as usize * 4;
            for c in 0..4 {
                let d = pixels[a + c] as i32 - pixels[b + c] as i32;
                sum += (d * d) as f64;
            }
            count += 1;
        }
    }
    if count != 0 { sum / count as f64 } else { f64::MAX }
}

/// Returns 1 on success, 0 when no source patch exists, -1 on allocation failure.
pub fn content_fill(
    pixels: &mut [u8],
    stride: usize,
    mask: &[u8],
    mask_stride: usize,
    width: i32,
    height: i32,
) -> i32 {
    let n = width as usize * height as usize;
    let mut known = vec![0u8; n];
    let mut target = vec![0u8; n];
    let mut valid = vec![0u8; n];
    let mut queued = vec![0u8; n];
    let mut donors = vec![0i32; n];
    let mut queue = vec![0i32; n];
    let mut chosen = vec![0i32; n];

    let radius = if width >= 5 && height >= 5 { 2 } else { 0 };
    let mut missing: usize = 0;
    let mut donor_count: usize = 0;
    let mut head: usize = 0;
    let mut tail: usize = 0;
    let mut scan: usize = 0;
    // Selected pixels are filled. Unselected opaque pixels are the image to match and copy from; unselected
    // transparent ones are neither — nothing to match against, and left as they are.
    for y in 0..height {
        for x in 0..width {
            let p = (y * width + x) as usize;
            target[p] = (mask[y as usize * mask_stride + x as usize] != 0) as u8;
            known[p] = (target[p] == 0 && pixels[y as usize * stride + x as usize * 4 + 3] == 255) as u8;
            chosen[p] = -1;
            if target[p] != 0 {
                missing += 1;
            }
        }
    }
    if missing == 0 {
        // donorCount stays 1, so the result is success.
        return 1;
    }
    for y in 0..height {
        for x in 0..width {
            let p = (y * width + x) as usize;
            if known[p] == 0 {
                continue;
            }
            let mut ok = true;
            let mut dy = -radius;
            while dy <= radius && ok {
                let mut dx = -radius;
                while dx <= radius {
                    let sx = x + dx;
                    let sy = y + dy;
                    if sx < 0
                        || sy < 0
                        || sx >= width
                        || sy >= height
                        || known[(sy * width + sx) as usize] == 0
                    {
                        ok = false;
                        break;
                    }
                    dx += 1;
                }
                dy += 1;
            }
            if ok {
                valid[p] = 1;
                donors[donor_count] = p as i32;
                donor_count += 1;
            }
        }
    }
    if donor_count == 0 {
        return 0;
    }
    for y in 0..height {
        for x in 0..width {
            let pi = y * width + x;
            let p = pi as usize;
            if target[p] != 0
                && ((x != 0 && known[(pi - 1) as usize] != 0)
                    || (x + 1 < width && known[(pi + 1) as usize] != 0)
                    || (y != 0 && known[(pi - width) as usize] != 0)
                    || (y + 1 < height && known[(pi + width) as usize] != 0))
            {
                queue[tail] = p as i32;
                tail += 1;
                queued[p] = 1;
            }
        }
    }
    let mut seed: u32 = 0x6d2b79f5;
    loop {
        while head < tail {
            let p = queue[head] as usize;
            head += 1;
            let x = (p % width as usize) as i32;
            let y = (p / width as usize) as i32;
            let mut best: i32 = -1;
            let mut score = f64::MAX;
            let neighbors = [
                if x != 0 { p as i32 - 1 } else { -1 },
                if x + 1 < width { p as i32 + 1 } else { -1 },
                if y != 0 { p as i32 - width } else { -1 },
                if y + 1 < height { p as i32 + width } else { -1 },
            ];
            // Propagate coherent source offsets, then refine with randomized patch search.
            for k in 0..28 {
                let q;
                if k < 4 {
                    let t = neighbors[k as usize];
                    if t < 0 {
                        continue;
                    }
                    let base = if chosen[t as usize] >= 0 { chosen[t as usize] } else { t };
                    q = base + (p as i32 - t);
                } else {
                    q = donors[next_random(&mut seed) as usize % donor_count];
                }
                if q < 0 || q as usize >= n || valid[q as usize] == 0 {
                    continue;
                }
                let s = patch_match(pixels, stride, &known, width, height, p as i32, q, radius);
                if best < 0 || s < score {
                    score = s;
                    best = q;
                }
            }
            if best < 0 {
                best = donors[0];
            }
            let mut r = 64i32;
            while r >= 1 {
                let qx = (best % width) + (next_random(&mut seed) % (2 * r + 1) as u32) as i32 - r;
                let qy = (best / width) + (next_random(&mut seed) % (2 * r + 1) as u32) as i32 - r;
                if qx < 0
                    || qy < 0
                    || qx >= width
                    || qy >= height
                    || valid[(qy * width + qx) as usize] == 0
                {
                    r /= 2;
                    continue;
                }
                let q = qy * width + qx;
                let s = patch_match(pixels, stride, &known, width, height, p as i32, q, radius);
                if s < score {
                    score = s;
                    best = q;
                }
                r /= 2;
            }
            let destination = y as usize * stride + x as usize * 4;
            let source = (best / width) as usize * stride + (best % width) as usize * 4;
            pixels.copy_within(source..source + 4, destination);
            known[p] = 1;
            chosen[p] = best;
            for t in neighbors {
                if t >= 0
                    && target[t as usize] != 0
                    && known[t as usize] == 0
                    && queued[t as usize] == 0
                {
                    queued[t as usize] = 1;
                    queue[tail] = t;
                    tail += 1;
                }
            }
        }
        // A selected area that only transparency touches starts from the best random donor, then spreads.
        while scan < n && (target[scan] == 0 || known[scan] != 0) {
            scan += 1;
        }
        if scan >= n {
            break;
        }
        queue[tail] = scan as i32;
        tail += 1;
        queued[scan] = 1;
    }
    if donor_count != 0 { 1 } else { 0 }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn uniform(width: usize, height: usize, pixel: [u8; 4]) -> Vec<u8> {
        let mut rgba = vec![0u8; width * height * 4];
        for p in rgba.chunks_exact_mut(4) {
            p.copy_from_slice(&pixel);
        }
        rgba
    }

    #[test]
    fn content_fill_empty_mask_reports_success_and_leaves_pixels() {
        let (width, height) = (5usize, 5usize);
        let original = uniform(width, height, [90, 140, 200, 255]);
        let mut rgba = original.clone();
        let mask = vec![0u8; width * height];
        assert_eq!(
            content_fill(&mut rgba, width * 4, &mask, width, width as i32, height as i32),
            1
        );
        assert_eq!(rgba, original);
    }

    #[test]
    fn content_fill_uniform_image_keeps_hole_color() {
        let (width, height) = (13usize, 13usize);
        let original = uniform(width, height, [90, 140, 200, 255]);
        let mut rgba = original.clone();
        let mut mask = vec![0u8; width * height];
        mask[6 * width + 6] = 255;
        assert_eq!(
            content_fill(&mut rgba, width * 4, &mask, width, width as i32, height as i32),
            1
        );
        assert_eq!(rgba, original);
    }

    #[test]
    fn content_fill_without_a_donor_reports_no_source_and_leaves_pixels() {
        // Everything is selected, so there is no opaque unselected donor.
        let (width, height) = (6usize, 6usize);
        let original = uniform(width, height, [10, 20, 30, 255]);
        let mut rgba = original.clone();
        let mask = vec![255u8; width * height];
        assert_eq!(
            content_fill(&mut rgba, width * 4, &mask, width, width as i32, height as i32),
            0
        );
        assert_eq!(rgba, original);
    }

    #[test]
    fn content_fill_ignores_transparent_pixels_as_donors() {
        // Left half opaque red, right half transparent; the selected pixel sits in the transparent half.
        let (width, height) = (13usize, 13usize);
        let mut rgba = vec![0u8; width * height * 4];
        for y in 0..height {
            for x in 0..width {
                if x < 6 {
                    let i = y * width * 4 + x * 4;
                    rgba[i..i + 4].copy_from_slice(&[255, 0, 0, 255]);
                }
            }
        }
        let mut mask = vec![0u8; width * height];
        mask[6 * width + 10] = 255;
        assert_eq!(
            content_fill(&mut rgba, width * 4, &mask, width, width as i32, height as i32),
            1
        );
        let filled = 6 * width * 4 + 10 * 4;
        assert_eq!(&rgba[filled..filled + 4], &[255, 0, 0, 255]);
    }

    #[test]
    fn content_fill_is_deterministic() {
        let (width, height) = (12usize, 11usize);
        let mut first = vec![0u8; width * height * 4];
        for (i, b) in first.iter_mut().enumerate() {
            *b = (i * 37 % 256) as u8;
        }
        for y in 0..height {
            for x in 0..width {
                first[y * width * 4 + x * 4 + 3] = 255;
            }
        }
        let mut second = first.clone();
        let mut mask = vec![0u8; width * height];
        for y in 4..7 {
            for x in 5..8 {
                mask[y * width + x] = 255;
            }
        }
        assert_eq!(
            content_fill(&mut first, width * 4, &mask, width, width as i32, height as i32),
            1
        );
        assert_eq!(
            content_fill(&mut second, width * 4, &mask, width, width as i32, height as i32),
            1
        );
        assert_eq!(first, second);
    }
}
