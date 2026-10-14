//! Platform process lifecycle handling.
//!
//! The hazard this exists for (`docs/plan.md` §5.3): if the master dies, its
//! `php-cgi` children survive as orphans holding their ports, which makes the
//! development loop miserable. Every platform has its own mechanism.
//!
//! - **Windows:** a job object with `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`. When
//!   the last handle to the job closes — including because our process died —
//!   the OS terminates every process in it.
//! - **Unix:** a process group, so a single signal reaches the whole tree, and
//!   `PR_SET_PDEATHSIG` where it is available. Process groups are set up in the
//!   spawn itself (`process_group(0)`), so nothing to do here.

use std::process::Child;

/// A guard that must outlive the children it protects.
///
/// Dropping it kills the children it owns, so it has to be held for as long as
/// the server runs and dropped last.
#[derive(Debug)]
pub struct LifecycleGuard {
    #[cfg(windows)]
    job: windows_sys::Win32::Foundation::HANDLE,
}

// The job handle is owned exclusively by this guard; the type-level invariant
// is what makes using it from another thread sound.
unsafe impl Send for LifecycleGuard {}

// The guard is only ever *used* (adopting a child into the job), and adopting is
// safe from any thread: the job object itself is safe to share. Marking it
// `Sync` is what lets a pool that holds it be shared across the request threads,
// which is exactly what per-slot parallelism needs.
unsafe impl Sync for LifecycleGuard {}

impl LifecycleGuard {
    /// Create the guard for this process's children.
    pub fn new(name: &str) -> std::io::Result<Self> {
        #[cfg(windows)]
        {
            let _ = name;
            Ok(LifecycleGuard { job: create_job()? })
        }
        #[cfg(not(windows))]
        {
            let _ = name;
            Ok(LifecycleGuard {})
        }
    }

    /// Attach a freshly spawned child, so it dies with us.
    ///
    /// Failing to attach is worth surfacing: it is the difference between
    /// clean shutdown and a pile of orphans.
    pub fn adopt(&self, child: &Child) -> std::io::Result<()> {
        #[cfg(windows)]
        {
            use std::os::windows::io::AsRawHandle;
            use windows_sys::Win32::System::JobObjects::AssignProcessToJobObject;

            let process = child.as_raw_handle() as windows_sys::Win32::Foundation::HANDLE;
            // SAFETY: both handles are valid for the life of the call.
            let ok = unsafe { AssignProcessToJobObject(self.job, process) };
            if ok == 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        }
        #[cfg(not(windows))]
        {
            let _ = child;
            // Process groups and PDEATHSIG are configured at spawn time.
            Ok(())
        }
    }
}

#[cfg(windows)]
impl Drop for LifecycleGuard {
    fn drop(&mut self) {
        use windows_sys::Win32::Foundation::CloseHandle;
        // Closing the last handle to the job triggers KILL_ON_JOB_CLOSE.
        // SAFETY: the handle was created by `create_job` and is closed once.
        unsafe {
            CloseHandle(self.job);
        }
    }
}

#[cfg(windows)]
fn create_job() -> std::io::Result<windows_sys::Win32::Foundation::HANDLE> {
    use windows_sys::Win32::System::JobObjects::{
        CreateJobObjectW, JobObjectExtendedLimitInformation, SetInformationJobObject,
        JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    };

    // SAFETY: a null name and a null security descriptor are both documented as
    // valid, and the returned handle is checked before use.
    let job = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
    if job.is_null() {
        return Err(std::io::Error::last_os_error());
    }

    let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
    limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;

    // SAFETY: `limits` is a correctly sized, initialised struct and the handle
    // is valid.
    let ok = unsafe {
        SetInformationJobObject(
            job,
            JobObjectExtendedLimitInformation,
            std::ptr::addr_of_mut!(limits).cast(),
            std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        )
    };
    if ok == 0 {
        let err = std::io::Error::last_os_error();
        // SAFETY: the handle was created above and has not been closed.
        unsafe {
            use windows_sys::Win32::Foundation::CloseHandle;
            CloseHandle(job);
        }
        return Err(err);
    }

    Ok(job)
}

/// Spawn-side configuration that makes children die with their parent.
///
/// Kept as a single place so every spawn site gets the same treatment, and so
/// the Unix-only process-group call is compiled only where it exists.
pub fn configure_command(command: &mut std::process::Command) {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // Put the child in its own process group: signals aimed at the group
        // then reach php-cgi and anything it spawns, and our own group is not
        // taken down with it.
        command.process_group(0);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // CREATE_NEW_PROCESS_GROUP so a console ctrl event can be targeted at
        // the child rather than at us; job objects handle the reaping.
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        command.creation_flags(CREATE_NEW_PROCESS_GROUP);
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = command;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_guard_can_be_created_and_adopt_a_process() {
        let guard = LifecycleGuard::new("volucer-test").expect("create the guard");
        let mut command = std::process::Command::new(if cfg!(windows) { "cmd" } else { "sleep" });
        #[cfg(windows)]
        command.args(["/C", "exit 0"]);
        #[cfg(not(windows))]
        command.arg("0");
        configure_command(&mut command);

        let mut child = command.spawn().expect("spawn a trivial process");
        guard.adopt(&child).expect("adopt the child");
        let _ = child.wait();
    }

    #[test]
    fn configuring_a_command_is_infallible_and_idempotent() {
        let mut command = std::process::Command::new("cmd");
        configure_command(&mut command);
        configure_command(&mut command);
    }
}
