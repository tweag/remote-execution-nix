use std::{
    collections::HashMap,
    ffi::OsStr,
    fs::File,
    io::Cursor,
    os::unix::ffi::OsStrExt,
    os::unix::fs::PermissionsExt,
    path::Path,
    process::{exit, Command},
};

use fs_extra::dir::CopyOptions;
use nix_remote::{
    nar::{DirectorySink, EntrySink, FileSink, Nar},
    worker_op::Derivation,
    NixString, NixWriteExt, StorePath,
};
use serde::Serialize;
use walkdir::WalkDir;

#[derive(Serialize)]
struct OutputMetadata {
    references: Vec<StorePath>,
    nar_hash: NixString,
    nar_size: usize,
}

#[derive(Serialize)]
struct BuildMetadata {
    // The key is the output name, like "bin"
    metadata: HashMap<NixString, OutputMetadata>,
}

// TODO: clean up error handling, and move to the other crate
fn nar_from_filesystem<'a>(path: impl AsRef<Path>, sink: impl EntrySink<'a>) {
    let path = path.as_ref();
    let metadata = std::fs::symlink_metadata(path).unwrap();
    if metadata.is_dir() {
        let mut dir_sink = sink.become_directory();
        let entries: Result<Vec<_>, _> = std::fs::read_dir(path).unwrap().collect();
        let mut entries = entries.unwrap();
        entries.sort_by_key(|e| e.file_name());

        for entry in entries {
            let file_name = entry.file_name();
            let name = file_name.as_bytes();
            let entry_sink = dir_sink.create_entry(NixString::from_bytes(name));
            nar_from_filesystem(entry.path(), entry_sink);
        }
    } else if metadata.is_file() {
        let mut file_sink = sink.become_file();
        let executable = (metadata.permissions().mode() & 0o100) != 0;
        // TODO: make this streaming
        let contents = std::fs::read(path).unwrap(); // FIXME
        file_sink.set_executable(executable);
        file_sink.add_contents(&contents);
    } else if metadata.is_symlink() {
        let target = std::fs::read_link(path).unwrap();
        sink.become_symlink(NixString::from_bytes(target.into_os_string().as_bytes()));
    } else {
        panic!("not a dir, or a file, or a symlink")
    }
}

fn main() -> anyhow::Result<()> {
    // let args = std::env::args().collect::<Vec<_>>();
    // let path = &args[1];
    // let out_path = &args[2];

    // let mut out = File::create(out_path)?;

    // let mut nar = Nar::Target(NixString::from_bytes(b""));
    // nar_from_filesystem(path, &mut nar);
    // out.write_nix(&nar).unwrap();

    // Ok(())
    let drv = std::fs::read("root.drv")?;
    let drv: Derivation = bincode::deserialize_from(Cursor::new(drv))?;

    let cwd = std::env::current_dir()?;

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
