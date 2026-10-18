//! The Loom mark behind the run status line: its icon geometry and its weave.

use crate::view::mark::{LOOM_MARK_SIZE, MarkBar, loom_mark_bars, mark_breath, weft_rise};

/// The mark is the app icon scaled down, so the indicator, the launcher icon,
/// and the browser icon stay the same shape.
#[test]
fn mark_bars_are_the_app_icon_geometry() {
    let [warp, weft] = loom_mark_bars(512.);
    assert_eq!(
        warp,
        MarkBar {
            left: 150.,
            top: 128.,
            width: 88.,
            height: 252.,
            corner: 34.,
        }
    );
    assert_eq!(
        weft,
        MarkBar {
            left: 150.,
            top: 296.,
            width: 220.,
            height: 88.,
            corner: 34.,
        }
    );
}

/// Every bar scales with the square and stays inside it, so the mark never
/// paints outside the status line's height.
#[test]
fn mark_bars_scale_into_the_status_line() {
    for size in [10., LOOM_MARK_SIZE, 24.] {
        let [warp, weft] = loom_mark_bars(size);
        let icon = loom_mark_bars(512.);
        for (bar, icon) in [(warp, icon[0]), (weft, icon[1])] {
            let scale = size / 512.;
            assert!((bar.left - icon.left * scale).abs() < 1e-4);
            assert!((bar.top - icon.top * scale).abs() < 1e-4);
            assert!((bar.width - icon.width * scale).abs() < 1e-4);
            assert!((bar.height - icon.height * scale).abs() < 1e-4);
            assert!((bar.corner - icon.corner * scale).abs() < 1e-4);
            assert!(bar.left >= 0. && bar.left + bar.width <= size);
            assert!(bar.top >= 0. && bar.top + bar.height <= size);
            // The icon's corners are rounded, but never so far that a bar
            // becomes a lozenge.
            assert!(bar.corner > 0. && bar.corner <= bar.width / 2.);
        }
    }
}

/// The weave rests at the icon pose, so the reduced-motion frame and the seam
/// of the loop both look like the app icon.
#[test]
fn weave_rests_at_the_icon_pose() {
    assert_eq!(weft_rise(LOOM_MARK_SIZE, 0.), 0.);
    assert_eq!(mark_breath(0.), 1.);
    // The seam matches: progress just below 1 is back where progress 0 is, so a
    // repeating animation never jumps.
    assert!(weft_rise(LOOM_MARK_SIZE, 0.999_9).abs() < 1e-4);
    assert!((mark_breath(0.999_9) - 1.).abs() < 1e-4);
}

/// The greatest displacement of one weave: the weft lifts about a pixel and the
/// mark dims to 65% of the run's accent colour. Both keep the mark inside its
/// 14px square and legible on a dark surface.
#[test]
fn weave_peak_lifts_the_weft_and_dims_the_mark() {
    let size = LOOM_MARK_SIZE;
    let peak_rise = weft_rise(size, 0.5);
    assert!(
        (peak_rise - -1.26).abs() < 1e-4,
        "peak rise was {peak_rise}"
    );
    let peak_breath = mark_breath(0.5);
    assert!(
        (peak_breath - 0.65).abs() < 1e-4,
        "peak breath was {peak_breath}"
    );

    let [_, weft] = loom_mark_bars(size);
    for step in 0..=100 {
        let progress = step as f32 / 100.;
        let rise = weft_rise(size, progress);
        // The weft only ever rides up the warp, so the mark never paints below
        // the square it was given.
        assert!((-size..=0.).contains(&rise), "rise {rise} left the mark");
        assert!(weft.top + rise >= 0.);
        let breath = mark_breath(progress);
        assert!(
            (0.65..=1.).contains(&breath),
            "breath {breath} left the range"
        );
    }
}
