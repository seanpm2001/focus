use std::ffi::{OsStr, OsString};

use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::{
    sandbox::Sandbox,
    sandbox_command::{SandboxCommand, SandboxCommandOutput},
};

pub fn git_binary() -> OsString {
    OsString::from("git")
}

pub fn git_command(sandbox: &Sandbox) -> Result<(Command, SandboxCommand)> {
    SandboxCommand::new(git_binary(), sandbox)
}

pub fn write_config<P: AsRef<Path>>(
    repo_path: P,
    key: &str,
    val: &str,
    sandbox: &Sandbox,
) -> Result<()> {
    let (mut cmd, scmd) = git_command(&sandbox)?;
    scmd.ensure_success_or_log(
        cmd.current_dir(repo_path).arg("config").arg(key).arg(val),
        SandboxCommandOutput::Stderr,
        "git config",
    )
    .map(|_| ())
}

pub fn read_config<P: AsRef<Path>>(repo_path: P, key: &str, sandbox: &Sandbox) -> Result<String> {
    let (mut cmd, scmd) = git_command(&sandbox)?;
    let mut output_string = String::new();
    scmd.read_to_string(SandboxCommandOutput::Stdout, &mut output_string)
        .with_context(|| format!("reading config key {}", key))?;
    Ok(output_string)
}

pub fn run_git_command_consuming_stdout<I, S>(
    repo: &PathBuf,
    args: I,
    sandbox: &Sandbox,
) -> Result<String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let (mut cmd, scmd) = git_command(&sandbox)?;
    if let Err(e) = cmd.current_dir(repo).args(args).status() {
        scmd.log(
            crate::sandbox_command::SandboxCommandOutput::Stderr,
            &"failed 'git hash-object' command",
        )?;
        bail!("git failed: {}", e);
    }
    let mut stdout_contents = String::new();
    scmd.read_to_string(SandboxCommandOutput::Stdout, &mut stdout_contents)?;
    Ok(stdout_contents.trim().to_owned())
}
