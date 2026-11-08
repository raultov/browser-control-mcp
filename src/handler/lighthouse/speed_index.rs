//! Speed Index from the screenshots of a trace, as Lighthouse computes it
//! with speedline: each frame's visual progress compares its colour
//! histograms with the first frame's and the last one's, and the Speed
//! Index adds up, frame after frame, the time the page spent still
//! incomplete (`Σ elapsed × (1 − progress)`). The first frame is the blank
//! page at the start of the navigation (Lighthouse's first screenshot is
//! `about:blank`'s, white).

/// One colour histogram per channel (red, green, blue).
pub(crate) type Histogram = [[u32; 256]; 3];

/// Pixels this light in every channel are background, left out of the
/// histograms (speedline's white threshold), so a blank page is an empty
/// histogram and progress measures what was painted.
const WHITE: u8 = 249;

/// The histograms of a JPEG screenshot (`None` when it does not decode).
pub(crate) fn histogram(jpeg: &[u8]) -> Option<Box<Histogram>> {
    let mut decoder = zune_jpeg::JpegDecoder::new(std::io::Cursor::new(jpeg));
    let pixels = decoder.decode().ok()?;
    let (width, height) = decoder.dimensions()?;
    let channels = pixels.len().checked_div(width * height)?;
    if channels < 3 {
        return None;
    }
    let mut histogram = Box::new([[0u32; 256]; 3]);
    for pixel in pixels.chunks_exact(channels) {
        let rgb = [pixel[0], pixel[1], pixel[2]];
        if rgb.iter().all(|&c| c >= WHITE) {
            continue;
        }
        for (channel, value) in rgb.into_iter().enumerate() {
            histogram[channel][usize::from(value)] += 1;
        }
    }
    Some(histogram)
}

/// How far `current` got from `initial` to `target`, 0 to 100 (speedline's
/// `calculateFrameProgress`, floored): 100 when all three are the same.
fn progress(current: &Histogram, initial: &Histogram, target: &Histogram) -> f64 {
    let (mut matched, mut total) = (0u64, 0u64);
    for channel in 0..3 {
        for value in 0..256 {
            let start = initial[channel][value];
            let moved = current[channel][value].abs_diff(start);
            let needed = target[channel][value].abs_diff(start);
            matched += u64::from(moved.min(needed));
            total += u64::from(needed);
        }
    }
    if total == 0 {
        return 100.0;
    }
    // Lossless enough for pixel counts (under 2^53).
    (matched as f64 / total as f64 * 100.0).floor()
}

/// The Speed Index (ms) of `frames`, `(ms since the navigation started,
/// histograms)` in time order, starting from a blank page at 0: `None`
/// without frames.
pub(crate) fn speed_index(frames: &[(f64, Box<Histogram>)]) -> Option<f64> {
    let (_, target) = frames.last()?;
    let blank: Histogram = [[0; 256]; 3];
    let (mut index, mut last_at, mut last_progress) = (0.0, 0.0, 0.0);
    for (at, histogram) in frames {
        index += (at - last_at).max(0.0) * (1.0 - last_progress / 100.0);
        last_at = *at;
        last_progress = progress(histogram, &blank, target);
    }
    Some(index)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A histogram with `pixels` pixels of one dark colour.
    fn painted(pixels: u32) -> Box<Histogram> {
        let mut histogram = Box::new([[0; 256]; 3]);
        for channel in histogram.iter_mut() {
            channel[20] = pixels;
        }
        histogram
    }

    /// LH-SI-01: progress is the share of the final histogram painted, and
    /// the Speed Index the time weighted by what was still missing.
    #[test]
    fn speed_index_weighs_the_time_left_incomplete() {
        // Given a blank start, half the page at 1 s and all of it at 2 s
        let frames = vec![(1000.0, painted(50)), (2000.0, painted(100))];
        // When the Speed Index is computed
        let index = speed_index(&frames).unwrap();
        // Then the first second counts whole and the second half of it
        assert_eq!(index, 1500.0);
        assert_eq!(progress(&painted(50), &[[0; 256]; 3], &painted(100)), 50.0);
        assert_eq!(speed_index(&[]), None);
        // A page that never changed from blank is complete at its first frame
        let blank = vec![(300.0, painted(0)), (900.0, painted(0))];
        assert_eq!(speed_index(&blank), Some(300.0));
    }

    /// LH-SI-02: a real screenshot of Chrome's trace decodes, and its white
    /// background is left out.
    #[test]
    fn trace_screenshots_decode() {
        use base64::Engine;
        // Given the screenshots of a real mobile load
        let trace: serde_json::Value = serde_json::from_str(include_str!(
            "../../../tests/fixtures/cdp/lighthouse-trace-mobile.json"
        ))
        .unwrap();
        let shots: Vec<&str> = trace["traceEvents"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|e| e["name"] == "Screenshot")
            .filter_map(|e| e["args"]["snapshot"].as_str())
            .collect();
        // When the first and last are decoded
        let decode = |b64: &str| {
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(b64)
                .unwrap();
            histogram(&bytes).unwrap()
        };
        let (first, last) = (decode(shots[0]), decode(shots[shots.len() - 1]));
        // Then the blank first one has (almost) nothing, the last one the page
        let count = |h: &Histogram| h[0].iter().map(|&n| u64::from(n)).sum::<u64>();
        assert!(
            count(&first) < count(&last) / 10,
            "{} {}",
            count(&first),
            count(&last)
        );
        assert_eq!(histogram(b"not a jpeg"), None);
    }
}
