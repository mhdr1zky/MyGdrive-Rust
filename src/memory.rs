//! Return freed snapshot pages after bulk metadata work, at most once a minute.
//! This never evicts live objects, file bytes, journal records or offline data.
pub(crate) fn release_unused() {
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    {
        use std::{
            sync::Mutex,
            time::{Duration, Instant},
        };
        static LAST: Mutex<Option<Instant>> = Mutex::new(None);
        let Ok(mut last) = LAST.try_lock() else {
            return;
        };
        if last.is_some_and(|time| time.elapsed() < Duration::from_secs(60)) {
            return;
        }
        *last = Some(Instant::now());
        drop(last);
        // glibc only returns allocator-owned, unused pages. Active foreground
        // allocations remain owned by their users even if a transfer overlaps.
        unsafe {
            libc::malloc_trim(0);
        }
    }
}
