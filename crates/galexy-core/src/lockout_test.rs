//! Host tests for login cool-down accounting.

use super::*;

fn arm_actor(table: &mut LoginLockout, name: &str, tty: u8) {
    for i in 1..=LOCKOUT_MAX_FAILS {
        let note = table.fail(name, tty, 1_000, true);
        assert!(note.tracked);
        assert_eq!(note.actor_fails, i);
        assert_eq!(note.tty_fails, i);
        if i < LOCKOUT_MAX_FAILS {
            assert!(!note.actor_armed);
            assert!(!note.tty_armed);
            assert!(!table.blocked(name, tty, 1_000));
        } else {
            assert!(note.actor_armed);
            assert!(note.tty_armed);
        }
    }
}

#[test]
fn fifth_failure_arms_actor_and_tty() {
    let mut table = LoginLockout::new();
    arm_actor(&mut table, "eve", 0);
    let until = 1_000 + LOCKOUT_COOLDOWN_MS;
    assert_eq!(table.actor_until("eve"), until);
    assert_eq!(table.tty_until(0), until);
    assert!(table.blocked("eve", 0, until - 1));
    assert!(
        table.blocked("eve", 3, until - 1),
        "actor lock is every tty"
    );
    assert!(
        table.blocked("admin", 0, until - 1),
        "tty lock is every name"
    );
    assert!(!table.blocked("admin", 1, until - 1));
    assert!(!table.blocked("eve", 0, until), "deadline has elapsed");
    assert_eq!(table.active(until - 1), 2);
    assert_eq!(table.active(until), 0);
}

#[test]
fn success_and_expiry_clear() {
    let mut table = LoginLockout::new();
    for _ in 0..4 {
        table.fail("eve", 0, 50, true);
    }
    assert!(!table.blocked("eve", 0, 50));
    table.success("eve", 0);
    assert_eq!(table.actor_until("eve"), 0);
    assert_eq!(table.tty_until(0), 0);

    arm_actor(&mut table, "eve", 1);
    let until = 1_000 + LOCKOUT_COOLDOWN_MS;
    let again = table.fail("eve", 1, until - 10, true);
    assert!(!again.actor_armed, "a locked guess must not extend");
    assert!(!again.tty_armed);
    assert_eq!(table.actor_until("eve"), until);

    table.success("eve", 1);
    assert!(!table.blocked("eve", 1, until - 10));
    assert_eq!(table.active(until - 10), 0);
}

#[test]
fn unknown_name_locks_only_the_tty() {
    let mut table = LoginLockout::new();
    for i in 1..=LOCKOUT_MAX_FAILS {
        let note = table.fail("ghost", 2, 10, false);
        assert!(!note.tracked);
        assert_eq!(note.actor_fails, 0);
        assert_eq!(note.tty_fails, i);
        assert_eq!(note.tty_armed, i == LOCKOUT_MAX_FAILS);
    }
    assert_eq!(table.actor_until("ghost"), 0);
    assert!(table.blocked("eve", 2, 10));
    assert!(!table.blocked("eve", 0, 10));
    assert_eq!(table.active(10), 1);
}

#[test]
fn clear_actor_leaves_the_tty() {
    let mut table = LoginLockout::new();
    arm_actor(&mut table, "eve", 0);
    table.clear_actor("eve");
    assert_eq!(table.actor_until("eve"), 0);
    assert!(table.tty_until(0) > 1_000);
    assert!(table.blocked("admin", 0, 1_000));
    assert!(!table.blocked("eve", 1, 1_000));
}

#[test]
fn expired_slot_can_be_reused() {
    let mut table = LoginLockout::new();
    for i in 0..LOCKOUT_ACTORS {
        let mut buf = [0u8; 4];
        let name = slot_name(i, &mut buf);
        for _ in 0..LOCKOUT_MAX_FAILS {
            table.fail(name, 0, 100, true);
        }
    }
    assert!(table.blocked("n00", 0, 100));
    let open_at = 100 + LOCKOUT_COOLDOWN_MS;
    let note = table.fail("newbie", 1, open_at, true);
    assert!(note.tracked, "an elapsed actor slot is free for a new name");
    assert_eq!(note.actor_fails, 1);
    assert!(!table.blocked("newbie", 1, open_at));
}

fn slot_name(i: usize, buf: &mut [u8; 4]) -> &str {
    buf[0] = b'n';
    buf[1] = b'0' + (i / 10) as u8;
    buf[2] = b'0' + (i % 10) as u8;
    core::str::from_utf8(&buf[..3]).unwrap()
}
