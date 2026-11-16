//! The engine's commands for the Mac app, which links this static library:
//! the same `vaultsync` commands the command-line tool runs, in-process.

use std::ffi::{CStr, CString, c_char};

/// Runs one vaultsync command. `args` is a JSON array of strings, as typed
/// after `vaultsync`. Returns `{"output": …}` or `{"error": …}` as JSON, to
/// be released with `vs_free`. `run --all` returns only if the engine stops.
///
/// # Safety
/// `args` must be a valid NUL-terminated string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vs_command(args: *const c_char) -> *mut c_char {
    // SAFETY: the caller passes a valid C string.
    let args = unsafe { CStr::from_ptr(args) }.to_string_lossy();
    // A panic must not unwind into Swift: report it as an error instead.
    let result = std::panic::catch_unwind(|| {
        serde_json::from_str::<Vec<String>>(&args)
            .map_err(|e| format!("bad arguments: {e}"))
            .and_then(|args| vaultsync_engine::cli::run(&args).map_err(|e| format!("{e:#}")))
    })
    .unwrap_or_else(|_| Err("the engine crashed".to_owned()));
    let reply = match result {
        Ok(output) => serde_json::json!({ "output": output }),
        Err(error) => serde_json::json!({ "error": error }),
    };
    CString::new(reply.to_string())
        .expect("JSON has no NUL")
        .into_raw()
}

/// Releases a reply from `vs_command`.
///
/// # Safety
/// `reply` must come from `vs_command` and be released once.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn vs_free(reply: *mut c_char) {
    // SAFETY: the caller passes a reply from vs_command, once.
    drop(unsafe { CString::from_raw(reply) });
}
