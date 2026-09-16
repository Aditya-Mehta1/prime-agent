//! Package command validation and help, ported from
//! `package-manager-cli.ts` (`handlePackageCommand`, `parsePackageCommand`,
//! `printPackageCommandHelp`).

use crate::config::{APP_NAME, CONFIG_DIR_NAME};

use crate::public_command::{
    DAEMON_UPDATE_RESTART_COORDINATOR_FLAG, DAEMON_UPDATE_RESTART_ORIGIN_FLAG,
    DAEMON_UPDATE_RESTART_STATUS_FLAG,
};

/// Result of running a package command: printed output is handled here, and the
/// exit code is reported for the caller to propagate.
#[derive(Debug, Clone)]
pub struct PackageCommandOutcome {
    pub exit_code: Option<i32>,
}

const HANDLED_OK: PackageCommandOutcome = PackageCommandOutcome { exit_code: None };

fn fail(message: &str, hint: Option<&str>) -> PackageCommandOutcome {
    // handlePackageCommand prints its errors without the "Error: " prefix.
    eprintln!("{message}");
    if let Some(hint) = hint {
        eprintln!("{hint}");
    }
    PackageCommandOutcome { exit_code: Some(1) }
}

fn is_self_update_source(source: &str) -> bool {
    source == "self" || source == "pi" || source == APP_NAME
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PackageCommand {
    Install,
    Remove,
    Update,
    List,
}

impl PackageCommand {
    fn usage(&self) -> String {
        match self {
            PackageCommand::Install => format!("{APP_NAME} package install <source> [--local]"),
            PackageCommand::Remove => format!("{APP_NAME} package remove <source> [--local]"),
            PackageCommand::Update => format!(
                "{APP_NAME} update [--force] [--rollback] [--nightly|--stable] or {APP_NAME} package update [source]"
            ),
            PackageCommand::List => format!("{APP_NAME} package list"),
        }
    }
}

#[derive(Debug, Default)]
struct PackageCommandOptions {
    local: bool,
    help: bool,
    rollback: bool,
    invalid_option: Option<String>,
    invalid_argument: Option<String>,
    missing_option_value: Option<String>,
    conflicting_options: Option<String>,
    source: Option<String>,
    restart_coordinator: bool,
    restart_status_path: Option<String>,
    restart_origin_active_session_id: Option<String>,
}

fn parse_package_command(args: &[String]) -> Option<PackageCommandOptions> {
    let command = match args.first().map(String::as_str) {
        Some("uninstall") => Some(PackageCommand::Remove),
        Some("install") => Some(PackageCommand::Install),
        Some("remove") => Some(PackageCommand::Remove),
        Some("update") => Some(PackageCommand::Update),
        Some("list") => Some(PackageCommand::List),
        _ => None,
    }?;
    let rest = &args[1..];
    let mut options = PackageCommandOptions::default();
    let mut channel: Option<&str> = None;
    let mut self_flag = false;
    let mut extensions_flag = false;
    let mut extension_flag_source: Option<String> = None;
    let mut daemon_socket_seen = false;

    let mut index = 0;
    while index < rest.len() {
        let arg = rest[index].as_str();
        match arg {
            "-h" | "--help" => {
                options.help = true;
            }
            "--local" => {
                if matches!(command, PackageCommand::Install | PackageCommand::Remove) {
                    options.local = true;
                } else {
                    options
                        .invalid_option
                        .get_or_insert_with(|| arg.to_string());
                }
            }
            "--self" => {
                if command == PackageCommand::Update {
                    self_flag = true;
                } else {
                    options
                        .invalid_option
                        .get_or_insert_with(|| arg.to_string());
                }
            }
            "--extensions" => {
                if command == PackageCommand::Update {
                    extensions_flag = true;
                } else {
                    options
                        .invalid_option
                        .get_or_insert_with(|| arg.to_string());
                }
            }
            "--force" => {
                if command != PackageCommand::Update {
                    options
                        .invalid_option
                        .get_or_insert_with(|| arg.to_string());
                }
            }
            "--rollback" => {
                if command == PackageCommand::Update {
                    options.rollback = true;
                    self_flag = true;
                } else {
                    options
                        .invalid_option
                        .get_or_insert_with(|| arg.to_string());
                }
            }
            "--nightly" | "--stable" => {
                if command != PackageCommand::Update {
                    options
                        .invalid_option
                        .get_or_insert_with(|| arg.to_string());
                    index += 1;
                    continue;
                }
                let requested = if arg == "--nightly" {
                    "nightly"
                } else {
                    "stable"
                };
                if channel.is_some() && channel != Some(requested) {
                    options.conflicting_options.get_or_insert_with(|| {
                        "--nightly and --stable cannot be combined".to_string()
                    });
                }
                channel = Some(requested);
            }
            "--daemon-socket" => {
                if command != PackageCommand::Update {
                    options
                        .invalid_option
                        .get_or_insert_with(|| arg.to_string());
                    index += 1;
                    continue;
                }
                match rest.get(index + 1) {
                    Some(value) if !value.starts_with('-') => {
                        if daemon_socket_seen {
                            options.conflicting_options.get_or_insert_with(|| {
                                "--daemon-socket can only be provided once".to_string()
                            });
                        } else {
                            daemon_socket_seen = true;
                        }
                        index += 1;
                    }
                    _ => {
                        options
                            .missing_option_value
                            .get_or_insert_with(|| arg.to_string());
                    }
                }
            }
            DAEMON_UPDATE_RESTART_COORDINATOR_FLAG => {
                if command == PackageCommand::Update {
                    options.restart_coordinator = true;
                } else {
                    options
                        .invalid_option
                        .get_or_insert_with(|| arg.to_string());
                }
            }
            DAEMON_UPDATE_RESTART_STATUS_FLAG | DAEMON_UPDATE_RESTART_ORIGIN_FLAG => {
                if command != PackageCommand::Update {
                    options
                        .invalid_option
                        .get_or_insert_with(|| arg.to_string());
                    index += 1;
                    continue;
                }
                match rest.get(index + 1) {
                    Some(value) if !value.starts_with('-') => {
                        if arg == DAEMON_UPDATE_RESTART_STATUS_FLAG {
                            options.restart_status_path = Some(value.clone());
                        } else {
                            options.restart_origin_active_session_id = Some(value.clone());
                        }
                        index += 1;
                    }
                    _ => {
                        options
                            .missing_option_value
                            .get_or_insert_with(|| arg.to_string());
                    }
                }
            }
            "--extension" => {
                if command != PackageCommand::Update {
                    options
                        .invalid_option
                        .get_or_insert_with(|| arg.to_string());
                    index += 1;
                    continue;
                }
                match rest.get(index + 1) {
                    Some(value) if !value.starts_with('-') => {
                        if extension_flag_source.is_some() {
                            options.conflicting_options.get_or_insert_with(|| {
                                "--extension can only be provided once".to_string()
                            });
                        } else {
                            extension_flag_source = Some(value.clone());
                        }
                        index += 1;
                    }
                    _ => {
                        options
                            .missing_option_value
                            .get_or_insert_with(|| arg.to_string());
                    }
                }
            }
            _ if arg.starts_with('-') => {
                options
                    .invalid_option
                    .get_or_insert_with(|| arg.to_string());
            }
            _ => {
                if options.source.is_none() {
                    options.source = Some(arg.to_string());
                } else {
                    options
                        .invalid_argument
                        .get_or_insert_with(|| arg.to_string());
                }
            }
        }
        index += 1;
    }

    if command == PackageCommand::Update {
        if extension_flag_source.is_some() {
            if self_flag || extensions_flag {
                options.conflicting_options.get_or_insert_with(|| {
                    "--extension cannot be combined with --self or --extensions".to_string()
                });
            }
            if options.source.is_some() {
                options.conflicting_options.get_or_insert_with(|| {
                    "--extension cannot be combined with a positional source".to_string()
                });
            }
        } else if let Some(source) = &options.source {
            if !is_self_update_source(source) && (extensions_flag || self_flag) {
                options.conflicting_options.get_or_insert_with(|| {
                    "positional update targets cannot be combined with --self or --extensions"
                        .to_string()
                });
            }
        }
        let positional_source_not_self = options
            .source
            .as_deref()
            .is_some_and(|source| !is_self_update_source(source));
        if options.rollback
            && (extensions_flag || extension_flag_source.is_some() || positional_source_not_self)
        {
            options.conflicting_options =
                Some("--rollback only applies to Prime Agent itself".to_string());
        }
        if channel.is_some()
            && (extensions_flag || extension_flag_source.is_some() || positional_source_not_self)
        {
            options.conflicting_options.get_or_insert_with(|| {
                "--nightly and --stable only apply to Prime Agent itself".to_string()
            });
        }
    }

    Some(options)
}

fn print_package_command_help(command: PackageCommand) {
    let usage = command.usage();
    match command {
        PackageCommand::Install => println!(
            "Usage:\n  {usage}\n\nInstall a package and add it to settings.\n\nOptions:\n  --local    Install project-locally ({CONFIG_DIR_NAME}/settings.json)\n\nExamples:\n  {APP_NAME} package install npm:@foo/bar\n  {APP_NAME} package install git:github.com/user/repo\n  {APP_NAME} package install git:git@github.com:user/repo\n  {APP_NAME} package install https://github.com/user/repo\n  {APP_NAME} package install ssh://git@github.com/user/repo\n  {APP_NAME} package install ./local/path\n"
        ),
        PackageCommand::Remove => println!(
            "Usage:\n  {usage}\n\nRemove a package and its source from settings.\n\nOptions:\n  --local    Remove from project settings ({CONFIG_DIR_NAME}/settings.json)\n\nExamples:\n  {APP_NAME} package remove npm:@foo/bar\n"
        ),
        PackageCommand::Update => println!(
            "Usage:\n  {usage}\n\nUpdate {APP_NAME} or installed packages.\n\nOptions:\n  --self                  Update {APP_NAME} only\n  --extensions            Update installed packages only\n  --extension <source>    Update one package only\n  --force                 Reinstall {APP_NAME} even if the current version is latest\n  --rollback              Restore the previous compiled release\n  --nightly               Switch updates to the nightly channel (unreleased builds, may be broken)\n  --stable                Return updates to the stable channel\n  --daemon-socket <path>  Restart the daemon listening on this exact socket\n\nCommands:\n  {APP_NAME} update                Update {APP_NAME}\n  {APP_NAME} package update        Update installed packages\n  {APP_NAME} package update <source> Update one package\n"
        ),
        PackageCommand::List => println!(
            "Usage:\n  {usage}\n\nList installed packages from user and project settings.\n"
        ),
    }
}

/// Run a package command, mirroring `handlePackageCommand` up to the point
/// where the package manager subsystem is needed; those paths produce a clear
/// typed error instead.
pub fn handle_package_command(args: &[String]) -> PackageCommandOutcome {
    let Some(options) = parse_package_command(args) else {
        return PackageCommandOutcome { exit_code: None };
    };
    let command = match args.first().map(String::as_str) {
        Some("uninstall") | Some("remove") => PackageCommand::Remove,
        Some("install") => PackageCommand::Install,
        Some("update") => PackageCommand::Update,
        Some("list") => PackageCommand::List,
        _ => return PackageCommandOutcome { exit_code: None },
    };
    let command_name = match command {
        PackageCommand::Install => "install",
        PackageCommand::Remove => "remove",
        PackageCommand::Update => "update",
        PackageCommand::List => "list",
    };

    if options.help {
        print_package_command_help(command);
        return HANDLED_OK;
    }

    if let Some(invalid_option) = &options.invalid_option {
        if invalid_option == "-l"
            && matches!(command, PackageCommand::Install | PackageCommand::Remove)
        {
            return fail("Option -l was removed. Use \"--local\".", None);
        }
        return fail(
            &format!("Unknown option {invalid_option} for \"{command_name}\"."),
            Some(&format!(
                "Use \"{APP_NAME} --help\" or \"{}\".",
                command.usage()
            )),
        );
    }
    if let Some(missing) = &options.missing_option_value {
        return fail(
            &format!("Missing value for {missing}."),
            Some(&format!("Usage: {}", command.usage())),
        );
    }
    if let Some(invalid_argument) = &options.invalid_argument {
        return fail(
            &format!("Unexpected argument {invalid_argument}."),
            Some(&format!("Usage: {}", command.usage())),
        );
    }
    if let Some(conflict) = &options.conflicting_options {
        return fail(conflict, Some(&format!("Usage: {}", command.usage())));
    }
    if options.restart_coordinator {
        // A valid coordinator invocation is validated against the daemon
        // update-restart directory before running; that needs the daemon
        // update machinery, which is not linked into this build.
        return fail(
            "daemon update restart coordination is not available in this build yet",
            None,
        );
    }
    if options.restart_status_path.is_some() || options.restart_origin_active_session_id.is_some() {
        return fail(
            "Invalid daemon update restart coordinator invocation.",
            None,
        );
    }

    let source_missing = matches!(command, PackageCommand::Install | PackageCommand::Remove)
        && options.source.is_none();
    if source_missing {
        return fail(
            &format!("Missing {command_name} source."),
            Some(&format!("Usage: {}", command.usage())),
        );
    }

    // Everything past this point needs the package manager subsystem.
    fail(
        &format!(
            "the \"{command_name}\" package operation needs the package manager subsystem, which is not available in this build yet"
        ),
        None,
    )
}
