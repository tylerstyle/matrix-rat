//! A program started for a sound, playing or recording: in a process group of its own,
//! which is killed as a whole, and never outliving the app.
//!
//! It is killed, with everything it started, when its owner says so and when this is
//! dropped (the task was: the runtime shutting down, a panic that unwinds); the kernel
//! kills it when the app dies any other way (`PR_SET_PDEATHSIG`).

use std::io;
use std::process::ExitStatus;

/// A started program, the leader of its own process group.
pub(super) struct Group {
    /// Private: whoever waited for it behind this type's back would leave it sending
    /// signals to a number that may be another's by then.
    child: tokio::process::Child,
    /// Its process group, numbered as itself.
    group: libc::pid_t,
    /// It was waited for: its number may be another's from now on.
    reaped: bool,
}

impl Group {
    /// Starts `command` (its arguments and what it reads and writes are the caller's
    /// matter): no shell, in a process group of its own (the terminal's keys do not
    /// reach it, and it is killed with all it starts), and set to be killed by the
    /// kernel when this thread ends. The async threads live as long as the app does,
    /// so in effect: when the app does.
    pub fn spawn(mut command: std::process::Command) -> io::Result<Self> {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
        let parent = std::process::id() as libc::pid_t;
        // SAFETY: this runs in the new process between fork and exec, where only
        // async-signal-safe calls may be made: these are three system calls, and the
        // error built from the last one does not allocate.
        unsafe {
            command.pre_exec(move || {
                // All five arguments, as the call is declared to take them.
                let (signal, none) = (libc::SIGKILL as libc::c_ulong, 0 as libc::c_ulong);
                if libc::prctl(libc::PR_SET_PDEATHSIG, signal, none, none, none) == -1 {
                    return Err(io::Error::last_os_error());
                }
                // The app may have ended before that was in place.
                if libc::getppid() != parent {
                    libc::_exit(1);
                }
                Ok(())
            });
        }
        let child = tokio::process::Command::from(command)
            .kill_on_drop(true)
            .spawn()?;
        // Never zero or less: a signal to "minus that" would go to this app's own
        // group, or to everything the user runs.
        let group = child
            .id()
            .and_then(|id| libc::pid_t::try_from(id).ok())
            .filter(|group| *group > 0)
            .ok_or_else(|| io::Error::other("the program has no process number"))?;
        Ok(Self {
            child,
            group,
            reaped: false,
        })
    }

    /// The number of the program, which is its group's.
    #[cfg(test)]
    pub fn id(&self) -> libc::pid_t {
        self.group
    }

    /// Sends `signal` to the program and all it started, unless it was waited for.
    pub fn signal(&self, signal: libc::c_int) {
        if !self.reaped {
            // SAFETY: a plain system call. While the group's leader is not waited for,
            // its number is not given to another process.
            unsafe { libc::kill(-self.group, signal) };
        }
    }

    /// Kills the program and all it started.
    pub fn kill(&self) {
        self.signal(libc::SIGKILL);
    }

    /// What the program reads, once.
    pub fn take_stdin(&mut self) -> Option<tokio::process::ChildStdin> {
        self.child.stdin.take()
    }

    /// What the program writes, once.
    pub fn take_stdout(&mut self) -> Option<tokio::process::ChildStdout> {
        self.child.stdout.take()
    }

    /// Waits for the program's end, however it ends, and kills whatever it started
    /// and left behind in its group. Safe to drop half-way and to call again.
    pub async fn wait(&mut self) -> io::Result<ExitStatus> {
        let status = self.child.wait().await;
        if status.is_ok() && !self.reaped {
            self.reaped = true;
            // At once, with nothing awaited in between. While anything is left in the
            // group its number stays taken; if nothing is, the number is free from
            // here on, and that it is given out again within this instant is as good
            // as impossible (numbers are handed out in a long round).
            // SAFETY: a plain system call.
            unsafe { libc::kill(-self.group, libc::SIGKILL) };
        }
        status
    }

    /// Kills the program with all it started and waits for it.
    pub async fn put_down(&mut self) {
        self.kill();
        let _ = self.wait().await;
    }
}

impl Drop for Group {
    fn drop(&mut self) {
        // Dropped before its end (the task was): nothing of it stays.
        self.kill();
    }
}
