#![cfg(loom)]

use loom::model::Builder;
use loom::sync::Arc;
use loom::sync::atomic::{AtomicBool, AtomicUsize, Ordering::SeqCst};
use loom::thread;

#[test]
fn sequentially_consistent_protocol_does_not_retire_an_observed_value() {
    let mut builder = Builder::new();
    builder.max_branches = 10_000;
    builder.check(|| {
        let pointer_is_new = Arc::new(AtomicBool::new(false));
        let enters = Arc::new(AtomicUsize::new(0));
        let old_is_alive = Arc::new(AtomicBool::new(true));

        let reader = {
            let pointer_is_new = Arc::clone(&pointer_is_new);
            let enters = Arc::clone(&enters);
            let old_is_alive = Arc::clone(&old_is_alive);
            thread::spawn(move || {
                enters.fetch_add(1, SeqCst);
                let saw_new = pointer_is_new.load(SeqCst);
                thread::yield_now();
                if !saw_new {
                    assert!(old_is_alive.load(SeqCst), "reader observed a retired value");
                }
                enters.fetch_sub(1, SeqCst);
            })
        };

        let writer = {
            let pointer_is_new = Arc::clone(&pointer_is_new);
            let enters = Arc::clone(&enters);
            let old_is_alive = Arc::clone(&old_is_alive);
            thread::spawn(move || {
                pointer_is_new.swap(true, SeqCst);
                while enters.load(SeqCst) > 0 {
                    thread::yield_now();
                }
                old_is_alive.store(false, SeqCst);
            })
        };

        reader.join().unwrap();
        writer.join().unwrap();
    });
}
