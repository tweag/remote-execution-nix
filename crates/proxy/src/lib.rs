use futures::StreamExt;
use prost::Message;
use ring::digest::{Context, SHA256};
use tonic::transport::Channel;
use uuid::Uuid;

pub mod generated;

use generated::{
    build::bazel::remote::execution::v2::{Digest, NodeProperties},
    google::bytestream::{byte_stream_client::ByteStreamClient, ReadRequest, WriteRequest},
};

#[derive(Debug, Clone)]
pub struct TypedDigest<T: Message>(pub Digest, std::marker::PhantomData<T>);

impl<T: Message> TypedDigest<T> {
    pub fn new(digest: Digest) -> Self {
        TypedDigest(digest, std::marker::PhantomData)
    }

    pub fn from_message(msg: &T) -> Self {
        Self::new(digest(&msg.encode_to_vec()))
    }
}

pub struct Blob {
    pub data: Vec<u8>,
    pub digest: Digest,
}

pub fn digest(data: &[u8]) -> Digest {
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

pub fn default_properties(executable: bool) -> NodeProperties {
    NodeProperties {
        properties: vec![],
        mtime: Some(prost_types::Timestamp {
            seconds: 0,
            nanos: 0,
        }),
        unix_mode: Some(if executable { 0o444 } else { 0o555 }),
    }
}

pub fn blob_cloned(data: impl AsRef<[u8]>) -> (Blob, Digest) {
    blob(data.as_ref().to_owned())
}

pub fn blob(data: Vec<u8>) -> (Blob, Digest) {
    let digest = digest(&data);
    (
        Blob {
            digest: digest.clone(),
            data,
        },
        digest,
    )
}

pub fn protoblob<T: Message>(data: &T) -> (Blob, TypedDigest<T>) {
    let data = data.encode_to_vec();
    let digest = digest(&data);
    (
        Blob {
            digest: digest.clone(),
            data,
        },
        TypedDigest(digest, std::marker::PhantomData),
    )
}

const CHUNK_SIZE: usize = 64 * 1024;

// TODO: check if the blob is there already
pub async fn upload_blob(client: &mut ByteStreamClient<Channel>, blob: Blob) -> anyhow::Result<()> {
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
    // dbg!(resource_name);
    client.write(futures::stream::iter(requests)).await?;
    Ok(())
}

pub async fn upload_proto<T: Message>(
    client: &mut ByteStreamClient<Channel>,
    data: &T,
) -> anyhow::Result<TypedDigest<T>> {
    let data = data.encode_to_vec();
    let (blob, digest) = blob(data);
    upload_blob(client, blob).await?;
    Ok(TypedDigest(digest, std::marker::PhantomData))
}

// pub async fn upload_proto<T: Message>(
//     client: &mut ByteStreamClient<Channel>,
//     data: &T,
// ) -> anyhow::Result<Digest> {
//     let data = data.encode_to_vec();
//     let (blob, digest) = blob(data);
//     dbg!(&digest);
//     upload_blob(client, blob).await?;
//     Ok(digest)
// }

pub fn blob_request(digest: &Digest) -> ReadRequest {
    let resource_name = format!("my-instance/blobs/{}/{}", digest.hash, digest.size_bytes);
    ReadRequest {
        resource_name,
        read_offset: 0,
        read_limit: 0,
    }
}

pub async fn download_blob(
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

pub async fn download_proto<T: prost::Message + Default>(
    client: &mut ByteStreamClient<Channel>,
    digest: &TypedDigest<T>,
) -> anyhow::Result<T> {
    let bytes = download_blob(client, &digest.0).await?;
    Ok(T::decode(bytes.as_ref())?)
}

// pub async fn download_proto<T: prost::Message + Default>(
//     client: &mut ByteStreamClient<Channel>,
//     digest: &Digest,
// ) -> anyhow::Result<T> {
//     let bytes = download_blob(client, digest).await?;
//     Ok(T::decode(bytes.as_ref())?)
// }
