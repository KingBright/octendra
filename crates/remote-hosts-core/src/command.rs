//! Command profiles and validation.

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::ServerProtectionPolicy;

/// Command risk class.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CommandClass {
    /// Reads state without mutation.
    ReadOnly,
    /// Performs bounded build or local compute.
    Build,
    /// Sensitive or mutating command.
    Sensitive,
}

/// A structured command profile that can be exposed to agents.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct CommandProfile {
    /// Profile name.
    pub name: String,
    /// Program name.
    pub program: String,
    /// Fixed or templated argument list.
    pub args: Vec<String>,
    /// Command class.
    pub class: CommandClass,
    /// Timeout in seconds.
    pub timeout_seconds: u64,
    /// Maximum captured output bytes.
    pub output_limit_bytes: usize,
    /// Whether a TTY is required.
    pub requires_tty: bool,
}

/// Command validation error.
#[derive(Debug, Error, Eq, PartialEq)]
pub enum CommandValidationError {
    /// Program is empty.
    #[error("program must not be empty")]
    EmptyProgram,
    /// Program is not on the allowlist.
    #[error("program `{0}` is not allowed for this command class")]
    ProgramNotAllowed(String),
    /// Shell metacharacter was detected.
    #[error("argument `{0}` contains shell metacharacters; use structured arguments")]
    ShellMetacharacter(String),
    /// A shell script profile has an invalid executable or argument shape.
    #[error("invalid shell script profile: {0}")]
    InvalidShellProfile(String),
    /// Timeout is invalid.
    #[error("timeout must be between 1 and {max} seconds, got {actual}")]
    InvalidTimeout {
        /// Maximum timeout.
        max: u64,
        /// Actual timeout.
        actual: u64,
    },
    /// Output limit is invalid.
    #[error("output limit must be between 1 and {max} bytes, got {actual}")]
    InvalidOutputLimit {
        /// Maximum output bytes.
        max: usize,
        /// Actual output bytes.
        actual: usize,
    },
}

/// Public description of a built-in command profile.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct CommandProfileInfo {
    /// Stable profile name accepted by agent-facing tools.
    pub name: &'static str,
    /// Short description of the profile.
    pub description: &'static str,
    /// Command class.
    pub class: CommandClass,
    /// Example argument lists.
    pub examples: Vec<Vec<&'static str>>,
}

/// Error returned when a built-in command profile cannot be resolved.
#[derive(Debug, Error, Eq, PartialEq)]
pub enum CommandProfileResolutionError {
    /// Profile name is unknown.
    #[error("unknown command profile `{0}`")]
    UnknownProfile(String),
    /// Profile does not accept the supplied argument count.
    #[error("profile `{profile}` accepts at most {max} arguments, got {actual}")]
    TooManyArguments {
        /// Profile name.
        profile: String,
        /// Maximum accepted argument count.
        max: usize,
        /// Actual argument count.
        actual: usize,
    },
    /// Argument is not allowed for this profile.
    #[error("argument `{argument}` is not allowed for profile `{profile}`")]
    ArgumentNotAllowed {
        /// Profile name.
        profile: String,
        /// Rejected argument.
        argument: String,
    },
    /// Resolved command failed built-in validation.
    #[error(transparent)]
    Validation(#[from] CommandValidationError),
}

/// Built-in command profile catalog exposed to agents.
#[derive(Clone, Debug, Default)]
pub struct CommandProfileCatalog;

impl CommandProfileCatalog {
    /// Lists built-in profiles.
    pub fn list_builtin() -> Vec<CommandProfileInfo> {
        vec![
            CommandProfileInfo {
                name: "host.identity",
                description: "Print the remote hostname.",
                class: CommandClass::ReadOnly,
                examples: vec![vec![]],
            },
            CommandProfileInfo {
                name: "host.uname",
                description: "Print kernel and architecture information.",
                class: CommandClass::ReadOnly,
                examples: vec![vec!["-a"], vec!["-m"]],
            },
            CommandProfileInfo {
                name: "host.uptime",
                description: "Print uptime and load average.",
                class: CommandClass::ReadOnly,
                examples: vec![vec![]],
            },
            CommandProfileInfo {
                name: "host.whoami",
                description: "Print the effective remote user.",
                class: CommandClass::ReadOnly,
                examples: vec![vec![]],
            },
            CommandProfileInfo {
                name: "disk.usage",
                description: "Print filesystem usage with df.",
                class: CommandClass::ReadOnly,
                examples: vec![vec!["-h"], vec!["-h", "/"]],
            },
            CommandProfileInfo {
                name: "memory.free",
                description: "Print memory usage with free.",
                class: CommandClass::ReadOnly,
                examples: vec![vec!["-h"]],
            },
            CommandProfileInfo {
                name: "process.snapshot",
                description: "Print a process snapshot with ps aux.",
                class: CommandClass::ReadOnly,
                examples: vec![vec![]],
            },
            CommandProfileInfo {
                name: "gpu.nvidia_smi",
                description: "Print bounded NVIDIA GPU status.",
                class: CommandClass::ReadOnly,
                examples: vec![
                    vec!["-L"],
                    vec!["--query-gpu=name,utilization.gpu", "--format=csv"],
                ],
            },
            CommandProfileInfo {
                name: "service.status",
                description: "Print systemd service status for one unit.",
                class: CommandClass::ReadOnly,
                examples: vec![vec!["sshd.service"]],
            },
            CommandProfileInfo {
                name: "shell.posix",
                description: "Run a POSIX shell script through the existing pooled workspace connection.",
                class: CommandClass::Sensitive,
                examples: vec![vec!["set -e\nhostname\nuptime"]],
            },
            CommandProfileInfo {
                name: "shell.powershell",
                description: "Run a PowerShell script through the existing pooled workspace connection.",
                class: CommandClass::Sensitive,
                examples: vec![vec!["$ErrorActionPreference = 'Stop'\nGet-ComputerInfo"]],
            },
        ]
    }

    /// Resolves a built-in command profile or direct program invocation.
    ///
    /// # Errors
    ///
    /// Returns an error only when the profile name or script is empty or malformed.
    pub fn resolve_builtin(
        name: &str,
        args: Vec<String>,
        policy: &ServerProtectionPolicy,
    ) -> Result<CommandProfile, CommandProfileResolutionError> {
        if matches!(name, "shell.posix" | "shell.powershell") {
            return resolve_shell_builtin(name, args, policy);
        }
        let profile = match name {
            "host.identity" => profile(name, "hostname", args, policy),
            "host.uname" => {
                let args = if args.is_empty() {
                    vec!["-a".to_owned()]
                } else {
                    args
                };
                profile(name, "uname", args, policy)
            }
            "host.uptime" => profile(name, "uptime", args, policy),
            "host.whoami" => profile(name, "whoami", args, policy),
            "disk.usage" => profile(name, "df", args, policy),
            "memory.free" => {
                let args = if args.is_empty() {
                    vec!["-h".to_owned()]
                } else {
                    args
                };
                profile(name, "free", args, policy)
            }
            "process.snapshot" => {
                let args = if args.is_empty() {
                    vec!["aux".to_owned()]
                } else {
                    args
                };
                profile(name, "ps", args, policy)
            }
            "gpu.nvidia_smi" => {
                let args = if args.is_empty() {
                    vec!["-L".to_owned()]
                } else {
                    args
                };
                profile(name, "nvidia-smi", args, policy)
            }
            "service.status" => {
                let Some(service) = args.first() else {
                    return Err(CommandProfileResolutionError::ArgumentNotAllowed {
                        profile: name.to_owned(),
                        argument: "<missing-service>".to_owned(),
                    });
                };
                profile(
                    name,
                    "systemctl",
                    vec!["status".to_owned(), service.clone()],
                    policy,
                )
            }
            other => {
                if other.trim().is_empty() {
                    return Err(CommandProfileResolutionError::UnknownProfile(
                        other.to_owned(),
                    ));
                }
                // Treat any other non-empty profile name as a direct POSIX shell command or program
                // if it contains spaces/newlines, or a direct program invocation otherwise.
                if other.contains(char::is_whitespace) && args.is_empty() {
                    shell_profile(
                        "shell.posix",
                        "sh",
                        vec!["-lc".to_owned(), other.to_owned()],
                        policy,
                    )
                } else {
                    CommandProfile {
                        name: other.to_owned(),
                        program: other.to_owned(),
                        args,
                        class: CommandClass::Sensitive,
                        timeout_seconds: policy.default_exec_timeout_seconds.max(900),
                        output_limit_bytes: policy.default_output_limit_bytes.max(1024 * 1024),
                        requires_tty: false,
                    }
                }
            }
        };

        profile.validate()?;
        Ok(profile)
    }
}

fn resolve_shell_builtin(
    name: &str,
    args: Vec<String>,
    policy: &ServerProtectionPolicy,
) -> Result<CommandProfile, CommandProfileResolutionError> {
    let script = one_shell_script(name, args)?;
    let profile = match name {
        "shell.posix" => shell_profile(name, "sh", vec!["-lc".to_owned(), script], policy),
        "shell.powershell" => shell_profile(
            name,
            "powershell.exe",
            vec![
                "-NoLogo".to_owned(),
                "-NoProfile".to_owned(),
                "-NonInteractive".to_owned(),
                "-Command".to_owned(),
                script,
            ],
            policy,
        ),
        other => {
            return Err(CommandProfileResolutionError::UnknownProfile(
                other.to_owned(),
            ));
        }
    };
    profile.validate()?;
    Ok(profile)
}

impl CommandProfile {
    /// Validates basic structural sanity without artificial program allowlists or metacharacter blocks.
    ///
    /// # Errors
    ///
    /// Returns an error if the program is empty or timeout/output limits are zero or out of range.
    pub fn validate(&self) -> Result<(), CommandValidationError> {
        const MAX_TIMEOUT_SECONDS: u64 = 86_400;
        const MAX_OUTPUT_LIMIT_BYTES: usize = 64 * 1024 * 1024;

        if self.program.trim().is_empty() {
            return Err(CommandValidationError::EmptyProgram);
        }

        if matches!(self.name.as_str(), "shell.posix" | "shell.powershell") {
            validate_shell_profile(self)?;
        }

        if self.timeout_seconds == 0 || self.timeout_seconds > MAX_TIMEOUT_SECONDS {
            return Err(CommandValidationError::InvalidTimeout {
                max: MAX_TIMEOUT_SECONDS,
                actual: self.timeout_seconds,
            });
        }

        if self.output_limit_bytes == 0 || self.output_limit_bytes > MAX_OUTPUT_LIMIT_BYTES {
            return Err(CommandValidationError::InvalidOutputLimit {
                max: MAX_OUTPUT_LIMIT_BYTES,
                actual: self.output_limit_bytes,
            });
        }

        Ok(())
    }
}

fn profile(
    name: &str,
    program: &str,
    args: Vec<String>,
    policy: &ServerProtectionPolicy,
) -> CommandProfile {
    CommandProfile {
        name: name.to_owned(),
        program: program.to_owned(),
        args,
        class: CommandClass::ReadOnly,
        timeout_seconds: policy.default_exec_timeout_seconds,
        output_limit_bytes: policy.default_output_limit_bytes,
        requires_tty: false,
    }
}

fn shell_profile(
    name: &str,
    program: &str,
    args: Vec<String>,
    policy: &ServerProtectionPolicy,
) -> CommandProfile {
    CommandProfile {
        name: name.to_owned(),
        program: program.to_owned(),
        args,
        class: CommandClass::Sensitive,
        timeout_seconds: policy.default_exec_timeout_seconds.max(900),
        output_limit_bytes: policy.default_output_limit_bytes.max(1024 * 1024),
        requires_tty: false,
    }
}

fn one_shell_script(
    name: &str,
    args: Vec<String>,
) -> Result<String, CommandProfileResolutionError> {
    if args.is_empty() {
        return Err(CommandProfileResolutionError::ArgumentNotAllowed {
            profile: name.to_owned(),
            argument: "<missing-script>".to_owned(),
        });
    }
    let script = if args.len() == 1 {
        args.into_iter().next().unwrap_or_default()
    } else {
        args.join("\n")
    };
    if script.trim().is_empty() || script.contains('\0') || script.len() > 16 * 1024 * 1024 {
        return Err(CommandProfileResolutionError::ArgumentNotAllowed {
            profile: name.to_owned(),
            argument: "<invalid-script>".to_owned(),
        });
    }
    Ok(script)
}

fn validate_shell_profile(profile: &CommandProfile) -> Result<(), CommandValidationError> {
    let script = match profile.name.as_str() {
        "shell.posix"
            if profile.program == "sh"
                && profile.args.len() == 2
                && profile.args.first().is_some_and(|arg| arg == "-lc") =>
        {
            profile.args.get(1)
        }
        "shell.powershell"
            if profile.program == "powershell.exe"
                && profile.args.len() == 5
                && profile.args[..4]
                    == ["-NoLogo", "-NoProfile", "-NonInteractive", "-Command"] =>
        {
            profile.args.get(4)
        }
        _ => {
            return Err(CommandValidationError::InvalidShellProfile(
                "expected shell.posix or shell.powershell with fixed launcher arguments".to_owned(),
            ));
        }
    };
    if script.is_none_or(|script| {
        script.trim().is_empty() || script.contains('\0') || script.len() > 16 * 1024 * 1024
    }) {
        return Err(CommandValidationError::InvalidShellProfile(
            "script must be non-empty, contain no NUL, and be at most 16 MiB".to_owned(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{CommandClass, CommandProfile, CommandProfileCatalog, CommandProfileResolutionError};
    use crate::ServerProtectionPolicy;

    fn readonly_profile(program: &str, args: Vec<String>) -> CommandProfile {
        CommandProfile {
            name: "test".to_owned(),
            program: program.to_owned(),
            args,
            class: CommandClass::ReadOnly,
            timeout_seconds: 30,
            output_limit_bytes: 1024,
            requires_tty: false,
        }
    }

    #[test]
    fn accepts_structured_and_direct_commands_without_allowlist_blocks() {
        let profile = readonly_profile("nvidia-smi", vec!["--query-gpu=name".to_owned()]);
        assert_eq!(profile.validate(), Ok(()));

        let custom = readonly_profile("htop", vec!["-d".to_owned(), "10".to_owned()]);
        assert_eq!(custom.validate(), Ok(()));
    }

    #[test]
    fn resolves_builtin_profile_with_defaults() -> Result<(), CommandProfileResolutionError> {
        let profile = CommandProfileCatalog::resolve_builtin(
            "host.uname",
            Vec::new(),
            &ServerProtectionPolicy::default(),
        )?;

        assert_eq!(profile.program, "uname");
        assert_eq!(profile.args, vec!["-a"]);
        Ok(())
    }

    #[test]
    fn resolves_posix_shell_script_for_real_workspace_operations()
    -> Result<(), CommandProfileResolutionError> {
        let script = "set -euo pipefail\nkubectl get pods | head -n 20";
        let profile = CommandProfileCatalog::resolve_builtin(
            "shell.posix",
            vec![script.to_owned()],
            &ServerProtectionPolicy::default(),
        )?;

        assert_eq!(profile.program, "sh");
        assert_eq!(profile.args, vec!["-lc", script]);
        assert_eq!(profile.class, CommandClass::Sensitive);
        profile.validate()?;
        Ok(())
    }
}
