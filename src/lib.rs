use std::ffi::c_int;
use std::io::{self, ErrorKind, Read};
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::sync::{Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant};

static STATE: Mutex<State> = Mutex::new(State::NoOneIsWaiting);
static CONDVAR: Condvar = Condvar::new();

enum State {
    NoOneIsWaiting,
    SomeoneIsWaiting,
}

static SIGCHLD_READER: OnceLock<UnixStream> = OnceLock::new();

// Use anyhow errors in testing, for backtraces.
#[cfg(test)]
type Result<T> = anyhow::Result<T>;
#[cfg(not(test))]
type Result<T> = io::Result<T>;

fn get_or_init_reader() -> Result<&'static UnixStream> {
    // Double-check locking. The first check is the already-initialized fast path.
    if let Some(reader) = SIGCHLD_READER.get() {
        return Ok(reader);
    }

    // Take the state mutex, so that only one thread tries to initialize the pipe at a time. We
    // don't want the mutex to own the reader, though, because many threads threads need to be able
    // to observe the state while one thread blocks on the reader.
    let _guard = STATE.lock().unwrap();

    // Check again, because two threads could've raced to take the lock.
    if let Some(reader) = SIGCHLD_READER.get() {
        return Ok(reader);
    }

    // Open and register the pipe. We could use a regular pipe instead of a Unix socket, but the
    // standard library provides `set_nonblocking` for sockets, which saves us an unsafe libc call.
    // The socket is bidirectional, but we'll only ever write in one direction.
    let (reader, writer) = UnixStream::pair()?;
    reader.set_nonblocking(true)?;
    writer.set_nonblocking(true)?;
    signal_hook::low_level::pipe::register(signal_hook::consts::SIGCHLD, writer)?;
    SIGCHLD_READER.set(reader).expect("only 1 thread gets here");
    Ok(SIGCHLD_READER.get().unwrap())
}

/// Block the current thread until either any `SIGCHLD` signal arrives.
///
/// Signals are buffered, and this function will return immediately if any signals have arrived
/// since the last time it was called, even if that was a long time ago. Spurious wakeups are also
/// possible. For both those reasons, you usually need to call this in a loop and poll your child
/// process each time it returns.
///
/// This function does not reap any exited children. Child process cleanup is only done by
/// [`Child::wait`] or [`Child::try_wait`].
///
/// [`Child::wait`]: https://doc.rust-lang.org/std/process/struct.Child.html#method.wait
/// [`Child::try_wait`]: https://doc.rust-lang.org/std/process/struct.Child.html#method.try_wait
pub fn wait() -> Result<()> {
    let signaled = wait_inner(None)?;
    debug_assert!(signaled, "timeout shouldn't be possible");
    Ok(())
}

/// Block the current thread until either any `SIGCHLD` signal arrives or a timeout passes. Returns
/// `true` if a signal arrived before the timeout.
///
/// Signals are buffered, and this function will return immediately if any signals have arrived
/// since the last time it was called, even if that was a long time ago. Spurious wakeups are also
/// possible. For both those reasons, you usually need to call this in a loop and poll your child
/// process each time it returns. [`wait_deadline`] can be more convenient, since you don't need to
/// decrement your timeout each time through the loop.
///
/// This function does not reap any exited children. Child process cleanup is only done by
/// [`Child::wait`] or [`Child::try_wait`].
///
/// [`Child::wait`]: https://doc.rust-lang.org/std/process/struct.Child.html#method.wait
/// [`Child::try_wait`]: https://doc.rust-lang.org/std/process/struct.Child.html#method.try_wait
pub fn wait_timeout(timeout: Duration) -> Result<bool> {
    let deadline = Instant::now() + timeout;
    wait_inner(Some(deadline))
}

/// Block the current thread until either any `SIGCHLD` signal arrives or a deadline passes.
/// Returns `true` if a signal arrived before the deadline.
///
/// Signals are buffered, and this function will return immediately if any signals have arrived
/// since the last time it was called, even if that was a long time ago. Spurious wakeups are also
/// possible. For both those reasons, you usually need to call this in a loop and poll your child
/// process each time it returns.
///
/// This function does not reap any exited children. Child process cleanup is only done by
/// [`Child::wait`] or [`Child::try_wait`].
///
/// [`Child::wait`]: https://doc.rust-lang.org/std/process/struct.Child.html#method.wait
/// [`Child::try_wait`]: https://doc.rust-lang.org/std/process/struct.Child.html#method.try_wait
pub fn wait_deadline(deadline: Instant) -> Result<bool> {
    wait_inner(Some(deadline))
}

fn wait_inner(maybe_deadline: Option<Instant>) -> Result<bool> {
    let mut guard = STATE.lock().unwrap();
    if matches!(*guard, State::SomeoneIsWaiting) {
        // Another thread is already blocking on SIGCHLD_READER. Wait for them to wake us up.
        // Spurious wakeups are allowed, so we don't need to do this in a loop.
        if let Some(deadline) = maybe_deadline {
            let timeout = deadline.saturating_duration_since(Instant::now());
            let (guard, timeout_result) = CONDVAR.wait_timeout(guard, timeout).unwrap();
            drop(guard); // silence warnings
            return Ok(!timeout_result.timed_out());
        } else {
            let guard = CONDVAR.wait(guard).unwrap();
            drop(guard); // silence warnings
            return Ok(true);
        }
    }

    // We're the thread that needs to poll SIGCHLD_READER. Set the SomeoneIsWaiting state while we
    // do this, and unlock the state mutex so that other threads can observe the state in the
    // meantime. After doing this, we *must* unset the state before exiting. No short-circuiting
    // with io errors.
    *guard = State::SomeoneIsWaiting;
    drop(guard);

    // The real work happens here. We can't short-circuit in this critical section.
    let wait_result = wait_short_circuitable(maybe_deadline);

    // Regardless of whether wait_inner succeeded or failed, reacquire the state mutex, exit the
    // SomeoneIsWaiting state, and wake up everyone who's sleeping on the condvar.
    guard = STATE.lock().unwrap();
    *guard = State::NoOneIsWaiting;
    CONDVAR.notify_all();
    wait_result
}

// Within this function we can short-circuit. The caller manages state cleanup.
fn wait_short_circuitable(maybe_deadline: Option<Instant>) -> Result<bool> {
    let mut reader = get_or_init_reader()?;
    // Wait until the pipe is readable or the deadline passes.
    let mut poll_fd = libc::pollfd {
        fd: reader.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    loop {
        let timeout_ms: c_int = if let Some(deadline) = maybe_deadline {
            let timeout = deadline.saturating_duration_since(Instant::now());
            timeout.as_millis().try_into().unwrap_or(c_int::MAX)
        } else {
            -1 // infinite timeout
        };
        let error_code = unsafe {
            libc::poll(
                &mut poll_fd, // an "array" of one
                1,            // the "array" length
                timeout_ms,
            )
        };
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
        // Read the pipe until EWOULDBLOCK. This could take more than one read.
        let mut buf = [0u8; 1024];
        let mut did_read_anything = false;
        loop {
            match reader.read(&mut buf) {
                Ok(0) => unreachable!("this pipe should never close"),
                Ok(_) => did_read_anything = true,
                Err(e) if e.kind() == ErrorKind::WouldBlock => break,
                // EINTR should not be possible for a nonblocking read.
                #[allow(clippy::useless_conversion)]
                Err(e) => return Err(e.into()),
            }
        }
        // If we read anything, we were signaled, and we should return. If not, check the clock to
        // see if we've timed out. Otherwise keep looping.
        if did_read_anything {
            // A signal arrived.
            return Ok(true);
        } else if let Some(deadline) = maybe_deadline {
            if Instant::now() > deadline {
                // The deadline passed.
                return Ok(false);
            }
        }
        // Otherwise we must've woken up spuriously. Keep looping.
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use duct::cmd;
    use std::time::{Duration, Instant};

    // We need to make sure only one test runs at a time, because these waits are global, and
    // they'll confuse each other.
    static ONE_TEST_AT_A_TIME: Mutex<()> = Mutex::new(());

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
        let _test_guard = ONE_TEST_AT_A_TIME.lock().unwrap();
        let start = Instant::now();

        cmd!("sleep", "0.25").start()?;
        wait()?;
        let dur = Instant::now() - start;
        assert_approx_eq(Duration::from_millis(250), dur);

        Ok(())
    }

    #[test]
    fn test_wait_deadline() -> Result<()> {
        let _test_guard = ONE_TEST_AT_A_TIME.lock().unwrap();
        let start = Instant::now();

        cmd!("sleep", "0.25").start()?;
        let timeout = Duration::from_millis(500);
        // This first wait should return true.
        let signaled = wait_deadline(Instant::now() + timeout)?;
        let dur = Instant::now() - start;
        assert_approx_eq(Duration::from_millis(250), dur);
        assert!(signaled);

        // This second wait should time out and return false.
        let signaled2 = wait_deadline(Instant::now() + timeout)?;
        let dur2 = Instant::now() - start;
        assert_approx_eq(Duration::from_millis(750), dur2);
        assert!(!signaled2);

        Ok(())
    }

    #[test]
    fn test_wait_timeout() -> Result<()> {
        let _test_guard = ONE_TEST_AT_A_TIME.lock().unwrap();
        let start = Instant::now();

        cmd!("sleep", "0.25").start()?;
        let timeout = Duration::from_millis(500);
        // This first wait should return true.
        let signaled = wait_timeout(timeout)?;
        let dur = Instant::now() - start;
        assert_approx_eq(Duration::from_millis(250), dur);
        assert!(signaled);

        // This second wait should time out and return false.
        let signaled2 = wait_timeout(timeout)?;
        let dur2 = Instant::now() - start;
        assert_approx_eq(Duration::from_millis(750), dur2);
        assert!(!signaled2);

        Ok(())
    }

    #[test]
    fn test_wait_many_threads() -> Result<()> {
        let _test_guard = ONE_TEST_AT_A_TIME.lock().unwrap();
        let start = Instant::now();

        cmd!("sleep", "0.25").start()?;
        let mut wait_threads = Vec::new();
        let mut short_timeout_threads = Vec::new();
        let mut long_timeout_threads = Vec::new();
        for _ in 0..4 {
            wait_threads.push(std::thread::spawn(move || -> Result<Duration> {
                wait()?;
                Ok(Instant::now() - start)
            }));
            short_timeout_threads.push(std::thread::spawn(move || -> Result<bool> {
                wait_timeout(Duration::from_millis(100))
            }));
            long_timeout_threads.push(std::thread::spawn(move || -> Result<bool> {
                wait_timeout(Duration::from_millis(400))
            }));
        }
        for thread in wait_threads {
            let dur = thread.join().unwrap()?;
            assert_approx_eq(Duration::from_millis(250), dur);
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
