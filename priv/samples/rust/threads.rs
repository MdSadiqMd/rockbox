use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::thread;

fn main() {
    let counter = Arc::new(AtomicU64::new(0));
    let handles: Vec<_> = (0..16u64)
        .map(|i| {
            let c = Arc::clone(&counter);
            thread::spawn(move || c.fetch_add(i, Ordering::Relaxed))
        })
        .collect();
    for h in handles {
        let _ = h.join();
    }
    let want: u64 = (0..16).sum();
    println!("counter = {} (expected {})", counter.load(Ordering::Relaxed), want);
}
