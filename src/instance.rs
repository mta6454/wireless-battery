//! Single instance and `--quit`. The Windows installer runs `WirelessBattery.exe --quit` to close a
//! running copy before upgrading or uninstalling.
//!
//! Windows: the running app owns a named mutex (so a second launch exits quietly) and a named
//! manual-reset event; `--quit` sets the event, the app's watcher thread sees it and asks the UI to
//! exit, and `--quit` waits until the mutex is gone.
//! macOS/Linux: a lock file instead of the mutex, and SIGTERM instead of the event.

#[cfg(windows)]
mod imp {
    use std::ptr::null;

    use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, ERROR_ALREADY_EXISTS, HANDLE};
    use windows_sys::Win32::System::Threading::{CreateEventW, CreateMutexW, OpenEventW, OpenMutexW, SetEvent, WaitForSingleObject, INFINITE};

    const MUTEX: &str = "Local\\WirelessBatteryCrossTray";
    const QUIT_EVENT: &str = "Local\\WirelessBatteryCrossQuit";
    const SYNCHRONIZE: u32 = 0x0010_0000;
    const EVENT_MODIFY_STATE: u32 = 0x0002;

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    /// Held for the lifetime of the app.
    pub struct Instance {
        mutex: HANDLE,
        quit_event: HANDLE,
    }

    /// None when another instance is already running.
    pub fn acquire() -> Option<Instance> {
        // SAFETY: plain Win32 object creation with valid NUL-terminated names.
        unsafe {
            let mutex = CreateMutexW(null(), 1, wide(MUTEX).as_ptr());
            if mutex.is_null() {
                return None;
            }
            if GetLastError() == ERROR_ALREADY_EXISTS {
                CloseHandle(mutex);
                return None;
            }
            let quit_event = CreateEventW(null(), 1, 0, wide(QUIT_EVENT).as_ptr());
            Some(Instance { mutex, quit_event })
        }
    }

    impl Instance {
        /// Blocks until `--quit` is requested, then calls `on_quit`. Run on its own thread.
        pub fn watch_quit(&self, on_quit: impl FnOnce() + Send + 'static) {
            let ev = self.quit_event as usize;
            if ev == 0 {
                return;
            }
            std::thread::spawn(move || {
                // SAFETY: the event handle stays open until the process exits.
                unsafe { WaitForSingleObject(ev as HANDLE, INFINITE) };
                on_quit();
            });
        }
    }

    impl Drop for Instance {
        fn drop(&mut self) {
            // SAFETY: closing handles we created.
            unsafe {
                CloseHandle(self.mutex);
                if !self.quit_event.is_null() {
                    CloseHandle(self.quit_event);
                }
            }
        }
    }

    /// Asks a running instance to exit and waits up to 5 s for it to go. True when nothing runs afterwards.
    pub fn request_quit() -> bool {
        // SAFETY: open/set/close on named kernel objects.
        unsafe {
            let ev = OpenEventW(EVENT_MODIFY_STATE, 0, wide(QUIT_EVENT).as_ptr());
            if ev.is_null() {
                return true; // not running
            }
            SetEvent(ev);
            CloseHandle(ev);
            for _ in 0..50 {
                let m = OpenMutexW(SYNCHRONIZE, 0, wide(MUTEX).as_ptr());
                if m.is_null() {
                    return true;
                }
                CloseHandle(m);
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
            false
        }
    }
}

#[cfg(not(windows))]
mod imp {
    //! macOS/Linux: an exclusive lock on `instance.lock` marks the running instance and holds its
    //! PID. `--quit` only signals that PID while the lock is actually held, so a stale file can
    //! never make it kill an unrelated process.
    use std::fs::{File, OpenOptions, TryLockError};
    use std::io::{Read, Seek, Write};
    use std::path::{Path, PathBuf};
    use std::process::Command;
    use std::time::Duration;

    pub struct Instance {
        _file: Option<File>, // the lock lives as long as this handle
    }

    fn lock_path() -> Option<PathBuf> {
        Some(crate::settings::dir()?.join("instance.lock"))
    }

    fn open(path: &Path) -> Option<File> {
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        OpenOptions::new().read(true).write(true).create(true).truncate(false).open(path).ok()
    }

    /// None when another instance holds the lock.
    pub fn acquire_at(path: &Path) -> Option<Instance> {
        let mut file = open(path)?;
        match file.try_lock() {
            Ok(()) => {}
            Err(TryLockError::WouldBlock) => return None,
            // Locking unsupported here (odd file system): run anyway, just without the guard.
            Err(TryLockError::Error(_)) => {}
        }
        let _ = file.set_len(0);
        let _ = file.rewind();
        let _ = write!(file, "{}", std::process::id());
        let _ = file.flush();
        Some(Instance { _file: Some(file) })
    }

    pub fn acquire() -> Option<Instance> {
        match lock_path() {
            Some(p) => acquire_at(&p),
            None => Some(Instance { _file: None }), // no home directory known: no single-instance guard
        }
    }

    impl Instance {
        /// Nothing to watch: SIGTERM ends the process and the OS removes the tray item.
        pub fn watch_quit(&self, _on_quit: impl FnOnce() + Send + 'static) {}
    }

    fn running_pid(path: &Path) -> Option<Option<u32>> {
        let mut file = open(path)?;
        match file.try_lock() {
            Ok(()) => {
                let _ = file.unlock();
                None // lock was free: nothing is running
            }
            Err(TryLockError::WouldBlock) => {
                let mut text = String::new();
                let _ = file.read_to_string(&mut text);
                Some(text.trim().parse().ok())
            }
            Err(TryLockError::Error(_)) => None,
        }
    }

    /// Signals the running instance and waits up to 5 s for its lock to go.
    /// `terminate` is injectable for tests.
    pub fn request_quit_at(path: &Path, terminate: impl Fn(u32)) -> bool {
        let pid = match running_pid(path) {
            None => return true, // not running
            Some(None) => return false, // running, but its PID is not readable yet
            Some(Some(pid)) => pid,
        };
        terminate(pid);
        for _ in 0..50 {
            if running_pid(path).is_none() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        false
    }

    pub fn request_quit() -> bool {
        let Some(path) = lock_path() else { return true };
        request_quit_at(&path, |pid| {
            let _ = Command::new("kill").arg("-TERM").arg(pid.to_string()).status();
        })
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        fn temp(name: &str) -> PathBuf {
            let dir = std::env::temp_dir().join(format!("wb-instance-test-{}-{name}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            dir.join("instance.lock")
        }

        #[test]
        fn second_instance_is_refused_until_the_first_exits() {
            let p = temp("single");
            let first = acquire_at(&p).expect("first instance gets the lock");
            assert!(acquire_at(&p).is_none(), "second instance must be refused");
            drop(first);
            assert!(acquire_at(&p).is_some(), "lock is free again after exit");
        }

        #[test]
        fn quit_with_nothing_running_does_not_signal_anyone() {
            let p = temp("idle");
            let called = std::cell::Cell::new(false);
            assert!(request_quit_at(&p, |_| called.set(true)));
            assert!(!called.get());
            // A stale file with a PID in it is not a running instance either.
            std::fs::write(&p, "1").unwrap();
            assert!(request_quit_at(&p, |_| called.set(true)));
            assert!(!called.get(), "a stale PID must never be signalled");
        }

        #[test]
        fn quit_signals_the_pid_that_holds_the_lock_and_waits_for_it() {
            let p = temp("running");
            let inst = std::cell::RefCell::new(Some(acquire_at(&p).unwrap()));
            let got = std::cell::Cell::new(0u32);
            // "Terminating" releases the lock, like a process exiting would.
            let ok = request_quit_at(&p, |pid| {
                got.set(pid);
                inst.borrow_mut().take();
            });
            assert!(ok);
            assert_eq!(got.get(), std::process::id());
        }

        #[test]
        fn quit_reports_failure_when_the_instance_does_not_go_away() {
            let p = temp("stuck");
            let _inst = acquire_at(&p).unwrap();
            // Shorten the wait: the signal does nothing, so the lock stays held.
            assert!(!request_quit_at(&p, |_| {}));
        }
    }
}

pub use imp::{acquire, request_quit};
