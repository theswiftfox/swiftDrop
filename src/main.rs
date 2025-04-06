use std::{
    io::Read,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use serde::Deserialize;
use tokio::{
    io::{AsyncReadExt as _, AsyncWriteExt},
    signal,
};

/// Service UUID.
/// 0xFF456C6C6100 Ella with FF prefix 00 Postfix
/// 35990ca1-d35d-4380-8f4f-ff456c6c6100
const SERVICE_UUID: uuid::Uuid = uuid::Uuid::from_u128(0x35990CA1D35D43808F4FFF456C6C6100u128);

/// SERVER_HELLO sent after establishing connection
/// SwiftDrop with FF prefix
const SERVER_HELLO: [u8; 10] = [0xFF, 0x53, 0x77, 0x69, 0x66, 0x74, 0x44, 0x72, 0x6F, 0x70];

/// CLIENT_HELLO to be expected as response to SERVER HELLO
/// Same as `SERVER_HELLO` but with F0 prefix
const CLIENT_HELLO: [u8; 10] = [0xF0, 0x53, 0x77, 0x69, 0x66, 0x74, 0x44, 0x72, 0x6F, 0x70];

const PSM: u16 = bluer::l2cap::PSM_LE_DYN_START + 3;

const SERVER_ACK: [u8; 1] = [0xAC];

trait MsgId {
    fn id() -> u8;
}

#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
struct MetaData {
    filename: String,
    checksum: String,
    chunk_count: usize,
}

impl MsgId for MetaData {
    fn id() -> u8 {
        0xFA
    }
}

struct Chunk;
impl MsgId for Chunk {
    fn id() -> u8 {
        0xFC
    }
}

struct TransfersFinished;
impl MsgId for TransfersFinished {
    fn id() -> u8 {
        0xFF
    }
}

enum Payload {
    MetaData(MetaData),
    Chunk(Vec<u8>),
    TransfersFinished,
}

impl Payload {
    fn from_bytes(bytes: &[u8]) -> Result<Self, String> {
        if bytes.is_empty() {
            return Err("Expected at least 1 byte.".to_owned());
        }

        let id = bytes[0];
        let payload = match id {
            id if id == MetaData::id() => {
                let metadata: MetaData = serde_json::from_slice(&bytes[1..])
                    .map_err(|e| format!("Deserialize failed: {e}"))?;
                Payload::MetaData(metadata)
            }
            id if id == Chunk::id() => {
                if bytes.len() == 1 {
                    return Err("Chunk is empty".to_owned());
                }
                Payload::Chunk(bytes[1..].to_vec())
            }
            id if id == TransfersFinished::id() => Payload::TransfersFinished,
            unkn => return Err(format!("Received Unknown MsgType: {unkn}")),
        };

        Ok(payload)
    }

    fn type_name(&self) -> String {
        match self {
            Payload::MetaData(_) => "MetaData",
            Payload::Chunk(_) => "Chunk",
            Payload::TransfersFinished => "TransfersFinished",
        }
        .to_owned()
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    if std::env::var("RUST_LOG").is_err() {
        // set default to info if none is set already
        // unsafe as env vars are inherently not threadsafe in linux and we need to be sure to not access them concurrently.
        unsafe { std::env::set_var("RUST_LOG", "info") }
    }

    env_logger::init();
    let session = bluer::Session::new().await?;
    let adapter = session.default_adapter().await?;
    adapter.set_powered(true).await?;

    let address = adapter.address().await?;
    let address_type = adapter.address_type().await?;

    log::info!(target: "main", "Advertising {} on Bluetooth adapter {} with address {} and type {}", SERVICE_UUID, adapter.name(), address, address_type);

    let le_advertisement = bluer::adv::Advertisement {
        advertisement_type: bluer::adv::Type::Peripheral,
        service_uuids: vec![SERVICE_UUID].into_iter().collect(),
        discoverable: Some(true),
        local_name: Some("SwiftDrop".to_string()),
        min_interval: Some(Duration::from_millis(20)),
        max_interval: Some(Duration::from_millis(20)),
        tx_power: Some(0),
        ..Default::default()
    };

    let adv_handle = adapter.advertise(le_advertisement).await?;

    let socket_address = bluer::l2cap::SocketAddr::new(address, address_type, PSM);
    let listener = bluer::l2cap::StreamListener::bind(socket_address).await?;

    log::info!(target: "main", "SwiftDrop running on PSM {} ! Press Ctrl + C to quit..", PSM);

    loop {
        log::info!(target: "main", "Waiting for connection..");
        tokio::select! {
            () = shutdown_signal() => {
                break;
            }
            maybe_conn = listener.accept() => {
                match maybe_conn {
                    Err(e) => {
                        log::error!(target: "listener", "Failed to establish connection: {e}");
                        continue;
                    },
                    Ok((socket, addr)) => {
                        let log_target = format!("{}", addr.addr);
                        tokio::select! {
                            () = shutdown_signal() => {
                                break;
                            }
                            res = handle_connection(socket, &log_target) => {
                                if let Err(e) = res {
                                    log::error!(target: &log_target, "{e}");
                                }
                                continue;
                            }
                        }
                    },
                }
            }
        }
    }

    log::info!("SwiftDrop shutting down. Dropping service & advertisement.");

    drop(listener);
    drop(adv_handle);
    tokio::time::sleep(std::time::Duration::from_secs(1)).await;

    Ok(())
}

async fn handle_connection(
    mut socket: bluer::l2cap::Stream,
    log_target: &str,
) -> Result<(), String> {
    let recv_mtu = socket.as_ref().recv_mtu().map_err(|e| e.to_string())?;
    log::info!(target: log_target, "Established connection with recv MTU {recv_mtu}");

    let mut buf = vec![0; recv_mtu as usize];

    let client_hello_parts = if CLIENT_HELLO.len() > recv_mtu as usize {
        let fits = CLIENT_HELLO.len() / recv_mtu as usize;
        if CLIENT_HELLO.len() % recv_mtu as usize == 0 {
            fits
        } else {
            fits + 1
        }
    } else {
        1
    };

    for idx in 0..client_hello_parts {
        let n = match socket.read(&mut buf).await {
            Ok(0) => {
                return Err("Socket closed before client hello..".to_owned());
            }
            Err(e) => {
                return Err(format!("Failed to read client_hello: {e}"));
            }
            Ok(n) => n,
        };

        let offs = idx * recv_mtu as usize;
        if CLIENT_HELLO[offs..offs + n] != buf[..n] {
            return Err(format!(
                "Received invalid Client hello. Expected [{:?}] got [{:?}]",
                &CLIENT_HELLO[offs..offs + n],
                &buf[..n]
            ));
        }
    }

    log::info!(target: log_target, "Received CLIENT_HELLO!");

    log::info!(target: log_target, "Sending SERVER_HELLO!");
    socket
        .write_all(SERVER_HELLO.as_slice())
        .await
        .map_err(|e| format!("Sending server hello failed: {e}"))?;

    // wait for MetaData
    let data = read_data(&mut socket, &mut buf).await?;
    let payload = Payload::from_bytes(data)?;

    let meta_data = match payload {
        Payload::MetaData(meta_data) => meta_data,
        o => return Err(format!("received unexpected Payload {}", o.type_name())),
    };
    log::info!(target: log_target, "Got MetaData: {meta_data:?}");

    send_ack(&mut socket).await?;

    let dl_dir =
        dirs::download_dir().unwrap_or_else(|| dirs::home_dir().expect("$HOME should be set."));

    let file_name = generate_filename(&dl_dir, &meta_data.filename);

    log::info!(target: log_target, "Will store data as {}", file_name.display());

    let bar = indicatif::ProgressBar::new(meta_data.chunk_count.try_into().expect("usize == u64"));

    let start = Instant::now();

    let mut file_buf: Vec<u8> = Vec::new();
    for _ in 1..=meta_data.chunk_count {
        let bytes = read_data(&mut socket, &mut buf).await?;
        // log::info!(target: "chunk", "{bytes:?}");
        let mut chunk = match Payload::from_bytes(bytes) {
            Ok(Payload::Chunk(chunk)) => chunk,
            _ => return Err(format!("Expected chunk but got {bytes:?}")),
        };

        file_buf.append(&mut chunk);

        // log::info!(target: log_target, "Chunk {chunk_no} written.");
        bar.inc(1);

        send_ack(&mut socket).await?;
    }

    // check that transfer complete is sent
    let bytes = read_data(&mut socket, &mut buf).await?;

    match Payload::from_bytes(bytes) {
        Ok(Payload::TransfersFinished) => (),
        o => {
            return Err(format!(
                "Expected 'TransfersFinished' but got {}",
                match o {
                    Ok(o) => o.type_name(),
                    Err(e) => e,
                }
            ));
        }
    };

    send_ack(&mut socket).await?;

    // verify checksum of file
    let sha256 = sha256::digest(&file_buf);

    if sha256 != meta_data.checksum {
        return Err(format!(
            "File corrupted. Expected Checksum {}, Calculated {sha256}",
            meta_data.checksum
        ));
    }

    let mut decoder = xz2::bufread::XzDecoder::new_stream(
        std::io::BufReader::<&[u8]>::new(&file_buf),
        xz2::stream::Stream::new_auto_decoder(u64::MAX, 0u32)
            .map_err(|e| format!("Unable to create decompressor: {e}"))?,
    );

    let mut decoded: Vec<u8> = Vec::new();
    decoder
        .read_to_end(&mut decoded)
        .map_err(|e| format!("Decompression failed: {e}"))?;

    let mut file_out = tokio::fs::File::create(&file_name)
        .await
        .map_err(|e| format!("Unable to create output File: {e}"))?;

    file_out
        .write_all(&decoded)
        .await
        .map_err(|e| format!("Writing received file failed: {e}"))?;

    let end = Instant::now();

    log::info!(target: log_target, "Receiving file completed in {:?}. Checksum OK!", end - start);
    Ok(())
}

async fn read_data<'a>(
    socket: &mut bluer::l2cap::Stream,
    buf: &'a mut [u8],
) -> Result<&'a [u8], String> {
    let n = match socket.read(buf).await {
        Ok(0) => {
            return Err("Socket closed.".to_owned());
        }
        Err(e) => {
            return Err(format!("Failed to read from socket: {e}"));
        }
        Ok(n) => n,
    };
    Ok(&buf[..n])
}

async fn send_ack(socket: &mut bluer::l2cap::Stream) -> Result<(), String> {
    socket
        .write_all(&SERVER_ACK)
        .await
        .map_err(|e| format!("Failed to send SERVER_ACK: {e}"))
}

fn generate_filename(dir: &Path, orig_name: &str) -> PathBuf {
    let mut path = dir.join(orig_name);
    let mut stem = path.file_stem().unwrap().to_string_lossy().to_string();
    let extension = path
        .extension()
        .map(|ext| format!(".{}", ext.to_string_lossy()))
        .unwrap_or_default();
    while path.exists() {
        stem = format!("{stem}(1)");
        path.set_file_name(format!("{stem}{extension}"));
    }
    path
}

async fn shutdown_signal() {
    let ctrl_c = async {
        signal::ctrl_c()
            .await
            .expect("failed to install Ctrl+C handler");
    };

    #[cfg(unix)]
    let terminate = async {
        signal::unix::signal(signal::unix::SignalKind::terminate())
            .expect("failed to install signal handler")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        () = ctrl_c => {},
        () = terminate => {},
    }
}
