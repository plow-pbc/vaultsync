// The vaultsync engine (crates/ffi), linked into the app as a static library.
#pragma once

/// Runs one vaultsync command: `args` is a JSON array of strings, as typed
/// after `vaultsync`. Returns {"output": …} or {"error": …} as JSON; release
/// it with vs_free.
char *vs_command(const char *args);

/// Releases a reply from vs_command.
void vs_free(char *reply);
