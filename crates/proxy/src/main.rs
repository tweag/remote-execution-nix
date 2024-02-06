// TODO:
// - basic sending of the nar tree works, but we aren't streaming
// - (bonus: pack small files into batch requests)
// - to do the builds, we'll need to map nix store paths to digests so that we
//   can fetch the filesystem trees; we have an innmemory version, but we may need to persist it into the AC
// - read the BuildDerivation op and figure out how to build it
// - remove leading /nix/store from uploaded files in build_input_root

mod generated;

use std::collections::HashMap;
use std::hash;
use std::io::{Cursor, Write};

use generated::build::bazel::remote::execution::v2::content_addressable_storage_client::ContentAddressableStorageClient;
use generated::build::bazel::remote::execution::v2::{FileNode, NodeProperties};
use generated::google::bytestream::byte_stream_client::ByteStreamClient;
use nix_remote::framed_data::FramedData;
use nix_remote::nar::{NarDirectoryEntry, NarFile};
use nix_remote::worker_op::{BuildDerivation, Stream, ValidPathInfo, WorkerOp};
use nix_remote::{nar::Nar, NixProxy, NixReadExt, NixWriteExt};
use nix_remote::{stderr, StorePath, StorePathSet, ValidPathInfoWithPath};
use prost::Message;
use ring::{
    digest::{Context, SHA256},
    hkdf::HKDF_SHA256,
};
use serde::Serialize;
use tonic::transport::{Channel, Endpoint};
use uuid::Uuid;

use crate::generated::build::bazel::remote::execution::v2::{
    batch_update_blobs_request::Request, BatchReadBlobsRequest, BatchUpdateBlobsRequest, Digest,
    FindMissingBlobsRequest,
};
use crate::generated::build::bazel::remote::execution::v2::{
    Directory, DirectoryNode, SymlinkNode,
};
use crate::generated::google::bytestream::WriteRequest;

//const KEY = env!("BUILDBUDDY_API_KEY");

#[derive(serde::Deserialize, Debug)]
struct AddMultipleToStoreData {
    data: Vec<(ValidPathInfoWithPath, Nar)>,
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
        let (blob, digest) = blob(file.contents.0.as_ref().to_owned());
        acc.push(blob);
        digest
    }

    fn convert_dir_rec(entries: &[NarDirectoryEntry], acc: &mut Vec<Blob>) -> Digest {
        let mut files = Vec::new();
        let mut symlinks = Vec::new();
        let mut directories = Vec::new();
        for entry in entries {
            // FIXME: proper error for non-utf8 names. The protocol apparently doesn't support non-utf8 names.
            let name = String::from_utf8(entry.name.0.as_ref().to_owned()).unwrap();
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
                    let target = String::from_utf8(sym.0.as_ref().to_owned()).unwrap();
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

fn build_input_root(
    store_path_map: &HashMap<StorePath, PathInfo>,
    paths: &StorePathSet,
) -> (Vec<Blob>, Digest) {
    let (files, dirs): (Vec<_>, Vec<_>) = paths
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
                    name: String::from_utf8(f.0 .0.as_ref().to_owned())
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
                name: String::from_utf8(d.0 .0.as_ref().to_owned())
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

    let root_dir = Directory {
        files: vec![],
        directories: vec![DirectoryNode {
            name: "nix".to_owned(),
            digest: Some(nix_dir_digest),
        }],
        symlinks: vec![],
        node_properties: Some(default_properties(true)),
    };
    let (root_dir_blob, root_dir_digest) = blob(root_dir.encode_to_vec());

    (
        vec![store_dir_blob, nix_dir_blob, root_dir_blob],
        root_dir_digest,
    )
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // TODO: figure out how authentication is going to work. It doesn't seem to be defined as
    // part of the REv2 protocol, so services like buildbuddy seem to roll their own.
    // For now, we're using a locally-running buildgrid instance.
    let channel = Endpoint::from_static("http://localhost:8980")
        .connect()
        .await
        .unwrap();
    let mut cas_client = ContentAddressableStorageClient::new(channel.clone());
    let mut bs_client = ByteStreamClient::new(channel);
    // TODO: put each of these mappings in the action cache
    let mut store_path_map = HashMap::new();
    eprintln!("Connected to CAS");

    let mut nix = NixProxy::new(std::io::stdin(), std::io::stdout());
    eprintln!("Preparing for handshake");
    let v = nix.handshake()?;
    eprintln!("got version {v}");
    nix.write.inner.write_nix(&stderr::Msg::Last(()))?;
    nix.write.inner.flush()?;

    while let Some(op) = nix.next_op()? {
        eprintln!("read op {op:?}");
        match op {
            WorkerOp::QueryValidPaths(_op, resp) => {
                let reply = resp.ty(StorePathSet { paths: Vec::new() });
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
                let (blobs, input_digest) =
                    build_input_root(&store_path_map, &op.0.derivation.input_sources);
                for blob in blobs {
                    upload_blob(&mut bs_client, blob).await?;
                }
                dbg!(input_digest);
                todo!()
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
