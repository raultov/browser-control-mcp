//! Lighthouse's scoring, ported from its source (`shared/statistics.js`,
//! `core/scoring.js` and the default config, Lighthouse 13): a metric scores
//! on a log-normal curve fixed by the value that scores 0.9 (`p10`) and the
//! one that scores 0.5 (`median`), boosted a little above 0.9 and floored
//! to 2 decimals; a pass/fail check scores 1 or 0; and a category is the
//! weighted mean of the scores of its checks that applied, out of 100.

use super::Device;

/// A metric's log-normal curve: the value that scores 0.9 (`p10`) and the
/// one that scores 0.5 (`median`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Curve {
    /// The value that scores 0.9.
    pub(crate) p10: f64,
    /// The value that scores 0.5.
    pub(crate) median: f64,
}

/// Lighthouse's approximation of the error function (Abramowitz and
/// Stegun 7.1.26), kept as is so the scores match its own.
fn erf(x: f64) -> f64 {
    let sign = x.signum();
    let x = x.abs();
    let (a1, a2, a3, a4, a5) = (
        0.254_829_592,
        -0.284_496_736,
        1.421_413_741,
        -1.453_152_027,
        1.061_405_429,
    );
    let t = 1.0 / (1.0 + 0.327_591_1 * x);
    let y = t * (a1 + t * (a2 + t * (a3 + t * (a4 + t * a5))));
    sign * (1.0 - y * (-x * x).exp())
}

/// Lighthouse's `getLogNormalScore`: 1 at 0, 0.9 at `p10`, 0.5 at the
/// median, towards 0 beyond; clamped to the band the value falls in, so a
/// value under `p10` always passes and one over the median always fails.
pub(crate) fn log_normal_score(curve: Curve, value: f64) -> f64 {
    if value <= 0.0 {
        return 1.0;
    }
    const INVERSE_ERFC_ONE_FIFTH: f64 = 0.906_193_802_436_823_2;
    let x_log_ratio = (value / curve.median).max(f64::MIN_POSITIVE).ln();
    let p10_log_ratio = -(curve.p10 / curve.median).max(f64::MIN_POSITIVE).ln();
    let standardized = x_log_ratio * INVERSE_ERFC_ONE_FIFTH / p10_log_ratio;
    let complementary = (1.0 - erf(standardized)) / 2.0;
    if value <= curve.p10 {
        complementary.clamp(0.9, 1.0)
    } else if value <= curve.median {
        complementary.clamp(0.5, 0.899_999_999_999_999_9)
    } else {
        complementary.clamp(0.0, 0.499_999_999_999_999_9)
    }
}

/// A metric's score as Lighthouse gives it (`Util.computeLogNormalScore`):
/// the curve's, raised by up to 0.005 above 0.9 (so more top scores reach
/// 1), then floored to 2 decimals.
pub(crate) fn metric_score(curve: Curve, value: f64) -> f64 {
    let mut score = log_normal_score(curve, value);
    if score > 0.9 {
        score += 0.05 * (score - 0.9);
    }
    (score * 100.0).floor() / 100.0
}

/// A category's score out of 100 from its `(weight, score)` pairs (the
/// checks that applied): `None` when nothing weighs.
pub(crate) fn category_score(scored: &[(f64, f64)]) -> Option<u32> {
    let total: f64 = scored.iter().map(|(weight, _)| weight).sum();
    if total <= 0.0 {
        return None;
    }
    let mean = scored.iter().map(|(w, s)| w * s).sum::<f64>() / total;
    // In 0..=100 by construction: the cast cannot truncate.
    Some((mean * 100.0).round().clamp(0.0, 100.0) as u32)
}

/// A performance metric Lighthouse scores.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Metric {
    /// First Contentful Paint (ms).
    FirstContentfulPaint,
    /// Speed Index (ms).
    SpeedIndex,
    /// Largest Contentful Paint (ms).
    LargestContentfulPaint,
    /// Total Blocking Time (ms).
    TotalBlockingTime,
    /// Cumulative Layout Shift (unitless).
    CumulativeLayoutShift,
}

impl Metric {
    /// Every metric, in Lighthouse's report order.
    pub(crate) const ALL: [Self; 5] = [
        Self::FirstContentfulPaint,
        Self::LargestContentfulPaint,
        Self::TotalBlockingTime,
        Self::CumulativeLayoutShift,
        Self::SpeedIndex,
    ];

    /// Its key in the reply.
    pub(crate) fn key(self) -> &'static str {
        match self {
            Self::FirstContentfulPaint => "first_contentful_paint_ms",
            Self::SpeedIndex => "speed_index_ms",
            Self::LargestContentfulPaint => "largest_contentful_paint_ms",
            Self::TotalBlockingTime => "total_blocking_time_ms",
            Self::CumulativeLayoutShift => "cumulative_layout_shift",
        }
    }

    /// Its weight in the performance score (Lighthouse 10 and later).
    pub(crate) fn weight(self) -> f64 {
        match self {
            Self::FirstContentfulPaint | Self::SpeedIndex => 10.0,
            Self::LargestContentfulPaint | Self::CumulativeLayoutShift => 25.0,
            Self::TotalBlockingTime => 30.0,
        }
    }

    /// Its curve on `device` (Lighthouse's mobile and desktop options).
    pub(crate) fn curve(self, device: Device) -> Curve {
        let (p10, median) = match (self, device) {
            (Self::FirstContentfulPaint, Device::Mobile) => (1800.0, 3000.0),
            (Self::FirstContentfulPaint, Device::Desktop) => (934.0, 1600.0),
            (Self::SpeedIndex, Device::Mobile) => (3387.0, 5800.0),
            (Self::SpeedIndex, Device::Desktop) => (1311.0, 2300.0),
            (Self::LargestContentfulPaint, Device::Mobile) => (2500.0, 4000.0),
            (Self::LargestContentfulPaint, Device::Desktop) => (1200.0, 2400.0),
            (Self::TotalBlockingTime, Device::Mobile) => (200.0, 600.0),
            (Self::TotalBlockingTime, Device::Desktop) => (150.0, 350.0),
            (Self::CumulativeLayoutShift, _) => (0.1, 0.25),
        };
        Curve { p10, median }
    }
}

/// The weight of each check in each category: Lighthouse's for the audit
/// the check stands for (`html-lang` is its `html-has-lang`,
/// `aria-references` its `aria-valid-attr-value`), 1 for a check it has no
/// scored audit for (`viewport`, `mixed-content`), and 3 for `duplicate-id`:
/// it flags any duplicate id, while Lighthouse scores only duplicate ARIA
/// ids (at 10), so a page with harmless duplicates loses less. The SEO
/// `document-title` check also counts in accessibility, and the
/// accessibility `image-alt` in SEO, as in Lighthouse.
pub(crate) const CHECK_WEIGHTS: &[(&str, &str, f64)] = &[
    ("accessibility", "image-alt", 10.0),
    ("accessibility", "label", 10.0),
    ("accessibility", "button-name", 10.0),
    ("accessibility", "aria-references", 10.0),
    ("accessibility", "link-name", 7.0),
    ("accessibility", "html-lang", 7.0),
    ("accessibility", "document-title", 7.0),
    ("accessibility", "color-contrast", 7.0),
    ("accessibility", "tabindex", 7.0),
    ("accessibility", "target-size", 7.0),
    ("accessibility", "duplicate-id", 3.0),
    ("accessibility", "heading-order", 3.0),
    ("seo", "is-crawlable", 93.0 / 23.0),
    ("seo", "document-title", 1.0),
    ("seo", "meta-description", 1.0),
    ("seo", "http-status-code", 1.0),
    ("seo", "link-text", 1.0),
    ("seo", "crawlable-anchors", 1.0),
    ("seo", "image-alt", 1.0),
    ("seo", "canonical", 1.0),
    ("seo", "viewport", 1.0),
    ("best_practices", "is-on-https", 5.0),
    ("best_practices", "mixed-content", 1.0),
    ("best_practices", "image-aspect-ratio", 1.0),
    ("best_practices", "doctype", 1.0),
    ("best_practices", "errors-in-console", 1.0),
];

#[cfg(test)]
mod tests {
    use super::*;

    /// LH-SCORE-01: the curve gives 0.9 at p10 and 0.5 at the median, 1 at
    /// 0, and the scores Lighthouse's formula gives.
    #[test]
    fn log_normal_scores_match_lighthouse() {
        // Given the mobile LCP curve
        let lcp = Metric::LargestContentfulPaint.curve(Device::Mobile);
        // When values are scored
        // Then the anchors hold and the bands clamp
        assert_eq!(metric_score(lcp, 2500.0), 0.9);
        assert_eq!(metric_score(lcp, 4000.0), 0.5);
        assert_eq!(log_normal_score(lcp, 0.0), 1.0);
        assert!(log_normal_score(lcp, 4000.1) < 0.5);
        assert!(log_normal_score(lcp, 2500.1) < 0.9);
        assert_eq!(metric_score(lcp, 1000.0), 1.0);
        assert_eq!(metric_score(lcp, 10_000.0), 0.0);
        // Floored, with the boost: 0.95 where rounding gave 0.96
        assert_eq!(metric_score(lcp, 2137.4), 0.95);
        // FCP 2.4 s on mobile: between p10 and the median
        let fcp = Metric::FirstContentfulPaint.curve(Device::Mobile);
        assert_eq!(metric_score(fcp, 2400.0), 0.71);
        // CLS 0.05 passes
        let cls = Metric::CumulativeLayoutShift.curve(Device::Desktop);
        assert_eq!(metric_score(cls, 0.05), 0.99);
        let tbt = Metric::TotalBlockingTime.curve(Device::Mobile);
        assert_eq!(metric_score(tbt, 201.5), 0.89);
    }

    /// LH-SCORE-02: the performance weights add up to 100 and a category is
    /// the weighted mean out of 100; nothing weighing has no score.
    #[test]
    fn categories_are_weighted_means() {
        let total: f64 = Metric::ALL.iter().map(|m| m.weight()).sum();
        assert_eq!(total, 100.0);
        assert_eq!(category_score(&[(10.0, 1.0), (30.0, 0.5)]), Some(63));
        assert_eq!(category_score(&[(1.0, 0.0)]), Some(0));
        assert_eq!(category_score(&[]), None);
    }

    /// LH-SCORE-03: every `audit_page` check weighs in its own category.
    #[test]
    fn every_check_has_a_weight() {
        let ids = crate::handler::audit::CHECK_IDS;
        for &(category, id) in &ids {
            let found = CHECK_WEIGHTS
                .iter()
                .any(|(c, i, _)| *c == category && *i == id);
            assert!(found, "{category}/{id}");
        }
    }
}
