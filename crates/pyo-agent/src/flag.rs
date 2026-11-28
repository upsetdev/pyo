//! A process-wide on/off flag the agent reports, like whether it last
//! wrote a client policy in an older install's shape.
//!
//! In the agent it's an [`AtomicBool`]. In tests each test thread has its
//! own value, so a test that sets a flag and reads it back isn't changed
//! by another test running at the same time. `#[tokio::test]` runs each
//! test on its own thread.

use std::sync::atomic::Ordering;
#[cfg(not(test))]
use std::sync::atomic::AtomicBool;

pub struct Flag {
    #[cfg(not(test))]
    on: AtomicBool,
    #[cfg(test)]
    on: std::sync::Mutex<Vec<(std::thread::ThreadId, bool)>>,
}

impl Flag {
    pub const fn new() -> Self {
        Flag {
            #[cfg(not(test))]
            on: AtomicBool::new(false),
            #[cfg(test)]
            on: std::sync::Mutex::new(Vec::new()),
        }
    }

    #[cfg(not(test))]
    pub fn load(&self, order: Ordering) -> bool {
        self.on.load(order)
    }

    #[cfg(not(test))]
    pub fn store(&self, on: bool, order: Ordering) {
        self.on.store(on, order)
    }

    #[cfg(not(test))]
    pub fn swap(&self, on: bool, order: Ordering) -> bool {
        self.on.swap(on, order)
    }

    #[cfg(test)]
    pub fn load(&self, _: Ordering) -> bool {
        let me = std::thread::current().id();
        let all = self.on.lock().unwrap_or_else(|e| e.into_inner());
        all.iter().any(|(t, on)| *t == me && *on)
    }

    #[cfg(test)]
    pub fn store(&self, on: bool, order: Ordering) {
        self.swap(on, order);
    }

    #[cfg(test)]
    pub fn swap(&self, on: bool, _: Ordering) -> bool {
        let me = std::thread::current().id();
        let mut all = self.on.lock().unwrap_or_else(|e| e.into_inner());
        match all.iter_mut().find(|(t, _)| *t == me) {
            Some((_, was)) => std::mem::replace(was, on),
            None => {
                all.push((me, on));
                false
            }
        }
    }
}

impl Default for Flag {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    static F: Flag = Flag::new();

    /// One thread's value isn't another's.
    #[test]
    fn each_test_thread_has_its_own_value() {
        F.store(true, Ordering::Relaxed);
        assert!(F.load(Ordering::Relaxed));
        let other = std::thread::spawn(|| {
            let before = F.load(Ordering::Relaxed);
            F.store(false, Ordering::Relaxed);
            before
        })
        .join()
        .unwrap();
        assert!(!other);
        assert!(F.load(Ordering::Relaxed), "another thread's store didn't reach this one");
        assert!(F.swap(false, Ordering::Relaxed));
        assert!(!F.swap(true, Ordering::Relaxed));
    }
}
