// TODO:
// - register built paths also. Don't forget to wrap in nix/store/...

// TODO:
// - write a EntrySink impl for nix.write somehow, this will facilitate some streaming
// - basic sending of the nar tree works, but we aren't streaming from or to nix
// - (bonus: pack small files into batch requests)
// - figure out how authentication is going to work. It doesn't seem to be defined as
//   part of the REv2 protocol, so services like buildbuddy seem to roll their own.
//   For now, we're using a locally-running buildbarn instance.
// - stream the worker's stderr and its stdout

// TODO(eventually): make nar generic over the file contents to make the hydrating nicer

// Known issues:
// - the fuse runner fails in unpackPhase because cp can't preserve permissions. Maybe set things up in /build instead? For now, hardlinking works

// There was a choice to be made: do we store the files/trees in CAS, or do we store nars. Current strategy is storing files, and packing/unpacking nars in the proxy.
// When returning the build result, we need to
// - return a nar
// - know all the references.
// Our approach: build the nar on the fly and stream it. Have the builder precompute references and store them somewhere in the CAS.

use std::collections::{HashMap, HashSet};
use std::io::{Cursor, Write};
use std::pin::Pin;

use anyhow::anyhow;
use futures::{Future, StreamExt};
use nix_remote::framed_data::FramedData;
use nix_remote::nar::{DirectorySink, EntrySink, FileSink, NarDirectoryEntry, NarFile};
use nix_remote::worker_op::{
    BuildResult, Derivation, DrvOutputs, QueryPathInfoResponse, ValidPathInfo, WorkerOp,
};
use nix_remote::{nar::Nar, NixProxy, NixReadExt, NixWriteExt};
use nix_remote::{stderr, NarHash, NixString, StorePath, StorePathSet, ValidPathInfoWithPath};
use nix_rev2::{
    blob, blob_cloned, blob_request, default_properties, digest, download_blob, download_proto,
    protoblob, upload_blob, upload_proto, Blob, TypedDigest,
};
use prost::Message;

use serde::{Deserialize, Serialize};
use tonic::transport::{Channel, Endpoint};

use nix_rev2::generated::{
    self,
    build::bazel::remote::execution::v2::{
        action_cache_client::ActionCacheClient,
        content_addressable_storage_client::ContentAddressableStorageClient,
        execution_client::ExecutionClient, Action, ActionResult, Command, Digest, Directory,
        DirectoryNode, ExecuteRequest, ExecuteResponse, ExecutedActionMetadata, FileNode,
        FindMissingBlobsRequest, GetActionResultRequest, OutputDirectory, OutputFile, SymlinkNode,
        Tree, UpdateActionResultRequest,
    },
    google::bytestream::byte_stream_client::ByteStreamClient,
};

//const KEY = env!("BUILDBUDDY_API_KEY");
#[derive(Debug, PartialEq, Eq, PartialOrd, Ord, Clone, Copy)]
pub enum PathType {
    File { executable: bool },
    Directory,
}

#[derive(Debug, Clone)]
pub enum PathInfo {
    File {
        digest: Digest,
        executable: bool,
    },
    Directory {
        root_digest: TypedDigest<Directory>,
        tree_digest: TypedDigest<Tree>,
    },
}

impl PathInfo {
    pub fn is_file(&self) -> bool {
        match self {
            PathInfo::File { .. } => true,
            PathInfo::Directory { .. } => false,
        }
    }
}

pub async fn path_info_from_tree(
    bs_client: &mut ByteStreamClient<Channel>,
    tree_digest: TypedDigest<Tree>,
) -> anyhow::Result<PathInfo> {
    let tree: Tree = download_proto(bs_client, &tree_digest).await?;
    let root_digest = TypedDigest::from_message(&tree.root.unwrap());

    Ok(PathInfo::Directory {
        root_digest,
        tree_digest,
    })
}

pub fn convert(nar: &Nar) -> (Vec<Blob>, PathInfo) {
    let mut blobs = Vec::new();

    fn convert_file_rec(file: &NarFile, acc: &mut Vec<Blob>) -> Digest {
        let (blob, digest) = blob_cloned(&file.contents);
        acc.push(blob);
        digest
    }

    fn convert_dir_rec(
        entries: &[NarDirectoryEntry],
        acc: &mut Vec<Blob>,
        dir_acc: &mut Vec<Directory>,
    ) -> (Directory, TypedDigest<Directory>) {
        let mut files = Vec::new();
        let mut symlinks = Vec::new();
        let mut directories = Vec::new();
        for entry in entries {
            // FIXME: proper error for non-utf8 names. The protocol apparently doesn't support non-utf8 names.
            let name = entry.name.to_string().unwrap();
            match &entry.node {
                Nar::Contents(file) => {
                    let digest = convert_file_rec(file, acc);
                    let properties = default_properties(file.executable);
                    let node = FileNode {
                        name,
                        digest: Some(digest),
                        is_executable: file.executable,
                        node_properties: Some(properties),
                    };
                    files.push(node);
                }
                Nar::Target(sym) => {
                    let target = sym.to_string().unwrap();
                    let properties = default_properties(true);
                    let node = SymlinkNode {
                        name,
                        target,
                        node_properties: Some(properties),
                    };
                    symlinks.push(node);
                }
                Nar::Directory(entries) => {
                    let (dir, digest) = convert_dir_rec(entries, acc, dir_acc);
                    dir_acc.push(dir);
                    let node = DirectoryNode {
                        name,
                        digest: Some(digest.0),
                    };
                    directories.push(node);
                }
            }
        }

        let dir = Directory {
            files,
            directories,
            symlinks,
            node_properties: Some(default_properties(true)),
        };
        let (blob, digest) = protoblob(&dir);
        acc.push(blob);
        (dir, digest)
    }

    let info = match nar {
        Nar::Contents(file) => PathInfo::File {
            executable: file.executable,
            digest: convert_file_rec(file, &mut blobs),
        },
        Nar::Target(_) => panic!("symlink at top level!"),
        Nar::Directory(entries) => {
            let mut dir_acc = Vec::new();
            let (dir, root_digest) = convert_dir_rec(entries, &mut blobs, &mut dir_acc);
            let tree = Tree {
                root: Some(dir),
                children: dir_acc,
            };
            let (tree_blob, tree_digest) = protoblob(&tree);
            blobs.push(tree_blob);
            PathInfo::Directory {
                root_digest,
                tree_digest,
            }
        }
    };

    (blobs, info)
}

#[derive(serde::Deserialize, Debug)]
struct AddMultipleToStoreData {
    data: Vec<(ValidPathInfoWithPath, Nar)>,
}

const METADATA_PATH: &str = "outputmetadata";
const INSTANCE: &str = "my-instance";

#[derive(Debug, Serialize, Deserialize)]
struct OutputMetadata {
    references: Vec<StorePath>,
    nar_hash: NixString,
    nar_size: usize,
}

#[derive(Debug, Serialize, Deserialize)]
struct BuildMetadata {
    // The key is the output name, like "bin"
    metadata: HashMap<NixString, OutputMetadata>,
}

fn build_input_root(
    store_path_map: &HashMap<StorePath, PathInfo>,
    derivation: &Derivation,
) -> (Vec<Blob>, Digest) {
    let (files, dirs): (Vec<_>, Vec<_>) = derivation
        .input_sources
        .paths
        .iter()
        .partition(|t| store_path_map.get(t).unwrap().is_file());
    let store_dir = Directory {
        files: files
            .into_iter()
            .map(|f| {
                let info = store_path_map.get(f).unwrap();
                let (executable, digest) = match info {
                    PathInfo::File { executable, digest } => (executable, digest),
                    _ => unreachable!(),
                };
                FileNode {
                    name: String::from_utf8(f.as_ref().to_owned())
                        .unwrap()
                        .strip_prefix("/nix/store/") // FIXME: look up the actual store path, and also adjust the directory tree building
                        .unwrap()
                        .to_owned(),
                    digest: Some(digest.clone()),
                    is_executable: *executable,
                    node_properties: Some(default_properties(*executable)),
                }
            })
            .collect(),
        directories: dirs
            .into_iter()
            .map(|d| {
                let root_digest = match store_path_map.get(d).unwrap() {
                    PathInfo::File { .. } => panic!("cannot be file"),
                    PathInfo::Directory { root_digest, .. } => root_digest,
                };

                DirectoryNode {
                    name: String::from_utf8(d.as_ref().to_owned())
                        .unwrap()
                        .strip_prefix("/nix/store/") // FIXME: look up the actual store path, and also adjust the directory tree building
                        .unwrap()
                        .to_owned(),
                    digest: Some(root_digest.0.clone()),
                }
            })
            .collect(),
        symlinks: vec![],
        node_properties: Some(default_properties(true)),
    };
    let (store_dir_blob, store_dir_digest) = blob(store_dir.encode_to_vec());

    let nix_dir = Directory {
        files: vec![],
        directories: vec![DirectoryNode {
            name: "store".to_owned(),
            digest: Some(store_dir_digest),
        }],
        symlinks: vec![],
        node_properties: Some(default_properties(true)),
    };
    let (nix_dir_blob, nix_dir_digest) = blob(nix_dir.encode_to_vec());

    // Serialize the derivation to json and include it in the root directory.
    let contents = bincode::serialize(derivation).unwrap();
    let (deriv_blob, deriv_digest) = blob(contents);

    let drv_builder = include_bytes!("../../../target/x86_64-unknown-linux-musl/debug/drv-adapter");
    let (drv_builder_blob, drv_builder_digest) = blob(drv_builder.to_vec());

    let root_dir = Directory {
        files: vec![
            FileNode {
                name: "root.drv".to_owned(),
                digest: Some(deriv_digest),
                is_executable: false,
                node_properties: Some(default_properties(false)),
            },
            FileNode {
                name: "drv-adapter".to_owned(),
                digest: Some(drv_builder_digest),
                is_executable: true,
                node_properties: Some(default_properties(true)),
            },
        ],
        directories: vec![DirectoryNode {
            name: "nix".to_owned(),
            digest: Some(nix_dir_digest),
        }],
        symlinks: vec![],
        node_properties: Some(default_properties(true)),
    };
    let (root_dir_blob, root_dir_digest) = blob(root_dir.encode_to_vec());

    (
        vec![
            store_dir_blob,
            nix_dir_blob,
            root_dir_blob,
            deriv_blob,
            drv_builder_blob,
        ],
        root_dir_digest,
    )
}

async fn lookup_store_paths(
    bs_client: &mut ByteStreamClient<Channel>,
    cas_client: &mut ContentAddressableStorageClient<Channel>,
    ac_client: &mut ActionCacheClient<Channel>,
    paths: &[StorePath],
) -> anyhow::Result<HashMap<StorePath, PathInfo>> {
    let mut digest_map: HashMap<_, _> = paths
        .iter()
        .map(|path| (path, store_path_action_digest(path.0.to_string().unwrap())))
        .collect();

    let req = FindMissingBlobsRequest {
        instance_name: INSTANCE.to_owned(),
        blob_digests: digest_map.values().cloned().collect(),
    };
    let missing: HashSet<_> = cas_client
        .find_missing_blobs(req)
        .await?
        .into_inner()
        .missing_blob_digests
        .into_iter()
        .map(|d| d.hash)
        .collect();

    digest_map.retain(|_path, digest| !missing.contains(&digest.hash));

    let mut ret = HashMap::new();
    for (path, path_digest) in digest_map {
        let req = GetActionResultRequest {
            instance_name: INSTANCE.to_owned(),
            action_digest: Some(path_digest),
            inline_stdout: false,
            inline_stderr: false,
            inline_output_files: vec![],
        };
        // TODO: better would be to treat NOT_FOUND specially
        let action_result = match ac_client.get_action_result(req).await {
            Ok(x) => x.into_inner(),
            Err(e) => {
                eprintln!("failed to get actionresult: {e}");
                continue;
            }
        };
        let path_info = if let Some(dir) = action_result.output_directories.first() {
            path_info_from_tree(
                bs_client,
                TypedDigest::new(dir.tree_digest.clone().unwrap()),
            )
            .await?
        } else if let Some(file) = action_result.output_files.first() {
            PathInfo::File {
                digest: file.digest.clone().unwrap(),
                executable: file.is_executable,
            }
        } else {
            panic!("what?");
        };
        ret.insert(path.clone(), path_info);
    }
    Ok(ret)
}

fn store_path_action_digest(store_path: String) -> Digest {
    let cmd = Command {
        arguments: vec![store_path],
        output_paths: vec!["store-path".into()],
        ..Default::default()
    };
    let cmd_digest = digest(&cmd.encode_to_vec());

    let action = Action {
        command_digest: Some(cmd_digest),
        ..Default::default()
    };
    digest(&action.encode_to_vec())
}

// Use this when the store path has already been uploaded to the CAS. root_digest is the digest of the store path's root directory
// (well, not actually the store path's root directory: it points at a directory called "nix", which contains "store" etc)
async fn add_store_path(
    ac_client: &mut ActionCacheClient<Channel>,
    bs_client: &mut ByteStreamClient<Channel>,
    store_path: String,
    info: PathInfo,
) -> anyhow::Result<()> {
    let cmd = Command {
        arguments: vec![store_path],
        output_paths: vec!["store-path".into()],
        ..Default::default()
    };
    let cmd_digest = upload_proto(bs_client, &cmd).await?;

    let action = Action {
        command_digest: Some(cmd_digest.0),
        ..Default::default()
    };
    let action_digest = upload_proto(bs_client, &action).await?;

    let (output_files, output_directories) = match info {
        PathInfo::File { executable, digest } => (
            vec![OutputFile {
                path: "store-path".into(),
                digest: Some(digest.clone()),
                is_executable: executable,
                contents: vec![],
                node_properties: None,
            }],
            vec![],
        ),
        PathInfo::Directory { tree_digest, .. } => (
            vec![],
            vec![OutputDirectory {
                path: "store-path".into(),
                tree_digest: Some(tree_digest.0.clone()),
            }],
        ),
    };

    let action_result = ActionResult {
        output_files,
        output_directories,
        execution_metadata: Some(ExecutedActionMetadata {
            worker: "Nix store upload".to_owned(),
            ..Default::default()
        }),
        ..Default::default()
    };

    let req = UpdateActionResultRequest {
        instance_name: "my-instance".to_owned(),
        action_digest: Some(action_digest.0.clone()),
        action_result: Some(action_result),
        results_cache_policy: None,
    };

    ac_client.update_action_result(req).await?;

    Ok(())
}

#[derive(Debug)]
struct BuiltPath {
    deriver: StorePath,
    registration_time: u64,
    build_metadata: Digest,
    info: PathInfo,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let channel = Endpoint::from_static("http://localhost:8980")
        .connect()
        .await
        .unwrap();
    let mut exec_client = ExecutionClient::new(channel.clone());
    let mut ac_client = ActionCacheClient::new(channel.clone());
    let mut bs_client = ByteStreamClient::new(channel.clone());
    let mut cas_client = ContentAddressableStorageClient::new(channel);
    eprintln!("Connected to CAS");

    let mut nix = NixProxy::new(std::io::stdin(), std::io::stdout());
    eprintln!("Preparing for handshake");
    let v = nix.handshake()?;
    eprintln!("got version {v}");
    nix.write.inner.write_nix(&stderr::Msg::Last(()))?;
    nix.write.inner.flush()?;

    let mut built_paths: HashMap<StorePath, BuiltPath> = HashMap::new();

    while let Some(op) = nix.next_op()? {
        eprintln!("read op {op:?}");
        match op {
            WorkerOp::QueryValidPaths(op, resp) => {
                dbg!(&op);
                let found_paths = lookup_store_paths(
                    &mut bs_client,
                    &mut cas_client,
                    &mut ac_client,
                    &op.paths.paths,
                )
                .await?;

                let reply = resp.ty(StorePathSet {
                    paths: found_paths.into_keys().collect(),
                });
                nix.write.inner.write_nix(&stderr::Msg::Last(()))?;
                nix.write.inner.write_nix(&reply)?;
                nix.write.inner.flush()?;
            }
            WorkerOp::QueryPathInfo(path, resp) => {
                let Some(BuiltPath {
                    deriver,
                    registration_time,
                    build_metadata,
                    ..
                }) = built_paths.get(&path)
                else {
                    panic!("We don't know about the path {:?}", path);
                };

                let metadata_blob = download_blob(&mut bs_client, build_metadata).await.unwrap();
                let metadata: BuildMetadata = bincode::deserialize(&metadata_blob).unwrap();
                dbg!(&metadata);
                let OutputMetadata {
                    references,
                    nar_hash,
                    nar_size,
                } = metadata.metadata.get(&path.0 .0).unwrap();

                let reply = QueryPathInfoResponse {
                    path: Some(ValidPathInfo {
                        deriver: deriver.clone(),
                        hash: NarHash::from_bytes(nar_hash.as_ref()),
                        references: StorePathSet {
                            paths: references.clone(),
                        },
                        registration_time: *registration_time,
                        nar_size: *nar_size as u64,
                        ultimate: true,
                        sigs: nix_remote::StringSet { paths: vec![] },
                        content_address: NixString::default(),
                    }),
                };
                nix.write.inner.write_nix(&stderr::Msg::Last(()))?;
                nix.write.inner.write_nix(&reply)?;
                nix.write.inner.flush()?;
            }
            WorkerOp::AddMultipleToStore(_op, _) => {
                let framed = FramedData::read(&mut nix.read.inner)?;
                let buf = framed.data.into_iter().fold(Vec::new(), |mut acc, data| {
                    acc.extend_from_slice(&data);
                    acc
                });
                let data: AddMultipleToStoreData = Cursor::new(buf).read_nix()?;
                for (path, nar) in data.data {
                    let (blobs, root_info) = convert(&nar);
                    for blob in blobs {
                        upload_blob(&mut bs_client, blob).await?;
                    }
                    add_store_path(
                        &mut ac_client,
                        &mut bs_client,
                        path.path.0.to_string().unwrap(),
                        root_info,
                    )
                    .await?;
                }
                nix.write.inner.write_nix(&stderr::Msg::Last(()))?;
                nix.write.inner.write_nix(&())?;
                nix.write.inner.flush()?;
            }
            WorkerOp::BuildDerivation(op, _resp) => {
                dbg!(&op);
                let store_path_map = lookup_store_paths(
                    &mut bs_client,
                    &mut cas_client,
                    &mut ac_client,
                    &op.0.derivation.input_sources.paths,
                )
                .await?;
                let (blobs, input_digest) = build_input_root(&store_path_map, &op.0.derivation);
                for blob in blobs {
                    upload_blob(&mut bs_client, blob).await?;
                }
                dbg!(&input_digest);

                let cmd = Command {
                    arguments: vec![
                        // FIXME: build barn runs multiple executions in the same container! The /nix directory
                        // will be preserved between runs. Maybe chroot is more reliable?
                        "./drv-adapter".to_owned(),
                    ],
                    environment_variables: vec![],
                    output_files: vec![],
                    output_directories: vec![],
                    output_paths: op
                        .0
                        .derivation
                        .outputs
                        .iter()
                        // We turn the absolute /nix/store/... path into a relative nix/store/... path because
                        // bazel wants a relative path. They point to the same thing because /nix is a symlink to nix.
                        .map(|out| out.1.store_path.0.to_string().unwrap()[1..].to_owned()) // yuck
                        .chain(std::iter::once(METADATA_PATH.to_owned()))
                        .collect(),
                    platform: None,
                    working_directory: "".to_owned(),
                    output_node_properties: vec![],
                };
                let (cmd_blob, cmd_digest) = blob(cmd.encode_to_vec());
                upload_blob(&mut bs_client, cmd_blob).await?;

                let action = Action {
                    command_digest: Some(cmd_digest),
                    input_root_digest: Some(input_digest),
                    timeout: None,
                    do_not_cache: false,
                    salt: "salt".as_bytes().to_owned(),
                    platform: None,
                };

                let (action_blob, action_digest) = blob(action.encode_to_vec());
                dbg!(&action_digest);
                upload_blob(&mut bs_client, action_blob).await?;

                let req = ExecuteRequest {
                    instance_name: "hardlinking".to_string(),
                    skip_cache_lookup: true,
                    action_digest: Some(action_digest),
                    execution_policy: None,
                    results_cache_policy: None,
                };

                let mut results = exec_client.execute(req).await?.into_inner();
                let mut last_op = None;
                while let Some(op) = results.next().await.transpose()? {
                    if op.done {
                        last_op = op.result;
                        break;
                    }
                }
                match last_op.unwrap() {
                    generated::google::longrunning::operation::Result::Error(e) => {
                        panic!("{:?}", e)
                    }
                    generated::google::longrunning::operation::Result::Response(resp) => {
                        assert_eq!(
                            resp.type_url,
                            "type.googleapis.com/build.bazel.remote.execution.v2.ExecuteResponse"
                        );
                        let resp = ExecuteResponse::decode(resp.value.as_ref())?;
                        let action_result = resp.result.unwrap();
                        dbg!(&action_result);

                        if action_result.exit_code != 0 {
                            panic!();
                        }

                        let (start_time, stop_time) = action_result
                            .execution_metadata
                            .and_then(|m| {
                                m.worker_start_timestamp.zip(m.worker_completed_timestamp)
                            })
                            .unwrap_or_default();

                        let metadata_digest = action_result
                            .output_files
                            .iter()
                            .find(|file| file.path == METADATA_PATH)
                            .unwrap()
                            .digest
                            .as_ref()
                            .unwrap();

                        for output in op.0.derivation.outputs {
                            let output_store_path: &[u8] = &output.1.store_path.as_ref()[1..];
                            let info = if let Some(dir) = action_result
                                .output_directories
                                .iter()
                                .find(|d| d.path.as_bytes() == output_store_path)
                            {
                                path_info_from_tree(
                                    &mut bs_client,
                                    TypedDigest::new(dir.tree_digest.clone().unwrap()),
                                )
                                .await?
                            } else {
                                let file = action_result
                                    .output_files
                                    .iter()
                                    .find(|f| f.path.as_bytes() == output_store_path)
                                    .unwrap();
                                PathInfo::File {
                                    digest: file.digest.clone().unwrap(),
                                    executable: file.is_executable,
                                }
                            };
                            built_paths.insert(
                                StorePath(output.1.store_path.0.clone()),
                                BuiltPath {
                                    deriver: op.0.store_path.clone(),
                                    info,
                                    registration_time: stop_time.seconds as u64,
                                    build_metadata: metadata_digest.clone(),
                                },
                            );
                        }

                        let resp = BuildResult {
                            status: nix_remote::worker_op::BuildStatus::Built,
                            error_msg: NixString::default(),
                            times_built: 1,
                            is_non_deterministic: false,
                            start_time: start_time.seconds as u64, // FIXME
                            stop_time: stop_time.seconds as u64,
                            built_outputs: DrvOutputs(vec![]),
                        };

                        if let Some(stderr_digest) = action_result.stderr_digest {
                            // TODO: stream this instead of sending all at once
                            let stderr = download_blob(&mut bs_client, &stderr_digest).await?;
                            nix.write
                                .inner
                                .write_nix(&stderr::Msg::Next(stderr.into()))?;
                        }
                        nix.write.inner.write_nix(&stderr::Msg::Last(()))?;
                        nix.write.inner.write_nix(&resp)?;
                        nix.write.inner.flush()?;
                    }
                }
            }
            WorkerOp::NarFromPath(op, _resp) => {
                let BuiltPath { info, .. } = built_paths.get(&op).unwrap();

                let nar = match info {
                    PathInfo::File { executable, digest } => {
                        let file_contents = download_blob(&mut bs_client, digest).await?;
                        Nar::Contents(NarFile {
                            contents: file_contents.into(),
                            executable: *executable,
                        })
                    }
                    PathInfo::Directory { tree_digest, .. } => {
                        let tree: Tree = download_proto(&mut bs_client, tree_digest).await.unwrap();

                        let flattened = flatten_tree(&tree);

                        let mut nar = Nar::default();
                        hydrate_nar(&mut bs_client, flattened, &mut nar)
                            .await
                            .unwrap();

                        nar
                    }
                };
                nix.write.inner.write_nix(&stderr::Msg::Last(()))?;

                nix.write.inner.write_nix(&nar)?;

                nix.write.inner.flush()?;
            }
            _ => {
                panic!("ignoring op");
            }
        }
    }

    Ok(())
}

// String is the hash of a digest
fn digest_directory_map(tree: &Tree) -> HashMap<String, Directory> {
    tree.children
        .iter()
        .chain(tree.root.as_ref())
        .map(|dir| {
            let msg = dir.encode_to_vec();
            let digest = digest(&msg);
            (digest.hash, dir.clone())
        })
        .collect()
}

// HACK: the returned thing isn't the real nar, but the file "contents" are replaced by some serialized digest.
fn flatten_tree(tree: &Tree) -> Nar {
    let dirs = digest_directory_map(tree);
    let mut nar = Nar::default();
    let root = (&mut nar).become_directory();

    fn flatten_tree_rec<'a>(
        dir: &Directory,
        dirs: &HashMap<String, Directory>,
        mut sink: impl DirectorySink<'a>,
    ) {
        for entry in &dir.files {
            let mut file = sink.create_entry(entry.name.clone().into()).become_file();
            let digest = entry.digest.as_ref().unwrap();
            file.set_executable(entry.is_executable);
            file.add_contents(format!("{}-{}", digest.hash, digest.size_bytes).as_bytes());
        }

        for entry in &dir.symlinks {
            sink.create_entry(entry.name.clone().into())
                .become_symlink(entry.target.clone().into());
        }

        for entry in &dir.directories {
            let dir_sink = sink
                .create_entry(entry.name.clone().into())
                .become_directory();
            let dir = dirs
                .get(&entry.digest.as_ref().unwrap().hash)
                .expect("some hash wasn't in our list");
            flatten_tree_rec(dir, dirs, dir_sink);
        }
    }

    flatten_tree_rec(tree.root.as_ref().unwrap(), &dirs, root);

    nar.sort();
    nar
}

fn hydrate_nar<'a, 'b: 'a>(
    bs_client: &'b mut ByteStreamClient<Channel>,
    fake_nar: Nar,
    sink: impl EntrySink<'a>,
) -> Pin<Box<dyn Future<Output = anyhow::Result<()>> + 'a>> {
    Box::pin(async {
        match fake_nar {
            Nar::Contents(NarFile {
                contents,
                executable,
            }) => {
                let mut sink = sink.become_file();
                sink.set_executable(executable);

                let contents = contents.to_string()?;
                let (hash, size_str) = contents
                    .rsplit_once('-')
                    .ok_or(anyhow!("invalid digest format"))?;
                let digest = Digest {
                    hash: hash.to_string(),
                    size_bytes: size_str.parse()?,
                };
                let req = blob_request(&digest);
                let mut resp = bs_client.read(req).await?.into_inner();
                while let Some(next) = resp.next().await {
                    let next = next?;
                    sink.add_contents(&next.data);
                }
            }
            Nar::Target(target) => sink.become_symlink(target),
            Nar::Directory(entries) => {
                let mut sink = sink.become_directory();
                for NarDirectoryEntry { name, node } in entries {
                    hydrate_nar(bs_client, node, sink.create_entry(name)).await?;
                }
            }
        }
        Ok(())
    })
}
