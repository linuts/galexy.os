use super::Bitmap;

#[test]
fn set_and_test_roundtrip() {
    let mut bm = Bitmap::<4>::new(); // 256 bits
    for index in [0, 1, 63, 64, 65, 127, 128, 255] {
        assert!(!bm.test(index), "fresh bit {index} should be clear");
        bm.set(index, true);
        assert!(bm.test(index), "bit {index} should be set");
        bm.set(index, false);
        assert!(!bm.test(index), "cleared bit {index} should be clear");
    }
}

#[test]
fn first_clear_scans() {
    let mut bm = Bitmap::<1>::new(); // 64 bits
    assert_eq!(bm.first_clear(), Some(0));
    bm.set(0, true);
    bm.set(1, true);
    bm.set(2, true);
    assert_eq!(bm.first_clear(), Some(3));
    bm.set(3, false); // already clear; no-op
                      // Fill everything except bit 50.
    for index in 0..64 {
        if index != 50 {
            bm.set(index, true);
        }
    }
    assert_eq!(bm.first_clear(), Some(50));
    bm.set(50, true);
    assert_eq!(bm.first_clear(), None, "full bitmap has no clear bit");
}

#[test]
fn count_set_counts_across_words() {
    let mut bm = Bitmap::<2>::new();
    assert_eq!(bm.count_set(), 0);
    bm.set(63, true);
    assert_eq!(bm.count_set(), 1);
    bm.set(64, true); // first bit of the second word
    bm.set(127, true);
    assert_eq!(bm.count_set(), 3);
}

#[test]
#[should_panic(expected = "out of bounds")]
fn out_of_bounds_panics() {
    let bm = Bitmap::<1>::new();
    let _ = bm.test(64);
}

#[test]
fn fill_sets_everything() {
    let mut bm = Bitmap::<2>::new();
    bm.fill(true);
    assert_eq!(bm.count_set(), 128);
    assert_eq!(bm.first_clear(), None);
    bm.fill(false);
    assert_eq!(bm.count_set(), 0);
    assert_eq!(bm.first_clear(), Some(0));
}

#[test]
fn capacity_is_word_bits_times_words() {
    assert_eq!(Bitmap::<1>::new().capacity(), 64);
    assert_eq!(Bitmap::<3>::new().capacity(), 192);
}
