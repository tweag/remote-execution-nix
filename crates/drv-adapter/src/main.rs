use std::{
    ffi::OsStr,
    io::Cursor,
    path::Path,
    process::{exit, Command},
};

use fs_extra::dir::CopyOptions;
use nix_remote::worker_op::Derivation;

fn main() -> anyhow::Result<()> {
    let drv = std::fs::read("root.drv")?;
    let drv: Derivation = bincode::deserialize_from(Cursor::new(drv))?;

    eprintln!("getting cwd");
    let cwd = std::env::current_dir()?;

    dbg!(Command::new("/usr/bin/id").output()?);

    // We get dumped in the worker directory, which contains a nix directory that needs
    // to get moved to /
    if Path::new("/nix").is_symlink() {
        // is_symlink returns true even if the symlink is broken (which is what we expect if
        // it's left over from an old build)
        eprintln!("removing /nix");
        std::fs::remove_file("/nix")?; // it should be a symlink
    } else if Path::new("/nix").exists() {
        eprintln!("removing /nix recursively");
        std::fs::remove_dir_all("/nix")?;
    }
    dbg!(Command::new("ls").output()?);
    dbg!(Command::new("ls").arg("/").output()?);
    eprintln!("copy nix to /nix");

    // FIXME: this is failing with "no such file or directory"
    //fs_extra::dir::copy(dbg!(cwd.join("nix")), "/nix", &CopyOptions::new())?;
    // TODO: this is a hack, but fs_extra is being annoying
    dbg!(Command::new("cp").args(["-r", "./nix", "/"]).status()?);

    eprintln!("running cmd");
    let status = Command::new(dbg!(drv.builder))
        .env_clear()
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

    for (_, out) in drv.outputs {
        // cp /nix/store/... nix/store
        let path: &OsStr = std::os::unix::ffi::OsStrExt::from_bytes(out.store_path.as_ref());
        dbg!(Command::new("cp")
            .arg("-r")
            .arg(path)
            .arg("nix/store/")
            .status()?);
    }

    if let Some(code) = status.code() {
        exit(code);
    } else {
        panic!("no status code");
    }
}
