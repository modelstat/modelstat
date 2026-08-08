//! The installed calibration is what segmentation actually runs on.
//!
//! Its own test binary on purpose: [`install_calibration`] swaps a process-wide
//! value, and the crate's unit tests segment concurrently on the defaults. Here
//! nothing else shares the process, so the swap can be observed exactly.

use modelstat_pipeline::{
    install_calibration, installed_calibration, segment_turns, Calibration, TurnMeta,
};

fn turns(n: usize) -> Vec<TurnMeta> {
    (0..n)
        .map(|i| TurnMeta {
            ts_ms: i as i64 * 1000,
            content_chars: 1,
            // A constant embedding: the topic check never fires, so only the
            // turn cap under test can split this run.
            embedding: vec![1.0, 0.0],
        })
        .collect()
}

#[test]
fn a_pushed_calibration_changes_the_next_scan_and_nothing_else() {
    let turns = turns(40);

    // Out of the box: the compiled defaults, 40 turns under the 100-turn cap.
    assert_eq!(*installed_calibration(), Calibration::default());
    assert_eq!(segment_turns(&turns).len(), 1);

    // A server-delivered payload, through the same validator the config channel
    // uses — no caller passes it anywhere, and segmentation still honours it.
    let (version, tighter) =
        Calibration::from_payload(r#"{"version":7,"segment_max_turns":10}"#).expect("valid");
    assert_eq!(version, 7);
    install_calibration(tighter);
    assert_eq!(segment_turns(&turns).len(), 4);

    // Reverting is just another install — no restart, no re-scan.
    install_calibration(Calibration::default());
    assert_eq!(segment_turns(&turns).len(), 1);
}
