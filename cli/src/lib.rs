use anyhow::{Context, Result, anyhow, bail, ensure};
use bytes::Bytes;
use localsend::crypto::cert::{fingerprint_from_cert_der, verify_cert_from_pem};
use localsend::crypto::hash::{sha256_file_content, sha256_hex};
use localsend::http::client::{LsHttpClient, LsHttpClientV2, LsHttpClientVersion};
use localsend::http::dto::{PrepareUploadRequestDto, ProtocolType, RegisterDto};
use localsend::http::dto_v2::InfoResponseDtoV2;
use localsend::http::server::v2::{PrepareUploadDecisionV2, ServerEventV2};
use localsend::http::server::{PeerIp, ServerConfigV2, ServerHandle, TlsConfig, start_with_port};
use localsend::http::state::ClientInfo;
use localsend::model::discovery::{DeviceType, PROTOCOL_VERSION_V2};
use localsend::model::transfer::{FileContent, FileDto};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::env;
use std::ffi::OsString;
use std::fs::{self, OpenOptions, Permissions};
use std::io::Write;
use std::net::IpAddr;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;
use tokio::time::{sleep, timeout};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

pub const USAGE: &str = "\
Использование:
  localsend-cli <FILE>
  localsend-cli --text <TEXT>
  localsend-cli --help

Получатель задан в программе: RUGM0Q3RJ0.";

const TARGET_ALIAS: &str = "RUGM0Q3RJ0";
const TARGET_HOST: &str = "RUGM0Q3RJ0.local";
const TARGET_PORT: u16 = 53317;
const TARGET_FINGERPRINT: &str = "CA6C3E0CDA4B31BDE63E171C8D548D00E658D15CAAD8E7DC8719CF0AD3A8DA12";
const SENDER_ALIAS: &str = "LocalSend CLI";
const SERVER_PORTS: std::ops::RangeInclusive<u16> = 53318..=53327;
const TARGET_TIMEOUT: Duration = Duration::from_secs(8);
const PREPARE_TIMEOUT: Duration = Duration::from_secs(60);
const UPLOAD_TIMEOUT: Duration = Duration::from_secs(30 * 60);
const CANCEL_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug, Eq, PartialEq)]
pub enum Command {
    Help,
    Send(Input),
}

#[derive(Debug, Eq, PartialEq)]
pub enum Input {
    File(PathBuf),
    Text(String),
}

pub fn parse_args(args: impl IntoIterator<Item = OsString>) -> Result<Command> {
    let args = args.into_iter().collect::<Vec<_>>();
    match args.as_slice() {
        [flag] if flag == "--help" || flag == "-h" => Ok(Command::Help),
        [flag, text] if flag == "--text" => {
            let text = text
                .clone()
                .into_string()
                .map_err(|_| anyhow!("Текст должен быть в UTF-8.\n\n{USAGE}"))?;
            ensure!(!text.is_empty(), "Текст не должен быть пустым.\n\n{USAGE}");
            Ok(Command::Send(Input::Text(text)))
        }
        [path] if !path.to_string_lossy().starts_with('-') => {
            Ok(Command::Send(Input::File(PathBuf::from(path))))
        }
        _ => bail!("Передайте один файл или параметр --text.\n\n{USAGE}"),
    }
}

pub async fn send(input: Input) -> Result<()> {
    let state_directory = state_directory()?;
    let identity = load_or_create_identity_at(&state_directory)?;
    let transfer = prepare_transfer(input).await?;
    let mut server = start_callback_server(&identity).await?;

    let operation = async {
        let peer = resolve_target(&identity).await?;
        send_transfer(
            &identity,
            server.port,
            peer,
            transfer,
            &mut server.cancellations,
        )
        .await
    }
    .await;

    let cleanup = server.stop().await;
    combine_operation_and_cleanup(operation, cleanup)
}

fn combine_operation_and_cleanup(operation: Result<()>, cleanup: Result<()>) -> Result<()> {
    match (operation, cleanup) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(error), Ok(())) => Err(error),
        (Ok(()), Err(error)) => Err(error.context("Не удалось остановить временный сервер")),
        (Err(operation_error), Err(cleanup_error)) => Err(anyhow!(
            "{operation_error:#}; дополнительно не удалось остановить временный сервер: \
             {cleanup_error:#}"
        )),
    }
}

#[derive(Clone)]
struct Identity {
    certificate: String,
    private_key: String,
    fingerprint: String,
}

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct StoredIdentity {
    certificate: String,
    private_key: String,
}

fn state_directory() -> Result<PathBuf> {
    let home = env::var_os("HOME")
        .filter(|value| !value.is_empty())
        .context("Переменная HOME не задана")?;
    Ok(PathBuf::from(home)
        .join("Library")
        .join("Application Support")
        .join("localsend-cli"))
}

fn load_or_create_identity_at(directory: &Path) -> Result<Identity> {
    fs::create_dir_all(directory).with_context(|| {
        format!(
            "Не удалось создать каталог идентификатора {}",
            directory.display()
        )
    })?;
    fs::set_permissions(directory, Permissions::from_mode(0o700)).with_context(|| {
        format!(
            "Не удалось установить права на каталог {}",
            directory.display()
        )
    })?;

    let path = directory.join("identity.json");
    match fs::symlink_metadata(&path) {
        Ok(metadata) => {
            ensure!(
                metadata.file_type().is_file(),
                "{} должен быть обычным файлом",
                path.display()
            );
            fs::set_permissions(&path, Permissions::from_mode(0o600))
                .with_context(|| format!("Не удалось установить права на {}", path.display()))?;
            let json = fs::read_to_string(&path)
                .with_context(|| format!("Не удалось прочитать {}", path.display()))?;
            let stored: StoredIdentity = serde_json::from_str(&json)
                .with_context(|| format!("Некорректный файл {}", path.display()))?;
            return validate_identity(stored);
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(error).with_context(|| format!("Не удалось проверить {}", path.display()));
        }
    }

    let identity = generate_identity()?;
    let stored = StoredIdentity {
        certificate: identity.certificate.clone(),
        private_key: identity.private_key.clone(),
    };
    let encoded =
        serde_json::to_vec_pretty(&stored).context("Не удалось сериализовать идентификатор")?;
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)
        .with_context(|| format!("Не удалось создать {}", path.display()))?;
    file.write_all(&encoded)
        .with_context(|| format!("Не удалось записать {}", path.display()))?;
    file.sync_all()
        .with_context(|| format!("Не удалось синхронизировать {}", path.display()))?;
    Ok(identity)
}

fn generate_identity() -> Result<Identity> {
    let key_pair = rcgen::KeyPair::generate().context("Не удалось создать закрытый ключ")?;
    let certificate = rcgen::CertificateParams::new(vec!["LocalSend User".to_string()])
        .context("Не удалось создать параметры сертификата")?
        .self_signed(&key_pair)
        .context("Не удалось подписать сертификат")?;
    validate_identity(StoredIdentity {
        certificate: certificate.pem(),
        private_key: key_pair.serialize_pem(),
    })
}

fn validate_identity(stored: StoredIdentity) -> Result<Identity> {
    verify_cert_from_pem(stored.certificate.clone(), None)
        .context("Сохраненный сертификат недействителен")?;
    let certificate_pem =
        pem::parse(&stored.certificate).context("Сохраненный сертификат не является PEM")?;
    ensure!(
        certificate_pem.tag() == "CERTIFICATE",
        "Сохраненный PEM не содержит сертификат"
    );
    let fingerprint = fingerprint_from_cert_der(certificate_pem.contents());

    let _client = LsHttpClient::new(
        &stored.private_key,
        &stored.certificate,
        LsHttpClientVersion::V2,
        None,
        Some(Duration::from_secs(1)),
    )
    .context("Сохраненный закрытый ключ не соответствует сертификату")?;

    Ok(Identity {
        certificate: stored.certificate,
        private_key: stored.private_key,
        fingerprint,
    })
}

fn normalize_fingerprint(fingerprint: &str) -> Result<String> {
    let normalized = fingerprint.to_ascii_uppercase();
    ensure!(
        normalized.len() == 64
            && normalized
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_lowercase()),
        "Некорректный SHA-256 отпечаток устройства {TARGET_ALIAS}"
    );
    Ok(normalized)
}

struct CallbackServer {
    port: u16,
    stop_tx: oneshot::Sender<()>,
    handle: ServerHandle,
    event_task: JoinHandle<()>,
    cancellations: mpsc::Receiver<CancelEvent>,
}

impl CallbackServer {
    async fn stop(self) -> Result<()> {
        let CallbackServer {
            stop_tx,
            handle,
            event_task,
            cancellations: _,
            port: _,
        } = self;

        let stop_result = stop_tx
            .send(())
            .map_err(|_| anyhow!("Временный сервер уже остановлен"));
        handle.wait_stopped().await;
        drop(handle);
        event_task
            .await
            .context("Задача обработки временного сервера завершилась аварийно")?;
        stop_result
    }
}

async fn start_callback_server(identity: &Identity) -> Result<CallbackServer> {
    let mut last_address_error = None;

    for port in SERVER_PORTS {
        let (event_tx, event_rx) = mpsc::channel(16);
        let (stop_tx, stop_rx) = oneshot::channel();
        let start = start_with_port(
            port,
            Some(TlsConfig {
                cert: identity.certificate.clone(),
                private_key: identity.private_key.clone(),
            }),
            ClientInfo {
                alias: SENDER_ALIAS.to_string(),
                version: PROTOCOL_VERSION_V2.to_string(),
                device_model: Some("macOS".to_string()),
                device_type: Some(DeviceType::Headless),
                token: identity.fingerprint.clone(),
            },
            None,
            Some(ServerConfigV2 {
                pin: None,
                event_tx,
            }),
            None,
            stop_rx,
        )
        .await;

        match start {
            Ok(handle) => {
                let (cancel_tx, cancellations) = mpsc::channel(16);
                let event_task = tokio::spawn(handle_server_events(event_rx, cancel_tx));
                return Ok(CallbackServer {
                    port,
                    stop_tx,
                    handle,
                    event_task,
                    cancellations,
                });
            }
            Err(error) if is_address_in_use(&error) => {
                last_address_error = Some(error);
            }
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("Не удалось запустить HTTPS-сервер на порту {port}"));
            }
        }
    }

    let detail = last_address_error
        .map(|error| format!(": {error:#}"))
        .unwrap_or_default();
    bail!("Все временные TCP-порты 53318-53327 заняты{detail}");
}

fn is_address_in_use(error: &anyhow::Error) -> bool {
    error.chain().any(|source| {
        source
            .downcast_ref::<std::io::Error>()
            .is_some_and(|error| error.kind() == std::io::ErrorKind::AddrInUse)
    })
}

async fn handle_server_events(
    mut events: mpsc::Receiver<ServerEventV2>,
    cancel_tx: mpsc::Sender<CancelEvent>,
) {
    while let Some(event) = events.recv().await {
        match event {
            ServerEventV2::PrepareUpload { decision_tx, .. } => {
                if decision_tx.send(PrepareUploadDecisionV2::Decline).is_err() {
                    continue;
                }
            }
            ServerEventV2::CancelReceived { ip, session_id } => {
                if cancel_tx
                    .send(CancelEvent { ip, session_id })
                    .await
                    .is_err()
                {
                    return;
                }
            }
            _ => {}
        }
    }
}

#[derive(Debug, Eq, PartialEq)]
struct CancelEvent {
    ip: PeerIp,
    session_id: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct Peer {
    ip: PeerIp,
    port: u16,
    fingerprint: String,
}

async fn resolve_target(identity: &Identity) -> Result<Peer> {
    let resolved = timeout(
        TARGET_TIMEOUT,
        tokio::net::lookup_host((TARGET_HOST, TARGET_PORT)),
    )
    .await
    .with_context(|| {
        format!(
            "Имя {TARGET_HOST} не разрешилось за {} секунд",
            TARGET_TIMEOUT.as_secs()
        )
    })?
    .with_context(|| format!("Не удалось разрешить адрес {TARGET_HOST}"))?;
    let mut addresses = resolved
        .filter_map(|address| match address.ip() {
            IpAddr::V4(ip) => Some(ip),
            IpAddr::V6(_) => None,
        })
        .collect::<Vec<_>>();
    addresses.sort_unstable();
    addresses.dedup();
    ensure!(
        !addresses.is_empty(),
        "Имя {TARGET_HOST} не имеет IPv4-адреса"
    );

    let expected_fingerprint = normalize_fingerprint(TARGET_FINGERPRINT)?;
    let mut errors = Vec::new();
    for address in addresses {
        match inspect_target(identity, address.into(), TARGET_PORT, &expected_fingerprint).await {
            Ok(peer) => return Ok(peer),
            Err(error) => errors.push(format!("{address}: {error:#}")),
        }
    }

    bail!(
        "Устройство {TARGET_ALIAS} недоступно по адресу {TARGET_HOST}: {}",
        errors.join("; ")
    )
}

async fn inspect_target(
    identity: &Identity,
    ip: IpAddr,
    port: u16,
    expected_fingerprint: &str,
) -> Result<Peer> {
    let client = LsHttpClientV2::try_new(
        &identity.private_key,
        &identity.certificate,
        Some(expected_fingerprint.to_string()),
        Some(TARGET_TIMEOUT),
    )
    .context("Не удалось создать закрепленный HTTPS-клиент для проверки цели")?;
    let host = ip.to_string();
    let info = client
        .info(ProtocolType::Https, &host, port)
        .await
        .with_context(|| format!("Не удалось получить сведения об устройстве {host}:{port}"))?;
    validate_target_info(&info, expected_fingerprint)?;

    Ok(Peer {
        ip: PeerIp { ip, scope_id: None },
        port,
        fingerprint: expected_fingerprint.to_string(),
    })
}

fn validate_target_info(info: &InfoResponseDtoV2, expected_fingerprint: &str) -> Result<()> {
    ensure!(
        info.alias == TARGET_ALIAS,
        "Получатель представился как {}, ожидалось {TARGET_ALIAS}",
        info.alias
    );
    ensure!(
        info.version == PROTOCOL_VERSION_V2,
        "Устройство {TARGET_ALIAS} использует протокол {}, ожидался {PROTOCOL_VERSION_V2}",
        info.version
    );
    let response_fingerprint = normalize_fingerprint(&info.fingerprint)?;
    ensure!(
        response_fingerprint == expected_fingerprint,
        "TLS-отпечаток в ответе устройства {TARGET_ALIAS} не совпадает с заданным"
    );
    Ok(())
}

struct PreparedTransfer {
    file: FileDto,
    content: TransferContent,
}

enum TransferContent {
    Path(PathBuf),
    Bytes(Vec<u8>),
}

async fn prepare_transfer(input: Input) -> Result<PreparedTransfer> {
    match input {
        Input::File(path) => prepare_file(path).await,
        Input::Text(text) => Ok(prepare_text(text)),
    }
}

async fn prepare_file(path: PathBuf) -> Result<PreparedTransfer> {
    let opened = tokio::fs::File::open(&path)
        .await
        .with_context(|| format!("Не удалось открыть {}", path.display()))?;
    let metadata = opened
        .metadata()
        .await
        .with_context(|| format!("Не удалось прочитать свойства {}", path.display()))?;
    ensure!(metadata.is_file(), "{} не является файлом", path.display());
    drop(opened);

    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .with_context(|| format!("Имя {} должно быть в UTF-8", path.display()))?
        .to_string();
    let id = Uuid::new_v4().to_string();
    let cancel = CancellationToken::new();
    let sha256 = sha256_file_content(FileContent::Path(path.clone()), &cancel)
        .await
        .with_context(|| format!("Не удалось вычислить SHA-256 {}", path.display()))?;
    let size_after_hash = tokio::fs::metadata(&path)
        .await
        .with_context(|| format!("Не удалось повторно проверить {}", path.display()))?
        .len();
    ensure!(
        metadata.len() == size_after_hash,
        "Размер {} изменился во время подготовки",
        path.display()
    );

    Ok(PreparedTransfer {
        file: FileDto {
            id: id.clone(),
            file_name,
            size: metadata.len(),
            file_type: "application/octet-stream".to_string(),
            sha256: Some(sha256),
            preview: None,
            metadata: None,
        },
        content: TransferContent::Path(path),
    })
}

fn prepare_text(text: String) -> PreparedTransfer {
    let preview = text.clone();
    let bytes = text.into_bytes();
    let id = Uuid::new_v4().to_string();
    PreparedTransfer {
        file: FileDto {
            id: id.clone(),
            file_name: format!("{id}.txt"),
            size: bytes.len() as u64,
            file_type: "text/plain".to_string(),
            sha256: Some(sha256_hex(&bytes)),
            preview: Some(preview),
            metadata: None,
        },
        content: TransferContent::Bytes(bytes),
    }
}

async fn send_transfer(
    identity: &Identity,
    sender_port: u16,
    peer: Peer,
    transfer: PreparedTransfer,
    cancellations: &mut mpsc::Receiver<CancelEvent>,
) -> Result<()> {
    let target_protocol = ProtocolType::Https;
    let peer_ip = peer.ip.to_string();
    let is_text = matches!(&transfer.content, TransferContent::Bytes(_));
    let client = LsHttpClient::new(
        &identity.private_key,
        &identity.certificate,
        LsHttpClientVersion::V2,
        Some(peer.fingerprint.clone()),
        Some(UPLOAD_TIMEOUT),
    )
    .context("Не удалось создать закрепленный HTTPS-клиент")?;

    let file_id = transfer.file.id.clone();
    let request = PrepareUploadRequestDto {
        info: sender_info(identity, sender_port),
        files: HashMap::from([(file_id.clone(), transfer.file.clone())]),
    };
    let prepared = timeout(
        PREPARE_TIMEOUT,
        client.prepare_upload(
            target_protocol.clone(),
            &peer_ip,
            peer.port,
            None,
            request,
            None,
        ),
    )
    .await
    .with_context(|| {
        format!(
            "Устройство {TARGET_ALIAS} не ответило за {} секунд",
            PREPARE_TIMEOUT.as_secs()
        )
    })?
    .with_context(|| format!("Устройство {TARGET_ALIAS} не приняло запрос передачи"))?;

    if prepared.status_code == 204 {
        ensure!(
            is_text,
            "Устройство {TARGET_ALIAS} не приняло файл; содержимое не отправлено"
        );
        return Ok(());
    }

    let response = prepared
        .response
        .context("Получатель вернул ответ без идентификатора сессии")?;
    let token = match response.files.get(&file_id).cloned() {
        Some(token) => token,
        None => {
            let error = anyhow!("Устройство {TARGET_ALIAS} не приняло передаваемый объект");
            return Err(cancel_session_after_error(
                &client,
                target_protocol,
                &peer,
                &response.session_id,
                error,
            )
            .await);
        }
    };

    let content = match async {
        validate_before_upload(&transfer).await?;
        into_file_content(transfer.content).await
    }
    .await
    {
        Ok(content) => content,
        Err(error) => {
            return Err(cancel_session_after_error(
                &client,
                target_protocol,
                &peer,
                &response.session_id,
                error,
            )
            .await);
        }
    };

    let upload_cancel = CancellationToken::new();
    let upload = client.upload(
        target_protocol.clone(),
        &peer_ip,
        peer.port,
        None,
        &response.session_id,
        &file_id,
        &token,
        content,
        |_| {},
        upload_cancel.clone(),
    );
    tokio::pin!(upload);
    let remote_cancel = wait_for_matching_cancel(cancellations, peer.ip, &response.session_id);
    tokio::pin!(remote_cancel);
    let upload_deadline = sleep(UPLOAD_TIMEOUT);
    tokio::pin!(upload_deadline);

    enum UploadOutcome {
        Finished(Result<()>),
        RemoteCancelled,
        LocalFailure(anyhow::Error),
    }

    let outcome = tokio::select! {
        result = &mut upload => {
            UploadOutcome::Finished(
                result.context("Передача содержимого завершилась ошибкой")
            )
        }
        cancel = &mut remote_cancel => {
            upload_cancel.cancel();
            let _ = (&mut upload).await;
            match cancel {
                Ok(()) => UploadOutcome::RemoteCancelled,
                Err(error) => UploadOutcome::LocalFailure(error),
            }
        }
        _ = &mut upload_deadline => {
            upload_cancel.cancel();
            let _ = (&mut upload).await;
            UploadOutcome::LocalFailure(anyhow!(
                "Передача не завершилась за {} минут",
                UPLOAD_TIMEOUT.as_secs() / 60
            ))
        }
    };

    match outcome {
        UploadOutcome::Finished(Ok(())) => Ok(()),
        UploadOutcome::RemoteCancelled => {
            bail!("Устройство {TARGET_ALIAS} отменило передачу")
        }
        UploadOutcome::Finished(Err(error)) | UploadOutcome::LocalFailure(error) => {
            Err(cancel_session_after_error(
                &client,
                target_protocol,
                &peer,
                &response.session_id,
                error,
            )
            .await)
        }
    }
}

async fn wait_for_matching_cancel(
    cancellations: &mut mpsc::Receiver<CancelEvent>,
    target_ip: PeerIp,
    session_id: &str,
) -> Result<()> {
    while let Some(event) = cancellations.recv().await {
        if event.ip == target_ip && event.session_id == session_id {
            return Ok(());
        }
    }
    bail!("Временный HTTPS-сервер прекратил принимать отмены")
}

async fn cancel_session_after_error(
    client: &LsHttpClient,
    protocol: ProtocolType,
    peer: &Peer,
    session_id: &str,
    error: anyhow::Error,
) -> anyhow::Error {
    let peer_ip = peer.ip.to_string();
    match timeout(
        CANCEL_TIMEOUT,
        client.cancel(protocol, &peer_ip, peer.port, session_id),
    )
    .await
    {
        Ok(Ok(())) => error,
        Ok(Err(cancel_error)) => {
            anyhow!("{error:#}; отмена сессии также завершилась ошибкой: {cancel_error:#}")
        }
        Err(_) => anyhow!(
            "{error:#}; устройство не подтвердило отмену сессии за {} секунд",
            CANCEL_TIMEOUT.as_secs()
        ),
    }
}

fn sender_info(identity: &Identity, port: u16) -> RegisterDto {
    RegisterDto {
        alias: SENDER_ALIAS.to_string(),
        version: PROTOCOL_VERSION_V2.to_string(),
        device_model: Some("macOS".to_string()),
        device_type: Some(DeviceType::Headless),
        token: identity.fingerprint.clone(),
        port,
        protocol: ProtocolType::Https,
        has_web_interface: false,
    }
}

async fn validate_before_upload(transfer: &PreparedTransfer) -> Result<()> {
    if let TransferContent::Path(path) = &transfer.content {
        let metadata = tokio::fs::metadata(path)
            .await
            .with_context(|| format!("Не удалось проверить {}", path.display()))?;
        ensure!(
            metadata.is_file(),
            "{} больше не является файлом",
            path.display()
        );
        ensure!(
            metadata.len() == transfer.file.size,
            "Размер {} изменился перед отправкой",
            path.display()
        );
    }
    Ok(())
}

async fn into_file_content(content: TransferContent) -> Result<FileContent> {
    match content {
        TransferContent::Path(path) => Ok(FileContent::Path(path)),
        TransferContent::Bytes(bytes) => {
            let (tx, rx) = mpsc::channel(1);
            tx.send(Bytes::from(bytes))
                .await
                .map_err(|_| anyhow!("Не удалось подготовить текст к отправке"))?;
            drop(tx);
            Ok(FileContent::Stream(rx))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use localsend::http::server::common::save::FileUploadTarget;
    use std::collections::HashSet;
    use std::net::Ipv4Addr;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU16, Ordering};
    use tokio::sync::Mutex;

    static NEXT_TEST_PORT: AtomicU16 = AtomicU16::new(45551);

    fn args(values: &[&str]) -> Vec<OsString> {
        values.iter().map(OsString::from).collect()
    }

    fn test_identity() -> Identity {
        generate_identity().expect("identity")
    }

    #[test]
    fn parses_file_text_and_help() {
        assert_eq!(
            parse_args(args(&["document.pdf"])).expect("file"),
            Command::Send(Input::File(PathBuf::from("document.pdf")))
        );
        assert_eq!(
            parse_args(args(&["--text", "hello"])).expect("text"),
            Command::Send(Input::Text("hello".to_string()))
        );
        assert_eq!(parse_args(args(&["--help"])).expect("help"), Command::Help);
    }

    #[test]
    fn rejects_invalid_arguments() {
        assert!(parse_args(args(&[])).is_err());
        assert!(parse_args(args(&["--text", ""])).is_err());
        assert!(parse_args(args(&["a", "b"])).is_err());
        assert!(parse_args(args(&["--unknown"])).is_err());
    }

    #[test]
    fn validates_exact_target_info() {
        let info = InfoResponseDtoV2 {
            alias: TARGET_ALIAS.to_string(),
            version: PROTOCOL_VERSION_V2.to_string(),
            device_model: None,
            device_type: None,
            fingerprint: TARGET_FINGERPRINT.to_string(),
            download: false,
        };
        validate_target_info(&info, TARGET_FINGERPRINT).expect("valid target");
    }

    #[test]
    fn rejects_target_info_mismatches() {
        let mut info = InfoResponseDtoV2 {
            alias: "rugm0q3rj0".to_string(),
            version: PROTOCOL_VERSION_V2.to_string(),
            device_model: None,
            device_type: None,
            fingerprint: TARGET_FINGERPRINT.to_string(),
            download: false,
        };
        assert!(validate_target_info(&info, TARGET_FINGERPRINT).is_err());

        info.alias = TARGET_ALIAS.to_string();
        info.version = "2.0".to_string();
        assert!(validate_target_info(&info, TARGET_FINGERPRINT).is_err());

        info.version = PROTOCOL_VERSION_V2.to_string();
        info.fingerprint = "B".repeat(64);
        assert!(validate_target_info(&info, TARGET_FINGERPRINT).is_err());
    }

    #[tokio::test]
    async fn prepares_file_payload() {
        let directory = env::temp_dir().join(format!("localsend-cli-file-test-{}", Uuid::new_v4()));
        fs::create_dir(&directory).expect("directory");
        let path = directory.join("sample.bin");
        fs::write(&path, b"content").expect("write");

        let transfer = prepare_file(path.clone()).await.expect("prepare");
        assert_eq!(transfer.file.file_name, "sample.bin");
        assert_eq!(transfer.file.size, 7);
        assert_eq!(transfer.file.file_type, "application/octet-stream");
        assert_eq!(
            transfer.file.sha256.as_deref(),
            Some(sha256_hex(b"content").as_str())
        );
        assert!(matches!(transfer.content, TransferContent::Path(ref p) if p == &path));

        fs::remove_dir_all(directory).expect("cleanup");
    }

    #[test]
    fn prepares_text_payload() {
        let transfer = prepare_text("Привет".to_string());
        assert!(transfer.file.file_name.ends_with(".txt"));
        assert_eq!(transfer.file.size, "Привет".len() as u64);
        assert_eq!(transfer.file.file_type, "text/plain");
        assert_eq!(transfer.file.preview.as_deref(), Some("Привет"));
        assert_eq!(
            transfer.file.sha256.as_deref(),
            Some(sha256_hex("Привет".as_bytes()).as_str())
        );
        assert!(
            matches!(transfer.content, TransferContent::Bytes(ref bytes) if bytes == "Привет".as_bytes())
        );
    }

    #[tokio::test]
    async fn accepts_only_matching_remote_cancellation() {
        let target_ip = PeerIp {
            ip: Ipv4Addr::LOCALHOST.into(),
            scope_id: None,
        };
        let other_ip = PeerIp {
            ip: Ipv4Addr::new(127, 0, 0, 2).into(),
            scope_id: None,
        };
        let (cancel_tx, mut cancellations) = mpsc::channel(4);
        cancel_tx
            .send(CancelEvent {
                ip: other_ip,
                session_id: "session".to_string(),
            })
            .await
            .expect("other ip");
        cancel_tx
            .send(CancelEvent {
                ip: target_ip,
                session_id: "other-session".to_string(),
            })
            .await
            .expect("other session");
        cancel_tx
            .send(CancelEvent {
                ip: target_ip,
                session_id: "session".to_string(),
            })
            .await
            .expect("matching");

        wait_for_matching_cancel(&mut cancellations, target_ip, "session")
            .await
            .expect("cancel");
    }

    #[tokio::test]
    async fn server_routes_cancellation_and_declines_incoming_uploads() {
        let (event_tx, event_rx) = mpsc::channel(2);
        let (cancel_tx, mut cancellations) = mpsc::channel(1);
        let event_task = tokio::spawn(handle_server_events(event_rx, cancel_tx));
        let ip = PeerIp {
            ip: Ipv4Addr::LOCALHOST.into(),
            scope_id: None,
        };

        event_tx
            .send(ServerEventV2::CancelReceived {
                ip,
                session_id: "session".to_string(),
            })
            .await
            .expect("cancel event");
        assert_eq!(
            cancellations.recv().await,
            Some(CancelEvent {
                ip,
                session_id: "session".to_string(),
            })
        );

        let (decision_tx, decision_rx) = oneshot::channel();
        event_tx
            .send(ServerEventV2::PrepareUpload {
                session_id: "incoming".to_string(),
                ip,
                info: localsend::http::dto_v2::RegisterDtoV2 {
                    alias: "Other".to_string(),
                    version: PROTOCOL_VERSION_V2.to_string(),
                    device_model: None,
                    device_type: None,
                    fingerprint: test_identity().fingerprint,
                    port: 53317,
                    protocol: localsend::http::dto_v2::ProtocolTypeV2::Https,
                    download: false,
                },
                cert_fingerprint: None,
                files: HashMap::new(),
                decision_tx,
            })
            .await
            .expect("prepare event");
        assert!(matches!(
            decision_rx.await.expect("decision"),
            PrepareUploadDecisionV2::Decline
        ));

        drop(event_tx);
        event_task.await.expect("event task");
    }

    #[tokio::test]
    async fn temporary_callback_server_releases_its_port() {
        let identity = test_identity();
        let server = start_callback_server(&identity).await.expect("server");
        let port = server.port;
        server.stop().await.expect("stop");
        let rebound = std::net::TcpListener::bind((Ipv4Addr::UNSPECIFIED, port))
            .expect("port must be released");
        drop(rebound);
    }

    #[tokio::test]
    async fn transfers_text_to_local_core_server() {
        let sender = test_identity();
        let target = test_identity();
        let port = NEXT_TEST_PORT.fetch_add(1, Ordering::Relaxed);
        let received = Arc::new(Mutex::new(Vec::new()));
        let (event_tx, mut event_rx) = mpsc::channel(16);
        let handler_received = received.clone();

        let event_task = tokio::spawn(async move {
            while let Some(event) = event_rx.recv().await {
                match event {
                    ServerEventV2::PrepareUpload {
                        files, decision_tx, ..
                    } => {
                        let accepted = files.keys().cloned().collect::<HashSet<_>>();
                        assert!(
                            decision_tx
                                .send(PrepareUploadDecisionV2::Accept(accepted))
                                .is_ok()
                        );
                    }
                    ServerEventV2::FileUpload { target_tx, .. } => {
                        let (binary_tx, mut binary_rx) = mpsc::channel(4);
                        let (result_tx, result_rx) = oneshot::channel();
                        assert!(
                            target_tx
                                .send(FileUploadTarget::Stream {
                                    binary_tx,
                                    result_rx,
                                })
                                .is_ok()
                        );
                        let handler_received = handler_received.clone();
                        tokio::spawn(async move {
                            let mut content = Vec::new();
                            while let Some(chunk) = binary_rx.recv().await {
                                content.extend_from_slice(&chunk);
                            }
                            *handler_received.lock().await = content;
                            assert!(result_tx.send(Ok(())).is_ok());
                        });
                    }
                    _ => {}
                }
            }
        });

        let (stop_tx, stop_rx) = oneshot::channel();
        let handle = start_with_port(
            port,
            Some(TlsConfig {
                cert: target.certificate.clone(),
                private_key: target.private_key.clone(),
            }),
            ClientInfo {
                alias: TARGET_ALIAS.to_string(),
                version: PROTOCOL_VERSION_V2.to_string(),
                device_model: Some("Test".to_string()),
                device_type: Some(DeviceType::Headless),
                token: target.fingerprint.clone(),
            },
            None,
            Some(ServerConfigV2 {
                pin: None,
                event_tx,
            }),
            None,
            stop_rx,
        )
        .await
        .expect("server");

        let peer = inspect_target(
            &sender,
            Ipv4Addr::LOCALHOST.into(),
            port,
            &target.fingerprint,
        )
        .await
        .expect("pinned info");
        assert!(
            inspect_target(&sender, Ipv4Addr::LOCALHOST.into(), port, &"B".repeat(64),)
                .await
                .is_err()
        );

        let (_cancel_tx, mut cancellations) = mpsc::channel(1);
        send_transfer(
            &sender,
            53318,
            peer,
            prepare_text("local transfer".to_string()),
            &mut cancellations,
        )
        .await
        .expect("transfer");
        assert_eq!(&*received.lock().await, b"local transfer");

        assert!(stop_tx.send(()).is_ok());
        handle.wait_stopped().await;
        drop(handle);
        event_task.await.expect("event task");
    }

    #[tokio::test]
    async fn does_not_treat_file_204_as_success() {
        let sender = test_identity();
        let target = test_identity();
        let port = NEXT_TEST_PORT.fetch_add(1, Ordering::Relaxed);
        let (event_tx, mut event_rx) = mpsc::channel(4);
        let event_task = tokio::spawn(async move {
            while let Some(event) = event_rx.recv().await {
                if let ServerEventV2::PrepareUpload { decision_tx, .. } = event {
                    assert!(
                        decision_tx
                            .send(PrepareUploadDecisionV2::Accept(HashSet::new()))
                            .is_ok()
                    );
                }
            }
        });
        let (stop_tx, stop_rx) = oneshot::channel();
        let handle = start_with_port(
            port,
            Some(TlsConfig {
                cert: target.certificate.clone(),
                private_key: target.private_key.clone(),
            }),
            ClientInfo {
                alias: TARGET_ALIAS.to_string(),
                version: PROTOCOL_VERSION_V2.to_string(),
                device_model: Some("Test".to_string()),
                device_type: Some(DeviceType::Headless),
                token: target.fingerprint.clone(),
            },
            None,
            Some(ServerConfigV2 {
                pin: None,
                event_tx,
            }),
            None,
            stop_rx,
        )
        .await
        .expect("server");

        let directory = env::temp_dir().join(format!("localsend-cli-204-test-{}", Uuid::new_v4()));
        fs::create_dir(&directory).expect("directory");
        let path = directory.join("sample.bin");
        fs::write(&path, b"content").expect("write");
        let transfer = prepare_file(path).await.expect("prepare");
        let (_cancel_tx, mut cancellations) = mpsc::channel(1);
        let error = send_transfer(
            &sender,
            53318,
            Peer {
                ip: PeerIp {
                    ip: Ipv4Addr::LOCALHOST.into(),
                    scope_id: None,
                },
                port,
                fingerprint: target.fingerprint,
            },
            transfer,
            &mut cancellations,
        )
        .await
        .expect_err("file must not succeed");
        assert!(format!("{error:#}").contains("содержимое не отправлено"));

        assert!(stop_tx.send(()).is_ok());
        handle.wait_stopped().await;
        drop(handle);
        event_task.await.expect("event task");
        fs::remove_dir_all(directory).expect("cleanup");
    }

    #[tokio::test]
    async fn stops_when_receiver_cancels_matching_session() {
        let sender = test_identity();
        let target = test_identity();
        let port = NEXT_TEST_PORT.fetch_add(1, Ordering::Relaxed);
        let (cancel_tx, mut cancellations) = mpsc::channel(2);
        let (event_tx, mut event_rx) = mpsc::channel(4);
        let event_task = tokio::spawn(async move {
            while let Some(event) = event_rx.recv().await {
                match event {
                    ServerEventV2::PrepareUpload {
                        session_id,
                        files,
                        decision_tx,
                        ..
                    } => {
                        let accepted = files.keys().cloned().collect::<HashSet<_>>();
                        assert!(
                            decision_tx
                                .send(PrepareUploadDecisionV2::Accept(accepted))
                                .is_ok()
                        );
                        cancel_tx
                            .send(CancelEvent {
                                ip: PeerIp {
                                    ip: Ipv4Addr::LOCALHOST.into(),
                                    scope_id: None,
                                },
                                session_id,
                            })
                            .await
                            .expect("cancel event");
                    }
                    ServerEventV2::FileUpload { target_tx, .. } => {
                        let (binary_tx, binary_rx) = mpsc::channel(1);
                        let (result_tx, result_rx) = oneshot::channel();
                        assert!(
                            target_tx
                                .send(FileUploadTarget::Stream {
                                    binary_tx,
                                    result_rx,
                                })
                                .is_ok()
                        );
                        tokio::spawn(async move {
                            let _held_open = (binary_rx, result_tx);
                            sleep(Duration::from_secs(1)).await;
                        });
                    }
                    _ => {}
                }
            }
        });
        let (stop_tx, stop_rx) = oneshot::channel();
        let handle = start_with_port(
            port,
            Some(TlsConfig {
                cert: target.certificate.clone(),
                private_key: target.private_key.clone(),
            }),
            ClientInfo {
                alias: TARGET_ALIAS.to_string(),
                version: PROTOCOL_VERSION_V2.to_string(),
                device_model: Some("Test".to_string()),
                device_type: Some(DeviceType::Headless),
                token: target.fingerprint.clone(),
            },
            None,
            Some(ServerConfigV2 {
                pin: None,
                event_tx,
            }),
            None,
            stop_rx,
        )
        .await
        .expect("server");

        let error = send_transfer(
            &sender,
            53318,
            Peer {
                ip: PeerIp {
                    ip: Ipv4Addr::LOCALHOST.into(),
                    scope_id: None,
                },
                port,
                fingerprint: target.fingerprint,
            },
            prepare_text("cancel me".to_string()),
            &mut cancellations,
        )
        .await
        .expect_err("receiver cancellation");
        assert!(format!("{error:#}").contains("отменило передачу"));

        assert!(stop_tx.send(()).is_ok());
        handle.wait_stopped().await;
        drop(handle);
        event_task.await.expect("event task");
    }

    #[test]
    fn persists_stable_identity_with_private_permissions() {
        let directory =
            env::temp_dir().join(format!("localsend-cli-identity-test-{}", Uuid::new_v4()));
        let first = load_or_create_identity_at(&directory).expect("first");
        let second = load_or_create_identity_at(&directory).expect("second");
        assert_eq!(first.fingerprint, second.fingerprint);

        let identity_mode = fs::metadata(directory.join("identity.json"))
            .expect("metadata")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(identity_mode, 0o600);
        fs::remove_dir_all(directory).expect("cleanup");
    }
}
