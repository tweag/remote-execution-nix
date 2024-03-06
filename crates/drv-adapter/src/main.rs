use std::{
    io::Cursor,
    path::Path,
    process::{exit, Command},
};

use nix_remote::worker_op::Derivation;

fn main() -> anyhow::Result<()> {
    let drv = std::fs::read("root.drv")?;
    let drv: Derivation = bincode::deserialize_from(Cursor::new(drv))?;

    eprintln!("getting cwd");
    let cwd = std::env::current_dir()?;

    // We get dumped in the worker directory, which contains a nix directory that needs
    // to get moved to /
    if Path::new("/nix").try_exists()? {
        eprintln!("removing /nix");
        std::fs::remove_dir_all("/nix")?;
    }
    eprintln!("linking nix to /nix");
    std::os::unix::fs::symlink(cwd.join("nix"), "/nix")?;

    eprintln!("running cmd");
    let status = Command::new(dbg!(drv.builder))
        .args(dbg!(drv.args.paths))
        .env("PATH", "/path-not-set")
        .env("HOME", "/homeless-shelter")
        .env("NIX_STORE", "/nix/store")
        .env("NIX_BUILD_CORES", "1")
        .env("NIX_BUILD_TOP", &cwd)
        .env("TMPDIR", &cwd)
        .env("TEMPDIR", &cwd)
        .env("TMP", &cwd)
        .env("TEMP", &cwd)
        .env("PWD", &cwd)
        .env("NIX_LOG_FD", "2")
        .env("TERM", "xterm-256color")
        .envs(dbg!(drv.env))
        .status()?;

    if let Some(code) = status.code() {
        exit(code);
    } else {
        panic!("no status code");
    }
}
