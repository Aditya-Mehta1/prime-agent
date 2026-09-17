//! pa-core platform wall: every OS-specific behavior behind small traits in
//! cfg-gated modules (MISSION.md, Windows-readiness).
//!
//! Unix implementations today; Windows implementations slot in behind the
//! same signatures later (see docs/windows-readiness.md for the plan). Call
//! sites in the session engine never branch on `cfg` themselves.

pub mod lock_dir;
pub mod perms;
pub mod process;
pub mod shell;

pub use lock_dir::LockDir;
pub use perms::{
    file_mode, is_executable, is_readable_writable, restrict_dir, restrict_file, set_private_mode,
};
pub use process::{
    kill_pid, kill_process_group_or_pid, pid_exists, set_new_process_group, termination_signal,
    Signal,
};
pub use shell::{get_shell_config, resolve_kernel_bash_shell, ShellConfig};
