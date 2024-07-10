// use eyre::{bail, eyre, Context};
use eyre::{bail, eyre};
use std::{
    env::consts::{DLL_PREFIX, DLL_SUFFIX},
    ffi::OsStr,
    path::{Path, PathBuf},
    str::FromStr,
};

pub use dora_message as message;

pub mod config;
pub mod coordinator_messages;
pub mod daemon_messages;
pub mod descriptor;
pub mod topics;

pub fn adjust_shared_library_path(path: &Path) -> Result<std::path::PathBuf, eyre::ErrReport> {
    let file_name = path
        .file_name()
        .ok_or_else(|| eyre!("shared library path has no file name"))?
        .to_str()
        .ok_or_else(|| eyre!("shared library file name is not valid UTF8"))?;
    if file_name.starts_with("lib") {
        bail!("Shared library file name must not start with `lib`, prefix is added automatically");
    }
    if path.extension().is_some() {
        bail!("Shared library file name must have no extension, it is added automatically");
    }

    let library_filename = format!("{DLL_PREFIX}{file_name}{DLL_SUFFIX}");

    let path = path.with_file_name(library_filename);
    Ok(path)
}

// Search for python binary.
// Match `python` for windows and `python3` for other platforms.
pub fn get_python_path() -> Result<std::path::PathBuf, eyre::ErrReport> {
    // let python = if cfg!(windows) {
    //     which::which("python")
    //         .context("failed to find `python` or `python3`. Make sure that python is available.")?
    // } else {
    //     which::which("python3")
    //         .context("failed to find `python` or `python3`. Make sure that python is available.")?
    // };
    // Ok(python)
    Ok(PathBuf::from_str("no python")?)
}

// Search for pip binary.
// First search for `pip3` as for ubuntu <20, `pip` can resolves to `python2,7 -m pip`
// Then search for `pip`, this will resolve for windows to python3 -m pip.
pub fn get_pip_path() -> Result<std::path::PathBuf, eyre::ErrReport> {
    // let python = match which::which("pip3") {
    //     Ok(python) => python,
    //     Err(_) => which::which("pip")
    //         .context("failed to find `pip3` or `pip`. Make sure that python is available.")?,
    // };
    // Ok(python)
    Ok(PathBuf::from_str("no pip")?)
}

// Helper function to run a program
pub async fn run<S>(_program: S, _args: &[&str], _pwd: Option<&Path>) -> eyre::Result<()>
where
    S: AsRef<OsStr>,
{
    // let mut run = tokio::process::Command::new(program);
    // run.args(args);

    // if let Some(pwd) = pwd {
    //     run.current_dir(pwd);
    // }
    // if !run.status().await?.success() {
    //     eyre::bail!("failed to run {args:?}");
    // };

    // Ok(())
    unimplemented!()
}
