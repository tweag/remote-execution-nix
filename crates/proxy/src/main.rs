// TODO:
// - store in BuildMetadata wether the path is a file or a directory, only try to the tree fetching if it's a directory, otherwise return a "singleton" NAR
// - write a EntrySink impl for nix.write somehow, this will facilitate some streaming
// - basic sending of the nar tree works, but we aren't streaming from or to nix
// - (bonus: pack small files into batch requests)
// - figure out how authentication is going to work. It doesn't seem to be defined as
//   part of the REv2 protocol, so services like buildbuddy seem to roll their own.
//   For now, we're using a locally-running buildbarn instance.
// - send the worker's stderr, maybe even streaming while the build is running

// TODO(eventually): make nar generic over the file contents to make the hydrating nicer

// About using the action cache:
// We should persist a mapping from nix store path to digest into the action cache. Then we can answer QueryValidPaths queries correctly and avoid redundant uploads from nix, and use the CAS as an actual nix store

// Known issues:
// - the fuse runner fails in unpackPhase because cp can't preserve permissions. Maybe set things up in /build instead? For now, hardlinking works

// There was a choice to be made: do we store the files/trees in CAS, or do we store nars. Current strategy is storing files, and packing/unpacking nars in the proxy.
// When returning the build result, we need to
// - return a nar
// - know all the references.
// Our approach: build the nar on the fly and stream it. Have the builder precompute references and store them somewhere in the CAS.

mod generated;

use std::collections::HashMap;
use std::io::{Cursor, Write};
use std::path::PathBuf;
use std::pin::Pin;

use anyhow::anyhow;
use futures::{Future, StreamExt};
use generated::build::bazel::remote::execution::v2::content_addressable_storage_client::ContentAddressableStorageClient;
use generated::build::bazel::remote::execution::v2::{
    FileNode, GetTreeRequest, NodeProperties, OutputDirectory, Tree,
};
use generated::google::bytestream::byte_stream_client::ByteStreamClient;
use generated::google::bytestream::ReadRequest;
use nix_remote::framed_data::FramedData;
use nix_remote::nar::{DirectorySink, EntrySink, FileSink, NarDirectoryEntry, NarFile};
use nix_remote::serialize::Tee;
use nix_remote::worker_op::{
    BuildResult, Derivation, DerivationOutput, DrvOutputs, QueryPathInfoResponse, ValidPathInfo,
    WorkerOp,
};
use nix_remote::{nar::Nar, NixProxy, NixReadExt, NixWriteExt};
use nix_remote::{
    stderr, NarHash, NixString, Realisation, StorePath, StorePathSet, ValidPathInfoWithPath,
};
use prost::Message;
use ring::{
    digest::{Context, SHA256},
    hkdf::HKDF_SHA256,
};
use serde::{Deserialize, Serialize};
use serde_bytes::ByteBuf;
use tonic::transport::{Channel, Endpoint};
use uuid::Uuid;

use crate::generated::build::bazel::remote::execution::v2::execution_client::ExecutionClient;
use crate::generated::build::bazel::remote::execution::v2::{
    batch_update_blobs_request::Request, BatchReadBlobsRequest, BatchUpdateBlobsRequest, Digest,
    FindMissingBlobsRequest,
};
use crate::generated::build::bazel::remote::execution::v2::{
    Action, Command, Directory, DirectoryNode, ExecuteOperationMetadata, ExecuteRequest,
    ExecuteResponse, SymlinkNode,
};
use crate::generated::google::bytestream::WriteRequest;

//const KEY = env!("BUILDBUDDY_API_KEY");

#[derive(serde::Deserialize, Debug)]
struct AddMultipleToStoreData {
    data: Vec<(ValidPathInfoWithPath, Nar)>,
}

const METADATA_PATH: &str = "outputmetadata";

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

struct Blob {
    data: Vec<u8>,
    digest: Digest,
}

#[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
enum PathType {
    File { executable: bool },
    Directory,
}

#[derive(Debug)]
struct PathInfo {
    digest: Digest,
    ty: PathType,
}

fn digest(data: &[u8]) -> Digest {
    let mut ctx = Context::new(&SHA256);
    ctx.update(data);
    let digest = ctx.finish();
    let hash_parts: Vec<_> = digest.as_ref().iter().map(|c| format!("{c:02x}")).collect();
    let hash: String = hash_parts.iter().flat_map(|c| c.chars()).collect();
    Digest {
        hash,
        size_bytes: data.len() as i64,
    }
}

fn default_properties(executable: bool) -> NodeProperties {
    NodeProperties {
        properties: vec![],
        mtime: Some(prost_types::Timestamp {
            seconds: 0,
            nanos: 0,
        }),
        unix_mode: Some(if executable { 0o444 } else { 0o555 }),
    }
}

fn blob_cloned(data: impl AsRef<[u8]>) -> (Blob, Digest) {
    blob(data.as_ref().to_owned())
}

fn blob(data: Vec<u8>) -> (Blob, Digest) {
    let digest = digest(&data);
    (
        Blob {
            digest: digest.clone(),
            data,
        },
        digest,
    )
}

fn convert(nar: &Nar) -> (Vec<Blob>, PathInfo) {
    let mut blobs = Vec::new();

    fn convert_file_rec(file: &NarFile, acc: &mut Vec<Blob>) -> Digest {
        let (blob, digest) = blob_cloned(&file.contents);
        acc.push(blob);
        digest
    }

    fn convert_dir_rec(entries: &[NarDirectoryEntry], acc: &mut Vec<Blob>) -> Digest {
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
                    let digest = convert_dir_rec(entries, acc);
                    let node = DirectoryNode {
                        name,
                        digest: Some(digest),
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
        let (blob, digest) = blob(dir.encode_to_vec());
        acc.push(blob);
        digest
    }

    let (digest, ty) = match nar {
        Nar::Contents(file) => (
            convert_file_rec(file, &mut blobs),
            PathType::File {
                executable: file.executable,
            },
        ),
        Nar::Target(_) => panic!("symlink at top level!"),
        Nar::Directory(entries) => (convert_dir_rec(entries, &mut blobs), PathType::Directory),
    };

    (blobs, PathInfo { digest, ty })
}

const CHUNK_SIZE: usize = 64 * 1024;

async fn upload_blob(client: &mut ByteStreamClient<Channel>, blob: Blob) -> anyhow::Result<()> {
    // TODO: respect size limits by making actual streams
    let uuid = Uuid::new_v4();
    // let req = WriteRequest {
    //     resource_name: format!(
    //         "my-instance/uploads/{uuid}/blobs/{}/{}",
    //         blob.digest.hash, blob.digest.size_bytes
    //     ),
    //     write_offset: 0,
    //     finish_write: true,
    //     data: blob.data,
    // };
    let mut chunks: Vec<_> = blob.data.chunks(CHUNK_SIZE).map(|c| c.to_owned()).collect();
    let resource_name = format!(
        "my-instance/uploads/{uuid}/blobs/{}/{}",
        blob.digest.hash, blob.digest.size_bytes
    );
    let last_chunk = chunks.pop().unwrap_or(vec![]);
    let requests = chunks
        .into_iter()
        .enumerate()
        .map({
            let resource_name = resource_name.clone();
            move |(i, data)| WriteRequest {
                resource_name: resource_name.clone(),
                write_offset: (i * CHUNK_SIZE) as i64,
                finish_write: false,
                data,
            }
        })
        .chain(std::iter::once(WriteRequest {
            resource_name: resource_name.clone(),
            write_offset: blob.digest.size_bytes - last_chunk.len() as i64,
            finish_write: true,
            data: last_chunk,
        }));
    dbg!(resource_name);
    client.write(futures::stream::iter(requests)).await?;
    Ok(())
}

fn blob_request(digest: &Digest) -> ReadRequest {
    let resource_name = format!("my-instance/blobs/{}/{}", digest.hash, digest.size_bytes);
    ReadRequest {
        resource_name,
        read_offset: 0,
        read_limit: 0,
    }
}

async fn download_blob(
    client: &mut ByteStreamClient<Channel>,
    digest: &Digest,
) -> anyhow::Result<Vec<u8>> {
    let req = blob_request(digest);
    let mut resp = client.read(req).await?.into_inner();
    let mut buf = Vec::new();
    while let Some(next) = resp.next().await {
        let next = next?;
        buf.extend_from_slice(&next.data);
    }

    Ok(buf)
}

async fn download_proto<T: prost::Message + Default>(
    client: &mut ByteStreamClient<Channel>,
    digest: &Digest,
) -> anyhow::Result<T> {
    let bytes = download_blob(client, digest).await?;
    Ok(T::decode(bytes.as_ref())?)
}

fn build_input_root(
    store_path_map: &HashMap<StorePath, PathInfo>,
    derivation: &Derivation,
) -> (Vec<Blob>, Digest) {
    let (files, dirs): (Vec<_>, Vec<_>) = derivation
        .input_sources
        .paths
        .iter()
        .partition(|t| matches!(store_path_map.get(t).unwrap().ty, PathType::File { .. }));
    let store_dir = Directory {
        files: files
            .into_iter()
            .map(|f| {
                let info = store_path_map.get(f).unwrap();
                let executable = match info.ty {
                    PathType::File { executable } => executable,
                    _ => unreachable!(),
                };
                FileNode {
                    name: String::from_utf8(f.as_ref().to_owned())
                        .unwrap()
                        .strip_prefix("/nix/store/") // FIXME: look up the actual store path, and also adjust the directory tree building
                        .unwrap()
                        .to_owned(),
                    digest: Some(info.digest.clone()),
                    is_executable: executable,
                    node_properties: Some(default_properties(executable)),
                }
            })
            .collect(),
        directories: dirs
            .into_iter()
            .map(|d| DirectoryNode {
                name: String::from_utf8(d.as_ref().to_owned())
                    .unwrap()
                    .strip_prefix("/nix/store/") // FIXME: look up the actual store path, and also adjust the directory tree building
                    .unwrap()
                    .to_owned(),
                digest: Some(store_path_map.get(d).unwrap().digest.clone()),
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

#[derive(Debug)]
struct BuiltPath {
    deriver: StorePath,
    root_digest: Digest,
    registration_time: u64,
    build_metadata: Digest,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let channel = Endpoint::from_static("http://localhost:8980")
        .connect()
        .await
        .unwrap();
    //let mut cas_client = ContentAddressableStorageClient::new(channel.clone());
    let mut exec_client = ExecutionClient::new(channel.clone());
    let mut bs_client = ByteStreamClient::new(channel);
    let mut store_path_map = HashMap::new();
    eprintln!("Connected to CAS");

    let mut nix = NixProxy::new(std::io::stdin(), std::io::stdout());
    eprintln!("Preparing for handshake");
    let v = nix.handshake()?;
    eprintln!("got version {v}");
    nix.write.inner.write_nix(&stderr::Msg::Last(()))?;
    nix.write.inner.flush()?;

    let mut queried_already = false;
    let mut known_paths: HashMap<StorePath, Digest> = HashMap::new();
    let mut built_paths: HashMap<StorePath, BuiltPath> = HashMap::new();

    while let Some(op) = nix.next_op()? {
        eprintln!("read op {op:?}");
        match op {
            WorkerOp::QueryValidPaths(op, resp) => {
                dbg!(op);
                if !queried_already {
                    queried_already = true;
                    let reply = resp.ty(StorePathSet { paths: Vec::new() });
                    nix.write.inner.write_nix(&stderr::Msg::Last(()))?;
                    nix.write.inner.write_nix(&reply)?;
                    nix.write.inner.flush()?;
                } else {
                    panic!()
                }
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
                dbg!(buf.len(), &buf[0..128]);
                let data: AddMultipleToStoreData = Cursor::new(buf).read_nix()?;
                for (path, nar) in data.data {
                    let (blobs, root_info) = convert(&nar);
                    store_path_map.insert(path.path, root_info);
                    for blob in blobs {
                        upload_blob(&mut bs_client, blob).await?;
                    }
                }
                nix.write.inner.write_nix(&stderr::Msg::Last(()))?;
                nix.write.inner.write_nix(&())?;
                nix.write.inner.flush()?;
            }
            WorkerOp::BuildDerivation(op, _resp) => {
                dbg!(&op);
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
                            let mut output_paths = action_result
                                .output_directories
                                .iter()
                                .cloned()
                                .map(|d| (d.path, d.tree_digest.unwrap()))
                                .chain(
                                    action_result
                                        .output_files
                                        .iter()
                                        .cloned()
                                        .map(|f| (f.path, f.digest.unwrap())),
                                );
                            built_paths.insert(
                                StorePath(output.1.store_path.0.clone()),
                                BuiltPath {
                                    deriver: op.0.store_path.clone(),
                                    root_digest: output_paths
                                        .find(|(path, _)| {
                                            dbg!(&path, &output.1.store_path);
                                            path.as_bytes()
                                                == &AsRef::<[u8]>::as_ref(&output.1.store_path)[1..]
                                        })
                                        .unwrap()
                                        .1,
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
                        nix.write.inner.write_nix(&stderr::Msg::Last(()))?;
                        nix.write.inner.write_nix(&resp)?;
                        nix.write.inner.flush()?;
                    }
                }
            }
            WorkerOp::NarFromPath(op, _resp) => {
                let BuiltPath { root_digest, .. } = built_paths.get(&op).unwrap();

                let tree: Tree = download_proto(&mut bs_client, root_digest).await.unwrap();

                dbg!(&tree);
                let flattened = flatten_tree(&tree);

                nix.write.inner.write_nix(&stderr::Msg::Last(()))?;
                let mut nar = Nar::default();
                hydrate_nar(&mut bs_client, flattened, &mut nar)
                    .await
                    .unwrap();
                nix.write.inner.write_nix(&nar)?;

                nix.write.inner.flush()?;
            }
            _ => {
                panic!("ignoring op");
            }
        }
    }

    // let data = "Hello, world!";
    // let mut ctx = Context::new(&SHA256);
    // ctx.update(data.as_bytes());
    // let digest = ctx.finish();
    // assert_eq!(digest.as_ref().len(), 32);
    // let hash_parts: Vec<_> = digest.as_ref().iter().map(|c| format!("{c:02x}")).collect();
    // assert_eq!(hash_parts.len(), 32);
    // dbg!(&hash_parts);
    // let hash: String = hash_parts.iter().flat_map(|c| c.chars()).collect();
    // assert_eq!(hash.len(), 64);
    // dbg!(&hash);
    // let result = cas_client
    //     .batch_update_blobs(BatchUpdateBlobsRequest {
    //         instance_name: "".to_owned(),
    //         requests: vec![Request {
    //             digest: Some(Digest {
    //                 hash: hash.clone(),
    //                 size_bytes: data.len() as i64,
    //             }),
    //             data: data.as_bytes().to_vec(),
    //         }],
    //     })
    //     .await
    //     .unwrap();
    // dbg!(result);
    // let result = cas_client
    //     .find_missing_blobs(FindMissingBlobsRequest {
    //         instance_name: "".to_owned(),
    //         blob_digests: vec![Digest {
    //             hash: hash.clone(),
    //             size_bytes: data.len() as i64,
    //         }],
    //     })
    //     .await
    //     .unwrap();

    // dbg!(result);

    // let result = cas_client
    //     .batch_read_blobs(BatchReadBlobsRequest {
    //         instance_name: "".to_owned(),
    //         digests: vec![Digest {
    //             hash,
    //             size_bytes: data.len() as i64,
    //         }],
    //     })
    //     .await
    //     .unwrap();

    // dbg!(result);

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
