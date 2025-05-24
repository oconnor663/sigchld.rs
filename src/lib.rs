use std::io::{self, ErrorKind, Read};
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::sync::{Condvar, Mutex, OnceLock};

static STATE: Mutex<State> = Mutex::new(State::NoOneIsWaiting);
static CONDVAR: Condvar = Condvar::new();

enum State {
    NoOneIsWaiting,
    SomeoneIsWaiting,
}

static SIGCHLD_READER: OnceLock<UnixStream> = OnceLock::new();

/// Some thread must call `init` (successfully) before any thread calls [`wait`] or
/// [`wait_timeout`].
pub fn init() -> io::Result<()> {
    // Double-check locking. The first check is the already-initialized fast path.
    if SIGCHLD_READER.get().is_some() {
        return Ok(());
    }

    // Take the state mutex, so that only one thread tries to initialize the pipe at a time. We
    // don't want the mutex to own the reader, though, because many threads threads need to be able
    // to observe the state while one thread blocks on the reader.
    let _guard = STATE.lock().unwrap();

    // Check again, because two threads could've raced to take the lock.
    if SIGCHLD_READER.get().is_some() {
        return Ok(());
    }

    // Open and register the pipe. We could use a regular pipe instead of a Unix socket, but the
    // standard library provides `set_nonblocking` for sockets, which saves us an unsafe libc call.
    // The socket is bidirectional, but we'll only ever write in one direction.
    let (reader, writer) = UnixStream::pair()?;
    reader.set_nonblocking(true)?;
    writer.set_nonblocking(true)?;
    signal_hook::low_level::pipe::register(signal_hook::consts::SIGCHLD, writer)?;
    SIGCHLD_READER.set(reader).expect("only 1 thread gets here");
    Ok(())
}

/// Block the current thread until any `SIGCHLD` signal arrives.
///
/// Spurious wakeups are possible, so even if you know there's only one child process, that process
/// could still be running after this function returns. This does not reap any exited children.
pub fn wait() -> io::Result<()> {
    let mut guard = STATE.lock().unwrap();
    if matches!(*guard, State::SomeoneIsWaiting) {
        // Another thread is already blocking on SIGCHLD_READER. Wait for them to wake us up.
        // Spurious wakeups are allowed, so we don't need to do this in a loop.
        guard = CONDVAR.wait(guard).unwrap();
        drop(guard); // silence warnings
        return Ok(());
    }
    // We're the thread that needs to poll SIGCHLD_READER. Set the SomeoneIsWaiting state while we
    // do this, and unlock the state mutex so that other threads can observe the state in the
    // meantime. After doing this, we *must* unset the state before exiting. No short-circuiting
    // with io errors.
    *guard = State::SomeoneIsWaiting;
    drop(guard);

    // The real work happens here. We can't short-circuit in this critical section.
    let wait_result = wait_inner();

    // Regardless of whether wait_inner succeeded or failed, reacquire the state mutex, exit the
    // SomeoneIsWaiting state, and wake up everyone who's sleeping on the condvar.
    guard = STATE.lock().unwrap();
    *guard = State::NoOneIsWaiting;
    CONDVAR.notify_all();
    wait_result
}

// Within this function we can short-circuit. The caller manages state cleanup.
fn wait_inner() -> io::Result<()> {
    let mut reader = SIGCHLD_READER.get().expect("init must have been called");
    // Wait until the pipe is readable.
    let mut poll_fd = libc::pollfd {
        fd: reader.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    let error_code = unsafe {
        libc::poll(
            &mut poll_fd, // an "array" of one
            1,            // the "array" length
            -1,           // infinite timeout
        )
    };
    if error_code < 0 {
        return Err(io::Error::last_os_error());
    }
    // Read the pipe until EWOULDBLOCK. This could take more than one read.
    let mut buf = [0u8; 1024];
    loop {
        match reader.read(&mut buf) {
            Ok(0) => unreachable!("this pipe should never close"),
            Ok(_) => continue,
            Err(e) if e.kind() == ErrorKind::WouldBlock => break,
            // EINTR should not be possible for a nonblocking read.
            Err(e) => return Err(e),
        }
    }
    // Either we were signaled, or we woke up spuriously. We don't distinguish.
    Ok(())
}
