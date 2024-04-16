// TODO: Set up shared lib to use between drv-adapter and proxy workspaces
// - Standardize the BuildMetadata and OutputMetada structs in the shared lib, and use in both workspaces
// - Write BuildMetadata and send back to the client
// - fInd possible refrences to pass to the reference scanner
use std::{
    collections::{HashMap, HashSet},
    ffi::OsStr,
    io::{Cursor, Write},
    os::unix::ffi::OsStrExt,
    os::unix::fs::PermissionsExt,
    path::Path,
    process::{exit, Command},
};

use aho_corasick::AhoCorasick;

use ring::digest::{Context, Digest, SHA256};

use fs_extra::dir::CopyOptions;
use nix_remote::{
    nar::{DirectorySink, EntrySink, FileSink, Nar},
    serialize::Tee,
    worker_op::Derivation,
    NixString, NixWriteExt, StorePath,
};
use serde::Serialize;
use walkdir::WalkDir;

type NixHash = Vec<u8>;

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

struct HashSink {
    hash: Context,
    size: usize,
}

impl HashSink {
    fn new() -> Self {
        HashSink {
            hash: Context::new(&SHA256),
            size: 0,
        }
    }

    /// Return the digest of the hash.
    fn finish(self) -> (Digest, usize) {
        (self.hash.finish(), self.size)
    }
}

impl Write for HashSink {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.hash.update(buf);
        self.size += buf.len();
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

struct RefScanSink {
    hashes: AhoCorasick,
    hash_to_path: HashMap<NixHash, StorePath>,
    seen: HashSet<StorePath>,
    tail: Vec<u8>,
}

impl RefScanSink {
    fn new(possible_paths: impl IntoIterator<Item = StorePath>) -> RefScanSink {
        let hash_to_path: HashMap<_, _> = possible_paths
            .into_iter()
            .map(|path| (path.as_ref()[..32].to_owned(), path))
            .collect();
        RefScanSink {
            hashes: AhoCorasick::new(hash_to_path.keys()).unwrap(),
            hash_to_path,
            seen: HashSet::new(),
            tail: Vec::new(),
        }
    }

    fn scan(&mut self) {
        self.seen
            .extend(self.hashes.find_overlapping_iter(&self.tail).map(|m| {
                self.hash_to_path
                    .get(&self.tail[m.range()])
                    .unwrap()
                    .clone()
            }));
    }
}

impl Write for RefScanSink {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.tail.extend_from_slice(buf);
        self.scan();

        let tail_start = buf.len().saturating_sub(31);
        self.tail.drain(..tail_start);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

struct Wye<W1, W2> {
    w1: W1,
    w2: W2,
}

impl<W1, W2> Wye<W1, W2> {
    fn new(w1: W1, w2: W2) -> Self {
        Wye { w1, w2 }
    }
}

impl<W1: Write, W2: Write> Write for Wye<W1, W2> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.w1.write_all(buf)?;
        self.w2.write_all(buf)?;
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.w1.flush()?;
        self.w2.flush()?;
        Ok(())
    }
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

fn path_to_metadata(
    possible_references: impl IntoIterator<Item = StorePath>,
    path: impl AsRef<Path>,
) -> OutputMetadata {
    let mut hasher = HashSink::new();
    let mut ref_scanner = RefScanSink::new(possible_references);

    let mut out = Wye::new(ref_scanner, hasher);

    let mut nar = Nar::Target(NixString::from_bytes(b""));
    nar_from_filesystem(path, &mut nar);
    out.write_nix(&nar).unwrap();

    let (digest, nar_size) = hasher.finish();

    OutputMetadata {
        references: ref_scanner.seen.into_iter().collect(),
        nar_hash: NixString::from_bytes(digest.as_ref()),
        nar_size,
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

    let mut metadata = BuildMetadata {
        metadata: HashMap::new(),
    };
    for (key, out) in drv.outputs {
        // cp /nix/store/... nix/store
        let path: &OsStr = std::os::unix::ffi::OsStrExt::from_bytes(out.store_path.as_ref());
        dbg!(Command::new("cp")
            .arg("-r")
            .arg(path)
            .arg("nix/store/")
            .status()?);

        metadata
            .metadata
            .insert(key, path_to_metadata(todo!(), path));
    }

    if let Some(code) = status.code() {
        exit(code);
    } else {
        panic!("no status code");
    }
}
