mod generated;

use generated::build::bazel::remote::execution::v2::content_addressable_storage_client::ContentAddressableStorageClient;
use nix_remote::nar::Nar;
use nix_remote::worker_op::ValidPathInfo;
use ring::{
    digest::{Context, SHA256},
    hkdf::HKDF_SHA256,
};
use tonic::transport::Endpoint;

use crate::generated::build::bazel::remote::execution::v2::{
    batch_update_blobs_request::Request, BatchReadBlobsRequest, BatchUpdateBlobsRequest, Digest,
    FindMissingBlobsRequest,
};

//const KEY = env!("BUILDBUDDY_API_KEY");

struct AddMultipleToStoreData {
    data: Vec<(ValidPathInfo, Nar)>,
}

#[tokio::main]
async fn main() {
    let channel = Endpoint::from_static("http://localhost:50051")
        .connect()
        .await
        .unwrap();
    let mut cas_client = ContentAddressableStorageClient::new(channel);
    // let cas_client = ContentAddressableStorageClient::with_interceptor(channel, |req| {
    //     req.metadata_mut().insert("x-buildbuddy-api-key", KEY);
    //     Ok(req)
    // });
    println!("Connected, apparently");

    let data = "Hello, world!";
    let mut ctx = Context::new(&SHA256);
    ctx.update(data.as_bytes());
    let digest = ctx.finish();
    assert_eq!(digest.as_ref().len(), 32);
    let hash_parts: Vec<_> = digest.as_ref().iter().map(|c| format!("{c:02x}")).collect();
    assert_eq!(hash_parts.len(), 32);
    dbg!(&hash_parts);
    let hash: String = hash_parts.iter().flat_map(|c| c.chars()).collect();
    assert_eq!(hash.len(), 64);
    dbg!(&hash);
    let result = cas_client
        .batch_update_blobs(BatchUpdateBlobsRequest {
            instance_name: "".to_owned(),
            requests: vec![Request {
                digest: Some(Digest {
                    hash: hash.clone(),
                    size_bytes: data.len() as i64,
                }),
                data: data.as_bytes().to_vec(),
            }],
        })
        .await
        .unwrap();
    dbg!(result);
    let result = cas_client
        .find_missing_blobs(FindMissingBlobsRequest {
            instance_name: "".to_owned(),
            blob_digests: vec![Digest {
                hash: hash.clone(),
                size_bytes: data.len() as i64,
            }],
        })
        .await
        .unwrap();

    dbg!(result);

    let result = cas_client
        .batch_read_blobs(BatchReadBlobsRequest {
            instance_name: "".to_owned(),
            digests: vec![Digest {
                hash,
                size_bytes: data.len() as i64,
            }],
        })
        .await
        .unwrap();

    dbg!(result);
}
