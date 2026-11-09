//! Read commands against a mock API: the CLI's JSON answer to one request.

use assert_cmd::Command;
use assert_cmd::assert::Assert;
use serde_json::Value;
use wiremock::MockServer;

use super::{IsolatedHome, json_stdout};

impl IsolatedHome {
    /// [`Self::command_with_server`] with JSON output.
    pub fn json_command(&self, server: &MockServer) -> Command {
        let mut command = self.command_with_server(server);
        command.args(["--format", "json"]);
        command
    }
}

/// Run `args` in a fresh home against `server` with JSON output; the caller
/// says whether it must succeed.
pub fn read(server: &MockServer, args: &[&str]) -> Assert {
    IsolatedHome::new().json_command(server).args(args).assert()
}

/// [`read`], requiring success, and the JSON it printed.
pub fn read_json(server: &MockServer, args: &[&str]) -> Value {
    json_stdout(&read(server, args).success())
}
