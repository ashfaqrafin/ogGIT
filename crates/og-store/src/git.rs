//! A thin wrapper around the `git` binary. Version 1 leaves pack work to Git
//! itself; Rust only decides what to trust.

use std::{
    ffi::OsStr,
    io::Write,
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
};

use anyhow::{Context, Result, bail};

/// Variables Git sets for hooks and remote helpers. Left in place they would
/// point our commands at the user's repo instead of the store.
const REPO_ENV: &[&str] = &[
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_INDEX_FILE",
    "GIT_NAMESPACE",
    "GIT_OBJECT_DIRECTORY",
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    "GIT_COMMON_DIR",
    "GIT_PREFIX",
    "GIT_CONFIG_PARAMETERS",
    "GIT_CONFIG_COUNT",
    "GIT_QUARANTINE_PATH",
    "GIT_SHALLOW_FILE",
    "GIT_GRAFT_FILE",
    "GIT_REPLACE_REF_BASE",
    "GIT_NO_REPLACE_OBJECTS",
    "GIT_IMPLICIT_WORK_TREE",
    "GIT_PROTOCOL",
];

pub fn clean_env(cmd: &mut Command) -> &mut Command {
    for var in REPO_ENV {
        cmd.env_remove(var);
    }
    cmd.env("GIT_TERMINAL_PROMPT", "0")
}

#[derive(Clone, Debug)]
pub struct Git {
    dir: PathBuf,
    env: Vec<(String, String)>,
}

impl Git {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into(), env: Vec::new() }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn with_env(mut self, key: &str, value: impl Into<String>) -> Self {
        self.env.push((key.to_string(), value.into()));
        self
    }

    pub fn command(&self) -> Command {
        let mut cmd = Command::new("git");
        clean_env(&mut cmd);
        cmd.arg("--git-dir").arg(&self.dir);
        for (k, v) in &self.env {
            cmd.env(k, v);
        }
        cmd.stdin(Stdio::null());
        cmd
    }

    pub fn output<I, S>(&self, args: I, input: Option<&[u8]>) -> Result<Output>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let args: Vec<_> = args.into_iter().map(|a| a.as_ref().to_owned()).collect();
        let mut cmd = self.command();
        cmd.args(&args).stdout(Stdio::piped()).stderr(Stdio::piped());
        if input.is_some() {
            cmd.stdin(Stdio::piped());
        }
        let mut child = cmd.spawn().context("failed to run git; is it installed?")?;
        // Feed stdin from another thread so a large input cannot deadlock
        // against a full stdout pipe.
        let writer = match (input, child.stdin.take()) {
            (Some(data), Some(mut stdin)) => {
                let data = data.to_vec();
                Some(std::thread::spawn(move || stdin.write_all(&data)))
            }
            _ => None,
        };
        let out = child.wait_with_output()?;
        if let Some(writer) = writer {
            // A command may exit early and close stdin; its exit status says
            // whether that was a failure.
            let _ = writer.join();
        }
        Ok(out)
    }

    /// Runs git and returns stdout, failing on a non-zero exit.
    pub fn run_bytes<I, S>(&self, args: I, input: Option<&[u8]>) -> Result<Vec<u8>>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let args: Vec<_> = args.into_iter().map(|a| a.as_ref().to_owned()).collect();
        let out = self.output(&args, input)?;
        if !out.status.success() {
            let stderr = String::from_utf8_lossy(&out.stderr);
            let shown: Vec<_> = args.iter().take(4).map(|a| a.to_string_lossy()).collect();
            bail!("git {} failed: {}", shown.join(" "), stderr.trim());
        }
        Ok(out.stdout)
    }

    pub fn run<I, S>(&self, args: I) -> Result<String>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let out = self.run_bytes(args, None)?;
        Ok(String::from_utf8(out).context("git printed non-UTF-8 output")?.trim_end().to_string())
    }

    /// Runs a predicate command: exit 0 is true, exit 1 is false, anything
    /// else is an error.
    pub fn test<I, S>(&self, args: I) -> Result<bool>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let out = self.output(args, None)?;
        match out.status.code() {
            Some(0) => Ok(true),
            Some(1) => Ok(false),
            _ => bail!("git failed: {}", String::from_utf8_lossy(&out.stderr).trim()),
        }
    }
}
