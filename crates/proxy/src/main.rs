mod generated;

use std::io::{Cursor, Write};

use generated::build::bazel::remote::execution::v2::content_addressable_storage_client::ContentAddressableStorageClient;
use nix_remote::framed_data::FramedData;
use nix_remote::worker_op::{Stream, ValidPathInfo, WorkerOp};
use nix_remote::{nar::Nar, NixProxy, NixReadExt, NixWriteExt};
use nix_remote::{stderr, StorePathSet, ValidPathInfoWithPath};
use ring::{
    digest::{Context, SHA256},
    hkdf::HKDF_SHA256,
};
use serde::Serialize;
use tonic::transport::Endpoint;

use crate::generated::build::bazel::remote::execution::v2::{
    batch_update_blobs_request::Request, BatchReadBlobsRequest, BatchUpdateBlobsRequest, Digest,
    FindMissingBlobsRequest,
};

//const KEY = env!("BUILDBUDDY_API_KEY");

#[derive(serde::Deserialize, Debug)]
struct AddMultipleToStoreData {
    data: Vec<(ValidPathInfoWithPath, Nar)>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // TODO: figure out how authentication is going to work. It doesn't seem to be defined as
    // part of the REv2 protocol, so services like buildbuddy seem to roll their own.
    // For now, we're using a locally-running buildgrid instance.
    // let channel = Endpoint::from_static("http://localhost:50051")
    //     .connect()
    //     .await
    //     .unwrap();
    // let mut cas_client = ContentAddressableStorageClient::new(channel);
    // eprintln!("Connected to CAS");

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
            WorkerOp::AddMultipleToStore(op, _) => {
                let framed = FramedData::read(&mut nix.read.inner)?;
                let buf = framed.data.into_iter().fold(Vec::new(), |mut acc, data| {
                    acc.extend_from_slice(&data);
                    acc
                });
                dbg!(buf.len(), &buf[0..128]);
                let data: AddMultipleToStoreData = Cursor::new(buf).read_nix()?;
                dbg!(data);
            }
            _ => {
                eprintln!("ignoring op");
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
