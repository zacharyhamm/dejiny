use crate::db::{log_error, open_db};
use std::ffi::CString;
use std::os::unix::ffi::OsStrExt;
use std::ptr;

pub fn store(command: &str, exit_code: i32, start: &str, end: &str, cwd: &str) {
    if let Err(e) = spawn_store_helper(command, exit_code, start, end, cwd) {
        eprintln!("dejiny: store failed: {e}");
    }
}

pub fn store_internal(command: &str, exit_code: i32, start: &str, end: &str, cwd: &str) {
    if let Err(e) = store_impl(command, exit_code, start, end, cwd) {
        log_error(&format!("store failed: {e}"));
        std::process::exit(1);
    }
}

struct StoreExecSpec {
    program: CString,
    _argv: Vec<CString>,
    argv_ptrs: Vec<*const libc::c_char>,
}

fn spawn_store_helper(
    command: &str,
    exit_code: i32,
    start: &str,
    end: &str,
    cwd: &str,
) -> anyhow::Result<()> {
    let exec_spec = build_store_exec_spec(command, exit_code, start, end, cwd)?;
    match unsafe { nix::unistd::fork()? } {
        nix::unistd::ForkResult::Parent { .. } => Ok(()),
        nix::unistd::ForkResult::Child => {
            // After fork, keep to libc/syscall-style operations until exec.
            unsafe {
                if libc::setsid() < 0 {
                    child_fail_and_exit();
                }
                libc::execv(exec_spec.program.as_ptr(), exec_spec.argv_ptrs.as_ptr());
                child_fail_and_exit();
            }
        }
    }
}

fn build_store_exec_spec(
    command: &str,
    exit_code: i32,
    start: &str,
    end: &str,
    cwd: &str,
) -> anyhow::Result<StoreExecSpec> {
    let current_exe = std::env::current_exe()?;
    let program = CString::new(current_exe.as_os_str().as_bytes())?;

    let argv = vec![
        program.clone(),
        CString::new("store-internal")?,
        CString::new("--command")?,
        CString::new(command)?,
        CString::new("--exit-code")?,
        CString::new(exit_code.to_string())?,
        CString::new("--start")?,
        CString::new(start)?,
        CString::new("--end")?,
        CString::new(end)?,
        CString::new("--cwd")?,
        CString::new(cwd)?,
    ];
    let mut argv_ptrs = argv.iter().map(|arg| arg.as_ptr()).collect::<Vec<_>>();
    argv_ptrs.push(ptr::null());

    Ok(StoreExecSpec {
        program,
        _argv: argv,
        argv_ptrs,
    })
}

unsafe fn child_fail_and_exit() -> ! {
    const MSG: &[u8] = b"dejiny: store helper launch failed\n";
    unsafe {
        libc::write(libc::STDERR_FILENO, MSG.as_ptr().cast(), MSG.len());
        libc::_exit(1);
    }
}

fn store_impl(
    command: &str,
    exit_code: i32,
    start: &str,
    end: &str,
    cwd: &str,
) -> anyhow::Result<()> {
    let conn = open_db()?;

    let start: f64 = start.parse()?;
    let end: f64 = end.parse()?;
    let hostname = hostname::get()?.to_string_lossy().into_owned();

    conn.execute(
        "INSERT INTO commands (command, exit_code, start, end, cwd, hostname)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        rusqlite::params![command, exit_code, start, end, cwd, hostname],
    )?;

    Ok(())
}
