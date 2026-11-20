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
    pub child: tokio::process::Child,
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
                if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL as libc::c_ulong) == -1 {
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
        let group = child
            .id()
            .map(|id| id as libc::pid_t)
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

    /// Waits for the program's end. Safe to drop half-way and to call again.
    pub async fn wait(&mut self) -> io::Result<ExitStatus> {
        let status = self.child.wait().await;
        self.reaped = true;
        status
    }

    /// Kills whatever the program started and left behind in its group after its own
    /// end, if anything: the group's number stays taken while anything is in it.
    pub fn sweep(&self) {
        // SAFETY: a plain system call.
        unsafe { libc::kill(-self.group, libc::SIGKILL) };
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
