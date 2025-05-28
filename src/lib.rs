//! # `sigchld` [![Actions Status](https://github.com/oconnor663/sigchld.rs/workflows/tests/badge.svg)](https://github.com/oconnor663/sigchld.rs/actions) [![crates.io](https://img.shields.io/crates/v/sigchld.svg)](https://crates.io/crates/sigchld) [![docs.rs](https://docs.rs/sigchld/badge.svg)](https://docs.rs/sigchld)
//!
//! This is a low-level utility for child process management. Unix doesn't provide a portable\* API
//! for waiting for a child process to exit **with a timeout**. The closest thing is waiting for
//! the `SIGCHLD` signal to be delivered, but Unix signal handling is quite complicated and
//! error-prone. This crate implements `SIGCHLD` handling (using [`signal_hook`] internally for
//! compatibility with other signal handling libraries) and allows any number of threads to wait
//! for that signal, with an optional timeout.
//!
//! Note that `SIGCHLD` indicates that _any_ child process has exited, but there's no (100%
//! reliable) way to know _which_ child it was. You need to [poll your child process][try_wait] in
//! a loop, and wait again if it hasn't exited yet. Most applications will want a higher-level
//! crate that does this loop internally; I'll list such crates here as they're implemented.
//!
//! \* Linux supports `signalfd`, but there's no equivalent on e.g. macOS.
//!
//! # Example
//!
//! ```rust
//! # fn main() -> std::io::Result<()> {
//! # use std::time::Duration;
//! // Create a waiter before spawning the child, to guarantee that we don't miss a signal.
//! let waiter = sigchld::Waiter::new()?;
//!
//! // Start a child process that sleeps for up to 3 seconds.
//! let sleep_time: f32 = rand::random_range(0.0..=3.0);
//! println!("Sleeping for {sleep_time:.3} seconds...", );
//! std::process::Command::new("sleep").arg(format!("{sleep_time}")).spawn()?;
//!
//! // Wait half a second for *any* child to exit. In this example `sleep` is the only child
//! // process, but in general we won't necessarily know which child woke us up.
//! let signaled: bool = waiter.wait_timeout(Duration::from_secs(1))?;
//!
//! if signaled {
//!     // In *this example* we know that the signal came from `sleep`.
//!     println!("Sleep exited.");
//! } else {
//!     println!("We gave up waiting after 1 sec.");
//! }
//! # Ok(())
//! # }
//! ```
//!
//! [`signal_hook`]: https://docs.rs/signal-hook
//! [try_wait]: https://doc.rust-lang.org/std/process/struct.Child.html#method.try_wait

use std::ffi::c_int;
use std::io::{self, ErrorKind, Read};
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::sync::{Arc, Condvar, LazyLock, Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};

struct State {
    someone_is_polling: bool,
    // Every time a signal is received, we replace the marker. Waiters clone the marker when
    // they're created, and they compare their clone the with the current marker *by address* to
    // see if any signals have been received since they were created. Heap addresses won't be
    // reused while any waiter retains a clone. The value of the byte in the marker doesn't matter,
    // but we can't use a ZST, because those don't actually get allocated.
    //
    // We could've just used a counter, but then in theory a counter could wrap around. And we
    // don't want Waiters to be Copy anyway, because the wait methods take them by value.
    signal_marker: Arc<u8>,
}

static STATE: LazyLock<Mutex<State>> = LazyLock::new(|| {
    Mutex::new(State {
        someone_is_polling: false,
        signal_marker: Arc::new(42), // the value doesn't matter
    })
});

static CONDVAR: Condvar = Condvar::new();

static SIGCHLD_READER: OnceLock<UnixStream> = OnceLock::new();

// Use anyhow errors in testing, for backtraces.
#[cfg(test)]
type Result<T> = anyhow::Result<T>;
#[cfg(not(test))]
type Result<T> = io::Result<T>;

/// An object that buffers `SIGCHLD` signals so that you can wait on them reliably.
///
/// There's a required order of operations here:
///
/// 1. Create a `Waiter`.
/// 2. Poll your child process with [`Child::try_wait`] or similar, to see if it's already exited.
/// 3. Call [`wait`](Self::wait), [`wait_timeout`](Self::wait_timeout), or
///    [`wait_deadline`](Self::wait_deadline).
/// 4. Loop back to step 1.
///
/// For example:
///
/// ```rust
/// # use std::io;
/// # fn main() -> io::Result<()> {
/// let mut child = std::process::Command::new("sleep").arg("1").spawn()?;
/// loop {
///     let waiter = sigchld::Waiter::new()?;
///     if child.try_wait()?.is_some() {
///         // The child has exited.
///         break;
///     }
///     waiter.wait()?;
/// }
/// # Ok(())
/// # }
/// ```
///
/// If you don't create the `Waiter` _before_ polling the child, then a signal delivered
/// immediately after you poll could get missed, and you could wait forever.
///
/// [`Child::try_wait`]: https://doc.rust-lang.org/std/process/struct.Child.html#method.try_wait
#[derive(Debug)]
pub struct Waiter {
    original_signal_marker: Arc<u8>,
}

impl Waiter {
    /// Create a `Waiter`.
    ///
    /// Any `SIGCHLD` signals that arrive after a `Waiter` is created, but before a call to
    /// [`wait`](Self::wait), [`wait_timeout`](Self::wait_timeout), or
    /// [`wait_deadline`](Self::wait_deadline), will be buffered. In that case the next call to one
    /// of those methods will return immediately.
    pub fn new() -> Result<Self> {
        let mut state_guard = lock_no_poison(&STATE);

        // The first time we get here, open the pipe and register the signal handler. Holding the
        // state lock guarantees that only one thread does this. (This is similar to
        // OnceLock::get_or_init, but that method makes it harder to return errors.)
        if SIGCHLD_READER.get().is_none() {
            // We could use a regular pipe instead of a Unix socket, but the standard library
            // provides `set_nonblocking` for sockets, which saves us an unsafe libc call. The
            // socket is bidirectional, but we'll only ever write in one direction.
            let (reader, writer) = UnixStream::pair()?;
            reader.set_nonblocking(true)?;
            writer.set_nonblocking(true)?;
            signal_hook::low_level::pipe::register(signal_hook::consts::SIGCHLD, writer)?;
            SIGCHLD_READER.set(reader).expect("only 1 thread gets here");
        }

        // If no other thread is currently polling the reader, drain it. This means we won't wake
        // up immediately for signals that were delivered before this Waiter was created. Again
        // it's important that we're holding the state lock while we do this.
        if !state_guard.someone_is_polling {
            drain_sigchld_reader(&mut state_guard)?;
            // There can't be anyone sleeping on the condvar at this point, no need to notify.
        }

        Ok(Self {
            original_signal_marker: Arc::clone(&state_guard.signal_marker),
        })
    }

    /// Block the current thread until any `SIGCHLD` signal arrives.
    ///
    /// If any `SIGCHLD` signals have arrived since the `Waiter` was created, this function will
    /// return immediately. This avoids a race condition where the child exits right after you call
    /// [`Child::try_wait`] but right before you call this function.
    ///
    /// This function does not reap any exited children. Child process cleanup is only done by
    /// [`Child::wait`] or [`Child::try_wait`].
    ///
    /// This function is not currently susceptible to "spurious wakeups" (i.e. returning early for
    /// no reason), but this property isn't guaranteed, and future versions might be. Getting woken
    /// up early by an unrelated child process exiting (e.g. one spawned by some unknown library
    /// code running on another thread) is similar to a spurious wakeup, and you might need to be
    /// defensive and wait in a loop either way.
    ///
    /// [`Child::wait`]: https://doc.rust-lang.org/std/process/struct.Child.html#method.wait
    /// [`Child::try_wait`]: https://doc.rust-lang.org/std/process/struct.Child.html#method.try_wait
    pub fn wait(self) -> Result<()> {
        let signaled = self.wait_inner(None)?;
        debug_assert!(signaled, "timeout shouldn't be possible");
        Ok(())
    }

    /// Block the current thread until either any `SIGCHLD` signal arrives or a timeout passes.
    /// Return `true` if a signal arrived before the timeout.
    ///
    /// If any `SIGCHLD` signals have arrived since the `Waiter` was created, this function will
    /// return immediately. This avoids a race condition where the child exits right after you call
    /// [`Child::try_wait`] but right before you call this function.
    ///
    /// This function does not reap any exited children. Child process cleanup is only done by
    /// [`Child::wait`] or [`Child::try_wait`].
    ///
    /// This function is not currently susceptible to "spurious wakeups" (i.e. returning early for
    /// no reason), but this property isn't guaranteed, and future versions might be. Getting woken
    /// up early by an unrelated child process exiting (e.g. one spawned by some unknown library
    /// code running on another thread) is similar to a spurious wakeup, and you might need to be
    /// defensive and wait in a loop either way.
    ///
    /// [`Child::wait`]: https://doc.rust-lang.org/std/process/struct.Child.html#method.wait
    /// [`Child::try_wait`]: https://doc.rust-lang.org/std/process/struct.Child.html#method.try_wait
    pub fn wait_timeout(self, timeout: Duration) -> Result<bool> {
        let deadline = Instant::now() + timeout;
        self.wait_inner(Some(deadline))
    }

    /// Block the current thread until either any `SIGCHLD` signal arrives or a deadline passes.
    /// Return `true` if a signal arrived before the deadline.
    ///
    /// If any `SIGCHLD` signals have arrived since the `Waiter` was created, this function will
    /// return immediately. This avoids a race condition where the child exits right after you call
    /// [`Child::try_wait`] but right before you call this function.
    ///
    /// This function does not reap any exited children. Child process cleanup is only done by
    /// [`Child::wait`] or [`Child::try_wait`].
    ///
    /// This function is not currently susceptible to "spurious wakeups" (i.e. returning early for
    /// no reason), but this property isn't guaranteed, and future versions might be. Getting woken
    /// up early by an unrelated child process exiting (e.g. one spawned by some unknown library
    /// code running on another thread) is similar to a spurious wakeup, and you might need to be
    /// defensive and wait in a loop either way.
    ///
    /// [`Child::wait`]: https://doc.rust-lang.org/std/process/struct.Child.html#method.wait
    /// [`Child::try_wait`]: https://doc.rust-lang.org/std/process/struct.Child.html#method.try_wait
    pub fn wait_deadline(self, deadline: Instant) -> Result<bool> {
        self.wait_inner(Some(deadline))
    }

    fn wait_inner(self, maybe_deadline: Option<Instant>) -> Result<bool> {
        let mut state_guard = lock_no_poison(&STATE);
        loop {
            // Check the signal count. If it's changed, another thread has observed SIGCHLD, and we
            // should return.
            if !Arc::ptr_eq(&self.original_signal_marker, &state_guard.signal_marker) {
                return Ok(true); // Another thread observed a signal.
            }

            // If another thread is already polling the SIGCHLD_READER, wait for them to wake us
            // up. Note that if there wasn't already a polling thread, we'd do at least one
            // non-blocking check for buffered signals, even if our timeout was zero / our deadline
            // was in the past. Since `someone_is_polling` is set, we rely on the polling thread to
            // have done that check *before* releasing the state lock. This sort of issue comes up
            // every single time I use a condvar, and in my head it's "the condvar footgun".
            if state_guard.someone_is_polling {
                if let Some(deadline) = maybe_deadline {
                    let timeout = deadline.saturating_duration_since(Instant::now());
                    let (returned_guard, timeout_result) =
                        CONDVAR.wait_timeout(state_guard, timeout).unwrap();
                    state_guard = returned_guard;
                    if timeout_result.timed_out() {
                        return Ok(false); // We timed out.
                    }
                } else {
                    state_guard = CONDVAR.wait(state_guard).unwrap();
                }
                // Go back to the top of the loop to re-check the signal count. If the polling
                // thread timed out, it's also possible that we might become the polling thread.
                continue;
            }

            // We're the thread that needs to poll the SIGCHLD_READER.
            return poll_sigchld_reader(maybe_deadline, state_guard);
        }
    }
}

fn poll_sigchld_reader(
    maybe_deadline: Option<Instant>,
    mut state_guard: MutexGuard<State>,
) -> Result<bool> {
    // "The condvar footgun": We must do a non-blocking check for our exit condition *before* we
    // release the guard, otherwise other threads might interpret someone_is_waiting to mean that
    // the condition is false, when in fact it would be true if they checked it themselves.
    debug_assert!(!state_guard.someone_is_polling);
    let signaled = drain_sigchld_reader(&mut state_guard)?;
    if signaled {
        // There can't be anyone sleeping on the condvar at this point, no need to notify.
        return Ok(true);
    }

    // There were no pending signals. Now we can change the state and release the guard while we
    // poll. All the fallible IO happens here, but we can't short-circuit in this critical section,
    // because we must reset the state at the end.
    state_guard.someone_is_polling = true;

    let wait_result = poll_sigchld_reader_inner(maybe_deadline, state_guard);

    // Regardless of whether wait_inner succeeded or failed, reacquire the state mutex, unset
    // `someone_is_waiting`, and wake any threads that are sleeping on the condvar. If we timed
    // out, one of them will take over as the polling thread.
    state_guard = lock_no_poison(&STATE);
    state_guard.someone_is_polling = false;
    CONDVAR.notify_all();
    wait_result
}

// Within this function we can short-circuit. The caller manages state cleanup.
fn poll_sigchld_reader_inner(
    maybe_deadline: Option<Instant>,
    mut state_guard: MutexGuard<State>,
) -> Result<bool> {
    let reader = SIGCHLD_READER.get().expect("already initialized");
    // Wait until the pipe is readable or the deadline passes.
    loop {
        let mut poll_fd = libc::pollfd {
            fd: reader.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        let timeout_ms: c_int = if let Some(deadline) = maybe_deadline {
            let timeout = deadline.saturating_duration_since(Instant::now());
            // Convert to milliseconds, rounding *up*. (That way we don't repeatedly sleep for 0ms
            // when we're close to the timeout.)
            (timeout.as_nanos().saturating_add(999_999) / 1_000_000)
                .try_into()
                .unwrap_or(c_int::MAX)
        } else {
            -1 // infinite timeout
        };
        // Release the state lock while polling and reacquire it afterwards.
        drop(state_guard);
        let error_code = unsafe {
            libc::poll(
                &mut poll_fd, // an "array" of one
                1,            // the "array" length
                timeout_ms,
            )
        };
        state_guard = lock_no_poison(&STATE);
        if error_code < 0 {
            // EINTR is expected here. If we're the only running thread, then we're the only thread
            // that can handle SIGCHLD, so it's probably even guaranteed. We don't *have* to loop,
            // because spurious wakeups are allowed, but it would be bad behavior to wake the caller
            // for unrelated signals.
            let e = io::Error::last_os_error();
            if e.kind() == ErrorKind::Interrupted {
                continue;
            } else {
                #[allow(clippy::useless_conversion)]
                return Err(io::Error::last_os_error().into());
            }
        }
        // Read signal bytes out of the pipe.
        let signaled = drain_sigchld_reader(&mut state_guard)?;
        // If we were signaled, return.
        if signaled {
            return Ok(true);
        }
        // Check the clock to see if we've timed out.
        if let Some(deadline) = maybe_deadline {
            if Instant::now() > deadline {
                return Ok(false);
            }
        }
        // Otherwise we must've woken up spuriously. Keep looping.
    }
}

// Read the SIGCHLD pipe until EOF and return true (signaled) if anything was in it. In that case
// also increment the signal_count.
fn drain_sigchld_reader(state: &mut State) -> Result<bool> {
    let mut reader = SIGCHLD_READER.get().expect("already initialized");
    // Read the pipe until EWOULDBLOCK. This could take more than one read.
    let mut buf = [0u8; 1024];
    let mut signaled = false;
    loop {
        match reader.read(&mut buf) {
            Ok(0) => unreachable!("this pipe should never close"),
            Ok(_) => signaled = true,
            Err(e) if e.kind() == ErrorKind::WouldBlock => break,
            // EINTR shouldn't be possible for a nonblocking read.
            #[allow(clippy::useless_conversion)]
            Err(e) => return Err(e.into()),
        }
    }
    if signaled {
        // If we were signaled, replace the marker. This is guaranteed to be a different heap
        // address than any marker clone saved by any still-existing waiter. The numerical value
        // doesn't matter.
        state.signal_marker = Arc::new(42);
    }
    Ok(signaled)
}

fn lock_no_poison<T>(mutex: &Mutex<T>) -> MutexGuard<T> {
    match mutex.lock() {
        Ok(guard) => guard,
        Err(e) => e.into_inner(),
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use duct::cmd;
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    // We need to make sure only one test runs at a time, because these waits are global, and
    // they'll confuse each other. Use a parking_lot mutex so that it doesn't get poisoned.
    //
    // XXX: These tests don't wait in a loop, because if there are bugs here that cause early
    // wakeups, I'd rather the tests fail than hide the bug. I expect these tests will "randomly"
    // fail under certain circumstances, and that's worth it to me to catch more bugs. But real
    // callers should wait in a loop so that they don't randomly fail.
    static ONE_TEST_AT_A_TIME: parking_lot::Mutex<()> = parking_lot::Mutex::new(());

    #[track_caller]
    fn assert_approx_eq(dur1: Duration, dur2: Duration) {
        const CLOSE_ENOUGH: f64 = 0.1; // 10%
        let lower_bound = 1.0 - CLOSE_ENOUGH;
        let upper_bound = 1.0 + CLOSE_ENOUGH;
        let ratio = dur1.as_secs_f64() / dur2.as_secs_f64();
        assert!(
            lower_bound < ratio && ratio < upper_bound,
            "{dur1:?} and {dur2:?} are not close enough",
        );
    }

    #[test]
    fn test_wait() -> Result<()> {
        let _test_guard = ONE_TEST_AT_A_TIME.lock();
        let start = Instant::now();

        let waiter = Waiter::new()?;
        cmd!("sleep", "0.25").start()?;
        waiter.wait()?;
        let dur = Instant::now() - start;
        assert_approx_eq(Duration::from_millis(250), dur);

        Ok(())
    }

    #[test]
    fn test_wait_deadline() -> Result<()> {
        let _test_guard = ONE_TEST_AT_A_TIME.lock();
        let start = Instant::now();

        let timeout = Duration::from_millis(500);
        let waiter = Waiter::new()?;
        cmd!("sleep", "0.25").start()?;
        // This first wait should return true.
        let signaled = waiter.wait_deadline(Instant::now() + timeout)?;
        let dur = Instant::now() - start;
        assert_approx_eq(Duration::from_millis(250), dur);
        assert!(signaled);

        // This second wait should time out and return false.
        let waiter2 = Waiter::new()?;
        let signaled2 = waiter2.wait_deadline(Instant::now() + timeout)?;
        let dur2 = Instant::now() - start;
        assert_approx_eq(Duration::from_millis(750), dur2);
        assert!(!signaled2);

        Ok(())
    }

    #[test]
    fn test_wait_timeout() -> Result<()> {
        let _test_guard = ONE_TEST_AT_A_TIME.lock();
        let start = Instant::now();

        let timeout = Duration::from_millis(500);
        let waiter = Waiter::new()?;
        cmd!("sleep", "0.25").start()?;
        // This first wait should return true.
        let signaled = waiter.wait_timeout(timeout)?;
        let dur = Instant::now() - start;
        assert_approx_eq(Duration::from_millis(250), dur);
        assert!(signaled);

        // This second wait should time out and return false.
        let waiter2 = Waiter::new()?;
        let signaled2 = waiter2.wait_timeout(timeout)?;
        let dur2 = Instant::now() - start;
        assert_approx_eq(Duration::from_millis(750), dur2);
        assert!(!signaled2);

        Ok(())
    }

    #[test]
    fn test_wait_many_threads() -> Result<()> {
        let _test_guard = ONE_TEST_AT_A_TIME.lock();
        let start = Instant::now();

        let handle = Arc::new(cmd!("sleep", "1").start()?);
        let mut wait_threads = Vec::new();
        let mut short_timeout_threads = Vec::new();
        let mut long_timeout_threads = Vec::new();
        for _ in 0..3 {
            let handle_clone = handle.clone();
            let waiter = Waiter::new()?;
            wait_threads.push(std::thread::spawn(move || -> Result<Duration> {
                waiter.wait()?;
                let dur = Instant::now() - start;
                assert!(handle_clone.try_wait()?.is_some(), "should've exited");
                Ok(dur)
            }));
            let handle_clone = handle.clone();
            let waiter = Waiter::new()?;
            short_timeout_threads.push(std::thread::spawn(move || -> Result<bool> {
                let signaled = waiter.wait_timeout(Duration::from_millis(500))?;
                assert!(handle_clone.try_wait()?.is_none(), "shouldn't have exited");
                Ok(signaled)
            }));
            let handle_clone = handle.clone();
            let waiter = Waiter::new()?;
            long_timeout_threads.push(std::thread::spawn(move || -> Result<bool> {
                let signaled = waiter.wait_timeout(Duration::from_millis(1500))?;
                assert!(handle_clone.try_wait()?.is_some(), "should've exited");
                Ok(signaled)
            }));
        }
        for thread in wait_threads {
            let dur = thread.join().unwrap()?;
            assert_approx_eq(Duration::from_millis(1000), dur);
        }
        for thread in short_timeout_threads {
            assert!(!thread.join().unwrap()?, "should not be signaled");
        }
        for thread in long_timeout_threads {
            assert!(thread.join().unwrap()?, "should be signaled");
        }

        Ok(())
    }
}
