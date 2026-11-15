//! Channel construction and small REAPI helpers. Service-agnostic.

use crate::config::{expand_header_specs, Settings};
use crate::proto::build::bazel::remote::execution::v2 as re;
use crate::proto::google::bytestream as bs;
use anyhow::{anyhow, bail, Context, Result};
use prost::Message;
use sha2::{Digest as _, Sha256};
use tonic::metadata::{Ascii, Binary, MetadataKey, MetadataValue};
use tonic::service::interceptor::InterceptedService;
use tonic::service::Interceptor;
use tonic::transport::{Certificate, Channel, ClientTlsConfig, Endpoint, Identity};
use tonic::{Request, Status};

const REQMETA_KEY: &str = "build.bazel.remote.execution.v2.requestmetadata-bin";
const MAX_MSG: usize = 64 * 1024 * 1024;

#[derive(Clone)]
pub struct Meta {
    headers: Vec<(MetadataKey<Ascii>, MetadataValue<Ascii>)>,
    reqmeta: MetadataValue<Binary>,
}

impl Interceptor for Meta {
    fn call(&mut self, mut req: Request<()>) -> Result<Request<()>, Status> {
        for (k, v) in &self.headers {
            req.metadata_mut().insert(k.clone(), v.clone());
        }
        req.metadata_mut().insert_bin(REQMETA_KEY, self.reqmeta.clone());
        Ok(req)
    }
}

pub type Svc = InterceptedService<Channel, Meta>;

pub struct Conn {
    pub svc: Svc,
    pub url: String,
}

impl Conn {
    pub fn capabilities(&self) -> re::capabilities_client::CapabilitiesClient<Svc> {
        re::capabilities_client::CapabilitiesClient::new(self.svc.clone()).max_decoding_message_size(MAX_MSG)
    }
    pub fn cas(&self) -> re::content_addressable_storage_client::ContentAddressableStorageClient<Svc> {
        re::content_addressable_storage_client::ContentAddressableStorageClient::new(self.svc.clone())
            .max_decoding_message_size(MAX_MSG)
            .max_encoding_message_size(MAX_MSG)
    }
    pub fn ac(&self) -> re::action_cache_client::ActionCacheClient<Svc> {
        re::action_cache_client::ActionCacheClient::new(self.svc.clone()).max_decoding_message_size(MAX_MSG)
    }
    pub fn exec(&self) -> re::execution_client::ExecutionClient<Svc> {
        re::execution_client::ExecutionClient::new(self.svc.clone()).max_decoding_message_size(MAX_MSG)
    }
    pub fn bytestream(&self) -> bs::byte_stream_client::ByteStreamClient<Svc> {
        bs::byte_stream_client::ByteStreamClient::new(self.svc.clone()).max_decoding_message_size(MAX_MSG)
    }
}

pub fn header_names(s: &Settings) -> Vec<String> {
    match expand_header_specs(&s.remote_header) {
        Ok(v) => v.into_iter().map(|(n, _)| n).collect(),
        Err(e) => vec![format!("(unresolved: {e})")],
    }
}

fn build_meta(s: &Settings, invocation_id: &str) -> Result<Meta> {
    let mut headers = vec![];
    for (n, v) in expand_header_specs(&s.remote_header)? {
        let k = MetadataKey::<Ascii>::from_bytes(n.as_bytes()).map_err(|_| anyhow!("header name '{n}' is not valid for gRPC"))?;
        let mut val = MetadataValue::<Ascii>::try_from(v.as_str())
            .map_err(|_| anyhow!("header '{n}': value is not valid ASCII metadata"))?;
        val.set_sensitive(true);
        headers.push((k, val));
    }
    let rm = re::RequestMetadata {
        tool_details: Some(re::ToolDetails {
            tool_name: "cargo-rbe".into(),
            tool_version: env!("CARGO_PKG_VERSION").into(),
        }),
        tool_invocation_id: invocation_id.into(),
        correlated_invocations_id: invocation_id.into(),
        action_mnemonic: "smoke".into(),
        ..Default::default()
    };
    Ok(Meta { headers, reqmeta: MetadataValue::from_bytes(&rm.encode_to_vec()) })
}

pub async fn connect(url: &str, s: &Settings, invocation_id: &str) -> Result<Conn> {
    let (scheme, rest) = url.split_once("://").ok_or_else(|| anyhow!("endpoint '{url}' needs a scheme (grpc:// or grpcs://)"))?;
    let (tls, http_scheme) = match scheme {
        "grpcs" | "https" => (true, "https"),
        "grpc" | "http" => (false, "http"),
        other => bail!("unsupported scheme '{other}' in '{url}' (use grpc:// or grpcs://)"),
    };
    let mut ep = Endpoint::from_shared(format!("{http_scheme}://{rest}"))
        .with_context(|| format!("bad endpoint '{url}'"))?
        .connect_timeout(std::time::Duration::from_secs(30))
        .tcp_nodelay(true)
        .http2_keep_alive_interval(std::time::Duration::from_secs(30))
        .keep_alive_while_idle(true);
    if tls {
        let mut cfg = ClientTlsConfig::new().with_enabled_roots();
        if let Some(p) = &s.tls_certificate {
            cfg = cfg.ca_certificate(Certificate::from_pem(std::fs::read(p).with_context(|| format!("reading {}", p.display()))?));
        }
        if let (Some(c), Some(k)) = (&s.tls_client_certificate, &s.tls_client_key) {
            cfg = cfg.identity(Identity::from_pem(std::fs::read(c)?, std::fs::read(k)?));
        }
        ep = ep.tls_config(cfg)?;
    }
    let channel = ep.connect().await.with_context(|| format!("connecting to {url}"))?;
    let svc = InterceptedService::new(channel, build_meta(s, invocation_id)?);
    Ok(Conn { svc, url: url.to_string() })
}

// ---- digests and blobs -------------------------------------------------------------------

pub fn digest_of(data: &[u8]) -> re::Digest {
    re::Digest { hash: hex::encode(Sha256::digest(data)), size_bytes: data.len() as i64 }
}

pub fn fmt_digest(d: &re::Digest) -> String {
    format!("{}/{}", &d.hash[..d.hash.len().min(12)], d.size_bytes)
}

fn prefix(instance: &str) -> String {
    if instance.is_empty() { String::new() } else { format!("{}/", instance.trim_matches('/')) }
}

/// FindMissingBlobs, then BatchUpdateBlobs for what is missing. Returns how many were uploaded.
pub async fn ensure_blobs(conn: &Conn, instance: &str, blobs: &[(re::Digest, Vec<u8>)]) -> Result<usize> {
    let mut cas = conn.cas();
    let missing = cas
        .find_missing_blobs(re::FindMissingBlobsRequest {
            instance_name: instance.into(),
            blob_digests: blobs.iter().map(|(d, _)| d.clone()).collect(),
            digest_function: 0,
        })
        .await
        .context("FindMissingBlobs")?
        .into_inner()
        .missing_blob_digests;
    if missing.is_empty() {
        return Ok(0);
    }
    let reqs: Vec<_> = blobs
        .iter()
        .filter(|(d, _)| missing.iter().any(|m| m.hash == d.hash))
        .map(|(d, data)| re::batch_update_blobs_request::Request {
            digest: Some(d.clone()),
            data: data.clone(),
            compressor: 0,
        })
        .collect();
    let n = reqs.len();
    let resp = cas
        .batch_update_blobs(re::BatchUpdateBlobsRequest { instance_name: instance.into(), requests: reqs, digest_function: 0 })
        .await
        .context("BatchUpdateBlobs")?
        .into_inner();
    for r in resp.responses {
        if let Some(st) = r.status {
            if st.code != 0 {
                bail!("BatchUpdateBlobs: blob {} rejected: code {} {}", r.digest.map(|d| fmt_digest(&d)).unwrap_or_default(), st.code, st.message);
            }
        }
    }
    Ok(n)
}

pub async fn batch_read(conn: &Conn, instance: &str, d: &re::Digest) -> Result<Vec<u8>> {
    let resp = conn
        .cas()
        .batch_read_blobs(re::BatchReadBlobsRequest {
            instance_name: instance.into(),
            digests: vec![d.clone()],
            acceptable_compressors: vec![],
            digest_function: 0,
        })
        .await
        .context("BatchReadBlobs")?
        .into_inner();
    let r = resp.responses.into_iter().next().ok_or_else(|| anyhow!("BatchReadBlobs returned no responses"))?;
    if let Some(st) = r.status {
        if st.code != 0 {
            bail!("BatchReadBlobs: code {} {}", st.code, st.message);
        }
    }
    Ok(r.data)
}

pub async fn bs_write(conn: &Conn, instance: &str, data: &[u8]) -> Result<()> {
    let d = digest_of(data);
    let name = format!("{}uploads/{}/blobs/{}/{}", prefix(instance), uuid::Uuid::new_v4(), d.hash, d.size_bytes);
    let chunk = 256 * 1024;
    let mut reqs = vec![];
    let mut off = 0usize;
    loop {
        let end = (off + chunk).min(data.len());
        reqs.push(bs::WriteRequest {
            resource_name: if off == 0 { name.clone() } else { String::new() },
            write_offset: off as i64,
            finish_write: end == data.len(),
            data: data[off..end].to_vec(),
        });
        off = end;
        if off >= data.len() {
            break;
        }
    }
    let resp = conn.bytestream().write(tokio_stream::iter(reqs)).await.context("ByteStream.Write")?.into_inner();
    if resp.committed_size != d.size_bytes && resp.committed_size != -1 {
        bail!("ByteStream.Write committed {} bytes, expected {}", resp.committed_size, d.size_bytes);
    }
    Ok(())
}

pub async fn bs_read(conn: &Conn, instance: &str, d: &re::Digest) -> Result<Vec<u8>> {
    let name = format!("{}blobs/{}/{}", prefix(instance), d.hash, d.size_bytes);
    let mut st = conn
        .bytestream()
        .read(bs::ReadRequest { resource_name: name, read_offset: 0, read_limit: 0 })
        .await
        .context("ByteStream.Read")?
        .into_inner();
    let mut out = Vec::with_capacity(d.size_bytes as usize);
    while let Some(m) = st.message().await.context("ByteStream.Read stream")? {
        out.extend_from_slice(&m.data);
    }
    Ok(out)
}

/// Fetch a blob, choosing BatchRead for small blobs and ByteStream otherwise.
pub async fn fetch(conn: &Conn, instance: &str, d: &re::Digest) -> Result<Vec<u8>> {
    let data = if d.size_bytes <= 1024 * 1024 { batch_read(conn, instance, d).await? } else { bs_read(conn, instance, d).await? };
    if digest_of(&data).hash != d.hash {
        bail!("fetched blob {} failed hash verification", fmt_digest(d));
    }
    Ok(data)
}
