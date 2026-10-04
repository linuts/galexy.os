use super::Ring;

#[test]
fn push_pop_fifo_order() {
    let mut ring: Ring<char, 3> = Ring::new();
    ring.push('a').expect("empty ring accepts");
    ring.push('b').expect("ring below capacity accepts");
    ring.push('c').expect("ring below capacity accepts");
    assert_eq!(ring.pop(), Some('a'));
    assert_eq!(ring.pop(), Some('b'));
    assert_eq!(ring.pop(), Some('c'));
    assert_eq!(ring.pop(), None);
}

#[test]
fn pop_empty_is_none() {
    let mut ring: Ring<u8, 4> = Ring::new();
    assert_eq!(ring.pop(), None);
    // And popping an emptied ring again keeps working.
    ring.push(1).expect("empty ring accepts");
    assert_eq!(ring.pop(), Some(1));
    assert_eq!(ring.pop(), None);
    assert_eq!(ring.pop(), None);
    ring.push(2).expect("emptied ring accepts again");
    assert_eq!(ring.pop(), Some(2));
}

#[test]
fn overflow_drops_newest() {
    let mut ring: Ring<u8, 4> = Ring::new();
    for i in 0..4 {
        ring.push(i).expect("ring below capacity accepts");
    }
    // Capacity 4: pushing a 5th value is dropped, oldest stays.
    assert_eq!(ring.push(4), None);
    assert_eq!(ring.pop(), Some(0));
    assert_eq!(ring.pop(), Some(1));
    assert_eq!(ring.pop(), Some(2));
    assert_eq!(ring.pop(), Some(3));
    assert_eq!(ring.pop(), None);
}

#[test]
fn wrap_around_works() {
    let mut ring: Ring<u8, 3> = Ring::new();
    // Fill, drain one slot, refill: the logical window now wraps the
    // physical buffer.
    for i in 0..3 {
        let _ = ring.push(i);
    }
    assert_eq!(ring.pop(), Some(0));
    let _ = ring.push(3);
    assert_eq!(ring.pop(), Some(1));
    let _ = ring.push(4);
    assert_eq!(ring.pop(), Some(2));
    let _ = ring.push(5);
    for i in 3..6 {
        assert_eq!(ring.pop(), Some(i), "order preserved across wrap");
    }
    assert_eq!(ring.pop(), None);
}
