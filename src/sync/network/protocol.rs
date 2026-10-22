use super::super::{
    AUTH_PREFIX, CONNECT_MESSAGE, HELLO, ID_PREFIX, SharedKey, SyncResult, aes_decrypt,
    aes_encrypt, code_key, generate_pairing_key_pair, rsa_decrypt, rsa_encrypt,
};
use super::backend::*;
use super::http;
use super::io_guard::TracedStream;
use super::owner::{
    self, BackendValue, ClientConfig, Command, Context, CryptoValue, Mode, NetworkEvent,
    NetworkState, ServerConfig,
};
use std::collections::HashMap;
use std::io;
use std::net::IpAddr;
use std::pin::Pin;
use std::sync::mpsc::Receiver;
use std::sync::{Arc, Mutex, atomic::Ordering};
use std::task::{Context as TaskContext, Poll};
use std::time::{Duration, Instant};
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt, ReadBuf};
use tokio::net::TcpStream;
use tokio::task::JoinSet;
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;

const IO_TIMEOUT: Duration = Duration::from_secs(10);
#[expect(
    clippy::result_large_err,
    reason = "Tungstenite's upgrade Callback trait requires an unboxed HTTP ErrorResponse"
)]
fn accept_sync_upgrade(
    _: &tokio_tungstenite::tungstenite::handshake::server::Request,
    response: tokio_tungstenite::tungstenite::handshake::server::Response,
) -> Result<
    tokio_tungstenite::tungstenite::handshake::server::Response,
    tokio_tungstenite::tungstenite::handshake::server::ErrorResponse,
> {
    Ok(response)
}
pub(super) fn ws_config() -> WebSocketConfig {
    WebSocketConfig::default()
        .read_buffer_size(16 * 1024)
        .write_buffer_size(16 * 1024)
        .max_write_buffer_size(super::super::MAX_WIRE_BYTES + 32 * 1024)
        .max_message_size(Some(super::super::MAX_WIRE_BYTES))
        .max_frame_size(Some(super::super::MAX_WIRE_BYTES))
}
pub(super) struct Counter {
    count: Arc<Context>,
    peer: bool,
}
struct TicketGuard {
    ticket: SessionTicket,
    context: Context,
}
impl Drop for TicketGuard {
    fn drop(&mut self) {
        self.context.retire_ticket(&self.ticket);
    }
}
impl Counter {
    fn reserve(context: &Context, peer: bool) -> SyncResult<Self> {
        let count = if peer {
            &context.shared.peers
        } else {
            &context.shared.connections
        };
        let limit = if peer { 8 } else { 16 };
        count
            .try_update(Ordering::AcqRel, Ordering::Acquire, |value| {
                (value < limit).then_some(value + 1)
            })
            .map_err(|_| owner::quota("Sync connection capacity exhausted"))?;
        Ok(Self {
            count: Arc::new(context.clone()),
            peer,
        })
    }
}
impl Drop for Counter {
    fn drop(&mut self) {
        let count = if self.peer {
            &self.count.shared.peers
        } else {
            &self.count.shared.connections
        };
        count.fetch_sub(1, Ordering::AcqRel);
    }
}
pub(super) async fn cancelled(context: &Context, ticket: Option<&SessionTicket>) {
    while !context.shared.closing.load(Ordering::Acquire)
        && ticket.is_none_or(SessionTicket::is_active)
    {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}
pub(super) async fn run(
    mode: Mode,
    commands: Receiver<Command>,
    context: &Context,
) -> SyncResult<()> {
    match mode {
        Mode::Server { listener, config } => server(listener, config, commands, context).await,
        Mode::Client(config) => client(config, commands, context).await,
    }
}
struct ServerState {
    config: ServerConfig,
    peers: Mutex<HashMap<String, PeerRecord>>,
    pairing: Mutex<PairState>,
    failures: Mutex<HashMap<IpAddr, (usize, Instant)>>,
}
struct PairState {
    code: owner::PairingCode,
    version: u64,
    expires: Instant,
}
async fn server(
    listener: std::net::TcpListener,
    config: ServerConfig,
    commands: Receiver<Command>,
    context: &Context,
) -> SyncResult<()> {
    let _listener_credit = Counter::reserve(context, false)?;
    let listener = tokio::net::TcpListener::from_std(listener)
        .map_err(|_| owner::error("Cannot own sync listener"))?;
    let loaded = context
        .backend
        .call(64 * 1024, |backend| {
            Ok(BackendValue::Peers(backend.peers()?))
        })
        .await?;
    let BackendValue::Peers(peers) = loaded.value else {
        return Err(owner::error("Invalid sync peer repository response"));
    };
    if peers.len() > 64 {
        return Err(owner::quota("Saved sync peer limit exceeded"));
    }
    let mut map = HashMap::new();
    for peer in peers {
        if peer.peer_id.is_empty() || peer.client_id.is_empty() || peer.auth_epoch == 0 {
            return Err(owner::error("Invalid saved sync peer"));
        }
        map.insert(peer.client_id.clone(), peer);
    }
    let state = Arc::new(ServerState {
        pairing: Mutex::new(PairState {
            code: config.pairing_code.clone(),
            version: 1,
            expires: Instant::now() + Duration::from_secs(180),
        }),
        config,
        peers: Mutex::new(map),
        failures: Mutex::new(HashMap::new()),
    });
    context.state(NetworkState::Listening);
    let address = listener
        .local_addr()
        .map_err(|_| owner::error("Cannot inspect sync listener"))?;
    context.event(NetworkEvent::Listening(address));
    let mut tasks = JoinSet::new();
    loop {
        if context.shared.closing.load(Ordering::Acquire) {
            break;
        }
        while let Ok(Command::Revoke(peer_id)) = commands.try_recv() {
            for ticket in context
                .shared
                .tickets
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .iter()
                .filter(|ticket| ticket.peer_id == peer_id)
            {
                ticket.retire();
            }
            let id = peer_id.clone();
            let result = context
                .backend
                .call(16 * 1024, move |backend| {
                    backend.revoke_peer(&id)?;
                    Ok(BackendValue::Unit)
                })
                .await;
            state
                .peers
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .retain(|_, peer| peer.peer_id != peer_id);
            if let Err(error) = result {
                context.fail(error);
            }
        }
        {
            let mut pairing = state
                .pairing
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            if Instant::now() >= pairing.expires {
                pairing.code = owner::PairingCode::generate()?;
                pairing.version = pairing
                    .version
                    .checked_add(1)
                    .ok_or_else(|| owner::error("Pairing sequence exhausted"))?;
                pairing.expires = Instant::now() + Duration::from_secs(180);
            }
        }
        tokio::select! {
            _=tokio::time::sleep(Duration::from_millis(10))=>{},
            Some(result)=tasks.join_next(),if !tasks.is_empty()=>{if let Ok(Err(error))=result{context.fail(error);}},
            accepted=listener.accept(),if context.shared.connections.load(Ordering::Acquire)<16=>{
                let(stream,address)=accepted.map_err(|_|owner::error("Sync accept failed"))?;let credit=Counter::reserve(context,false)?;let owned=context.clone();let state=state.clone();tasks.spawn(async move{let _credit=credit;server_connection(stream,address.ip(),state,&owned).await});
            }
        }
    }
    tasks.abort_all();
    while tasks.join_next().await.is_some() {}
    drop(listener);
    Ok(())
}
pub(super) struct BoundedTcpStream {
    inner: TcpStream,
    #[cfg(debug_assertions)]
    shared: Arc<owner::Shared>,
}
impl BoundedTcpStream {
    fn new(inner: TcpStream, context: &Context) -> Self {
        #[cfg(not(debug_assertions))]
        let _ = context;
        Self {
            inner,
            #[cfg(debug_assertions)]
            shared: context.shared.clone(),
        }
    }
}
impl AsyncRead for BoundedTcpStream {
    fn poll_read(
        self: Pin<&mut Self>,
        context: &mut TaskContext<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_read(context, buffer)
    }
}
impl AsyncWrite for BoundedTcpStream {
    fn poll_write(
        self: Pin<&mut Self>,
        context: &mut TaskContext<'_>,
        bytes: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        let bytes = &bytes[..bytes.len().min(16 * 1024)];
        let result = Pin::new(&mut this.inner).poll_write(context, bytes);
        #[cfg(debug_assertions)]
        {
            let mut diag = this
                .shared
                .diagnostics
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            diag.tcp_calls = diag.tcp_calls.saturating_add(1);
            diag.tcp_max_request = diag.tcp_max_request.max(bytes.len());
            if let Poll::Ready(Ok(count)) = &result {
                diag.tcp_accepted = diag.tcp_accepted.saturating_add(*count as u64);
            }
        }
        result
    }
    fn poll_write_vectored(
        self: Pin<&mut Self>,
        context: &mut TaskContext<'_>,
        bytes: &[io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        self.poll_write(
            context,
            bytes
                .iter()
                .find(|part| !part.is_empty())
                .map_or(&[], |part| part.as_ref()),
        )
    }
    fn is_write_vectored(&self) -> bool {
        false
    }
    fn poll_flush(self: Pin<&mut Self>, context: &mut TaskContext<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_flush(context)
    }
    fn poll_shutdown(self: Pin<&mut Self>, context: &mut TaskContext<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_shutdown(context)
    }
}
pub(super) enum Stream {
    Plain(BoundedTcpStream),
    ClientTls(tokio_rustls::client::TlsStream<BoundedTcpStream>),
    ServerTls(tokio_rustls::server::TlsStream<BoundedTcpStream>),
}
impl AsyncRead for Stream {
    fn poll_read(
        self: Pin<&mut Self>,
        context: &mut TaskContext<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Self::Plain(stream) => Pin::new(stream).poll_read(context, buffer),
            Self::ClientTls(stream) => Pin::new(stream).poll_read(context, buffer),
            Self::ServerTls(stream) => Pin::new(stream).poll_read(context, buffer),
        }
    }
}
impl AsyncWrite for Stream {
    fn poll_write(
        self: Pin<&mut Self>,
        context: &mut TaskContext<'_>,
        bytes: &[u8],
    ) -> Poll<io::Result<usize>> {
        match self.get_mut() {
            Self::Plain(stream) => Pin::new(stream).poll_write(context, bytes),
            Self::ClientTls(stream) => Pin::new(stream).poll_write(context, bytes),
            Self::ServerTls(stream) => Pin::new(stream).poll_write(context, bytes),
        }
    }
    fn poll_flush(self: Pin<&mut Self>, context: &mut TaskContext<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Self::Plain(stream) => Pin::new(stream).poll_flush(context),
            Self::ClientTls(stream) => Pin::new(stream).poll_flush(context),
            Self::ServerTls(stream) => Pin::new(stream).poll_flush(context),
        }
    }
    fn poll_shutdown(self: Pin<&mut Self>, context: &mut TaskContext<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Self::Plain(stream) => Pin::new(stream).poll_shutdown(context),
            Self::ClientTls(stream) => Pin::new(stream).poll_shutdown(context),
            Self::ServerTls(stream) => Pin::new(stream).poll_shutdown(context),
        }
    }
}
async fn server_connection(
    stream: TcpStream,
    ip: IpAddr,
    state: Arc<ServerState>,
    context: &Context,
) -> SyncResult<()> {
    let actual_send_buffer = bounded_send_buffer(&stream)?;
    #[cfg(debug_assertions)]
    {
        context
            .shared
            .diagnostics
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .send_buffer = actual_send_buffer;
    }
    #[cfg(not(debug_assertions))]
    let _ = actual_send_buffer;
    let stream = BoundedTcpStream::new(stream, context);
    let mut stream = if let Some(config) = &state.config.tls {
        Stream::ServerTls(
            tokio::time::timeout(
                IO_TIMEOUT,
                tokio_rustls::TlsAcceptor::from(config.clone()).accept(stream),
            )
            .await
            .map_err(|_| owner::error("Sync TLS handshake timed out"))?
            .map_err(|_| owner::error("Sync TLS handshake failed"))?,
        )
    } else {
        Stream::Plain(stream)
    };
    let (request, prefix) = http::read_request(&mut stream).await?;
    let route = request.route("");
    let mut code = 200;
    let body;
    match route {
        http::Route::Hello => body = HELLO.into(),
        http::Route::Id => body = format!("{ID_PREFIX}{}", state.config.server_id),
        http::Route::Auth => match authenticate_http(&request, ip, &state, context).await {
            Ok(response) => body = response,
            Err(failure) => {
                // Local bounded diagnostics contain error classes, never the
                // submitted cipher, pairing code, private key or response body.
                context.event(NetworkEvent::Error(failure));
                let blocked = record_failure(ip, &state);
                code = if blocked { 403 } else { 401 };
                body = if blocked { "Blocked IP" } else { "Auth failed" }.into();
            }
        },
        http::Route::Socket => {
            if blocked(ip, &state) {
                write_response(&mut stream, 401, "Unauthorized").await?;
                return Ok(());
            }
            let (client_id, token) = request.socket_auth()?;
            let peer = state
                .peers
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .get(&client_id)
                .cloned()
                .ok_or_else(|| owner::error("Unknown sync peer"))?;
            if aes_decrypt(&token, &peer.key)?.as_slice() != CONNECT_MESSAGE.as_bytes() {
                record_failure(ip, &state);
                write_response(&mut stream, 401, "Unauthorized").await?;
                return Ok(());
            }
            let mut duplicate = false;
            for ticket in context
                .shared
                .tickets
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .iter()
                .filter(|ticket| ticket.peer_id == peer.peer_id)
            {
                duplicate = true;
                ticket.retire();
            }
            let _peer_credit = if duplicate {
                let start = Instant::now();
                loop {
                    match Counter::reserve(context, true) {
                        Ok(credit) => break credit,
                        Err(error) => {
                            if start.elapsed() > Duration::from_secs(5)
                                || context.shared.closing.load(Ordering::Acquire)
                            {
                                return Err(error);
                            }
                            tokio::time::sleep(Duration::from_millis(2)).await;
                        }
                    }
                }
            } else {
                Counter::reserve(context, true)?
            };
            let ticket = context.new_ticket(&peer)?;
            let _ticket_guard = TicketGuard {
                ticket: ticket.clone(),
                context: context.clone(),
            };
            let begin = ticket.clone();
            context
                .backend
                .call(16 * 1024, move |backend| {
                    backend.begin_session(&begin)?;
                    Ok(BackendValue::Unit)
                })
                .await?;
            let guarded = TracedStream::new(stream, prefix);
            let trace = guarded.trace();
            let socket = tokio::time::timeout(
                IO_TIMEOUT,
                tokio_tungstenite::accept_hdr_async_with_config(
                    guarded,
                    accept_sync_upgrade,
                    Some(ws_config()),
                ),
            )
            .await
            .map_err(|_| owner::error("Sync upgrade timed out"))?
            .map_err(|_| owner::error("Sync upgrade failed"))?;
            context.event(NetworkEvent::Authenticated {
                peer_id: peer.peer_id.clone(),
            });
            let result = super::session::serve(
                socket,
                Some(trace),
                peer.clone(),
                ticket.clone(),
                false,
                state.config.capabilities.clone(),
                context.clone(),
            )
            .await;
            context.retire_ticket(&ticket);
            let retire = ticket.clone();
            let _ = context
                .backend
                .call(16 * 1024, move |backend| {
                    backend.retire_session(&retire);
                    Ok(BackendValue::Unit)
                })
                .await;
            context.event(NetworkEvent::PeerClosed {
                peer_id: peer.peer_id,
            });
            return result.map(|_| ());
        }
        http::Route::Unknown => {
            code = 401;
            body = "Forbidden".into();
        }
    }
    write_response(&mut stream, code, &body).await?;
    Ok(())
}
async fn authenticate_http(
    request: &http::Request,
    ip: IpAddr,
    state: &Arc<ServerState>,
    context: &Context,
) -> SyncResult<String> {
    if blocked(ip, state) {
        return Err(owner::error("Blocked IP"));
    }
    let message = request
        .header("m")
        .ok_or_else(|| owner::error("Missing authentication"))?
        .to_string();
    if let Some(id) = request.header("i") {
        let peer = state
            .peers
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .get(id)
            .cloned()
            .ok_or_else(|| owner::error("Unknown sync peer"))?;
        let text = String::from_utf8(aes_decrypt(&message, &peer.key)?)
            .map_err(|_| owner::error("Invalid auth text"))?;
        if !text.starts_with(AUTH_PREFIX) {
            return Err(owner::error("Auth failed"));
        }
        return aes_encrypt(HELLO.as_bytes(), &peer.key);
    }
    let (code, version) = {
        let pair = state
            .pairing
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        (pair.code.clone(), pair.version)
    };
    let capabilities = state.config.capabilities.clone();
    let server_name = state.config.server_name.clone();
    let epoch = state.config.pairing_epoch;
    let prepared=context.crypto.call(move||{
        let text=String::from_utf8(aes_decrypt(&message,&code_key(code.expose())?)?).map_err(|_|owner::error("Invalid auth text"))?;let lines=text.split('\n').collect::<Vec<_>>();if lines.len()!=4||lines[0]!=AUTH_PREFIX||lines[1].len()>4096||lines[2].is_empty()||lines[2].len()>256||lines[2].contains('\0')||!["lx_music_desktop","lx_music_mobile"].contains(&lines[3]){return Err(owner::error("Invalid pairing payload"));}
        let peer=PeerRecord{peer_id:owner::random_id()?,client_id:String::new(),key:SharedKey::generate()?,device_name:lines[2].into(),is_mobile:lines[3]=="lx_music_mobile",auth_epoch:epoch,capabilities};let peer=PeerRecord{client_id:peer.peer_id.clone(),..peer};let public=pairing_public_pem(lines[1])?;let payload=serde_json::json!({"clientId":peer.client_id,"key":peer.key.expose_base64(),"serverName":server_name}).to_string();let response=rsa_encrypt(payload.as_bytes(),&public)?;Ok(CryptoValue::Pair{peer,response})
    }).await?;
    let CryptoValue::Pair { peer, response } = prepared else {
        return Err(owner::error("Invalid pairing result"));
    };
    if state
        .pairing
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .version
        != version
    {
        return Err(owner::error("Pairing code expired"));
    }
    if state
        .peers
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .len()
        >= 64
    {
        return Err(owner::quota("Saved sync peer limit exceeded"));
    }
    let store = peer.clone();
    context
        .backend
        .call(16 * 1024, move |backend| {
            backend.store_peer(epoch, store)?;
            Ok(BackendValue::Unit)
        })
        .await?;
    state
        .peers
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .insert(peer.client_id.clone(), peer);
    Ok(response)
}
fn blocked(ip: IpAddr, state: &ServerState) -> bool {
    state
        .failures
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .get(&ip)
        .is_some_and(|(count, start)| *count >= 10 && start.elapsed() < Duration::from_secs(180))
}
fn pairing_public_pem(encoded: &str) -> SyncResult<String> {
    use base64::{Engine as _, engine::general_purpose::STANDARD};
    if encoded.is_empty()
        || encoded.len() > 4096
        || !encoded.is_ascii()
        || STANDARD.decode(encoded).is_err()
    {
        return Err(owner::error("Invalid pairing public key encoding"));
    }
    // LX transports SPKI Base64 as one line. RustCrypto's strict RFC7468 PEM
    // decoder requires 64-column line wrapping; this affects only its adapter.
    let mut pem = String::with_capacity(encoded.len() + 80);
    pem.push_str("-----BEGIN PUBLIC KEY-----\n");
    for line in encoded.as_bytes().chunks(64) {
        pem.push_str(
            std::str::from_utf8(line)
                .map_err(|_| owner::error("Invalid pairing public key encoding"))?,
        );
        pem.push('\n');
    }
    pem.push_str("-----END PUBLIC KEY-----\n");
    Ok(pem)
}
fn record_failure(ip: IpAddr, state: &ServerState) -> bool {
    let mut entries = state
        .failures
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    entries.retain(|_, (_, start)| start.elapsed() < Duration::from_secs(180));
    if entries.len() >= 128 && !entries.contains_key(&ip) {
        return true;
    }
    let entry = entries.entry(ip).or_insert((0, Instant::now()));
    entry.0 += 1;
    entry.0 >= 10
}
async fn write_response(stream: &mut Stream, code: u16, body: &str) -> SyncResult<()> {
    tokio::time::timeout(
        Duration::from_secs(5),
        stream.write_all(&http::response(code, body)),
    )
    .await
    .map_err(|_| owner::error("Sync HTTP write timed out"))?
    .map_err(|_| owner::error("Sync HTTP write failed"))?;
    stream
        .shutdown()
        .await
        .map_err(|_| owner::error("Sync HTTP close failed"))
}
async fn client(
    mut config: ClientConfig,
    commands: Receiver<Command>,
    context: &Context,
) -> SyncResult<()> {
    let mut failed = 0u32;
    loop {
        if context.shared.closing.load(Ordering::Acquire) {
            return Ok(());
        }
        while let Ok(Command::Revoke(peer_id)) = commands.try_recv() {
            if config
                .credentials
                .as_ref()
                .is_some_and(|credential| credential.server_id == peer_id)
            {
                return Ok(());
            }
        }
        context.state(NetworkState::Authenticating);
        match client_once(&config, context).await {
            Ok((credentials, retry)) => {
                config.credentials = Some(credentials);
                config.pairing_code = None;
                if !retry {
                    return Ok(());
                }
            }
            Err(error) => {
                context.fail(error);
                return Ok(());
            }
        }
        failed += 1;
        if failed > config.reconnect_limit {
            return Err(owner::error("Sync reconnect limit exceeded"));
        }
        let wait = Duration::from_millis((2000 + u64::from(failed / 2) * 3000).min(15000));
        tokio::select! {_=cancelled(context,None)=>return Ok(()),_=tokio::time::sleep(wait)=>{}}
    }
}
async fn client_once(
    config: &ClientConfig,
    context: &Context,
) -> SyncResult<(ClientCredentials, bool)> {
    let base = config.endpoint.base_url()?;
    let (status, hello) = http_get(config, &base, "hello", &[], context).await?;
    if status != 200 || hello != HELLO.as_bytes() {
        return Err(owner::error("Not an LX v4 sync peer"));
    }
    let (status, id) = http_get(config, &base, "id", &[], context).await?;
    let id = String::from_utf8(id).map_err(|_| owner::error("Invalid sync identity"))?;
    let server_id = id
        .strip_prefix(ID_PREFIX)
        .filter(|id| !id.is_empty() && id.len() <= 256)
        .ok_or_else(|| owner::error("Invalid sync identity"))?
        .to_string();
    if status != 200 {
        return Err(owner::error("Cannot read sync identity"));
    }
    let credentials = if let Some(code) = &config.pairing_code {
        let code = code.clone();
        let name = config.device_name.clone();
        let keys = context
            .crypto
            .call(|| Ok(CryptoValue::KeyPair(generate_pairing_key_pair()?)))
            .await?;
        let CryptoValue::KeyPair(keys) = keys else {
            return Err(owner::error("Invalid RSA pairing key result"));
        };
        let public = keys
            .public_pem
            .lines()
            .filter(|line| !line.starts_with("-----"))
            .collect::<String>();
        let message = format!("{AUTH_PREFIX}\n{public}\n{name}\nlx_music_desktop");
        let message = aes_encrypt(message.as_bytes(), &code_key(code.expose())?)?;
        let (status, body) =
            http_get(config, &base, "ah", &[("m", message.as_str())], context).await?;
        if status != 200 {
            return Err(owner::error(&format!("Pairing failed (HTTP {status})")));
        }
        let body = String::from_utf8(body).map_err(|_| owner::error("Invalid pairing response"))?;
        let raw = rsa_decrypt(&body, &keys.private_pem)?;
        let value: serde_json::Value =
            serde_json::from_slice(&raw).map_err(|_| owner::error("Invalid pairing response"))?;
        let client_id = value["clientId"]
            .as_str()
            .filter(|id| !id.is_empty() && id.len() <= 256)
            .ok_or_else(|| owner::error("Invalid pairing identity"))?
            .to_string();
        let key = SharedKey::from_base64(
            value["key"]
                .as_str()
                .ok_or_else(|| owner::error("Invalid pairing key"))?,
        )?;
        let server_name = value["serverName"]
            .as_str()
            .filter(|name| name.len() <= 256)
            .ok_or_else(|| owner::error("Invalid peer display name"))?
            .to_string();
        let credentials = ClientCredentials {
            server_id: server_id.clone(),
            client_id,
            key,
            server_name,
            auth_epoch: config.pairing_epoch,
        };
        let peer = PeerRecord {
            peer_id: server_id.clone(),
            client_id: credentials.client_id.clone(),
            key: credentials.key.clone(),
            device_name: credentials.server_name.clone(),
            is_mobile: false,
            auth_epoch: credentials.auth_epoch,
            capabilities: config.capabilities.clone(),
        };
        let epoch = config.pairing_epoch;
        context
            .backend
            .call(16 * 1024, move |backend| {
                backend.store_peer(epoch, peer)?;
                Ok(BackendValue::Unit)
            })
            .await?;
        credentials
    } else {
        let credentials = config
            .credentials
            .clone()
            .ok_or_else(|| owner::error("Missing sync credentials"))?;
        if credentials.server_id != server_id {
            return Err(owner::error("Sync peer identity changed"));
        }
        let message = aes_encrypt(
            format!("{AUTH_PREFIX}{}", config.device_name).as_bytes(),
            &credentials.key,
        )?;
        let (status, body) = http_get(
            config,
            &base,
            "ah",
            &[("i", &credentials.client_id), ("m", &message)],
            context,
        )
        .await?;
        let body = String::from_utf8(body).map_err(|_| owner::error("Invalid auth response"))?;
        if status != 200 || aes_decrypt(&body, &credentials.key)?.as_slice() != HELLO.as_bytes() {
            return Err(owner::error("Sync authentication failed"));
        }
        credentials
    };
    let _socket_credit = Counter::reserve(context, false)?;
    let _peer_credit = Counter::reserve(context, true)?;
    let peer = PeerRecord {
        peer_id: server_id,
        client_id: credentials.client_id.clone(),
        key: credentials.key.clone(),
        device_name: credentials.server_name.clone(),
        is_mobile: false,
        auth_epoch: credentials.auth_epoch,
        capabilities: config.capabilities.clone(),
    };
    let ticket = context.new_ticket(&peer)?;
    let _ticket_guard = TicketGuard {
        ticket: ticket.clone(),
        context: context.clone(),
    };
    let begin = ticket.clone();
    context
        .backend
        .call(16 * 1024, move |backend| {
            backend.begin_session(&begin)?;
            Ok(BackendValue::Unit)
        })
        .await?;
    let mut socket_url = base.clone();
    let path = format!("{}/socket", base.path().trim_end_matches('/'));
    socket_url.set_path(&path);
    socket_url
        .set_scheme(if base.scheme() == "https" {
            "wss"
        } else {
            "ws"
        })
        .map_err(|_| owner::error("Invalid WebSocket origin"))?;
    socket_url
        .query_pairs_mut()
        .append_pair("i", &credentials.client_id)
        .append_pair(
            "t",
            &aes_encrypt(CONNECT_MESSAGE.as_bytes(), &credentials.key)?,
        );
    let stream = connect(config, &base, context).await?;
    let (socket, _) = tokio::time::timeout(
        IO_TIMEOUT,
        tokio_tungstenite::client_async_with_config(socket_url.as_str(), stream, Some(ws_config())),
    )
    .await
    .map_err(|_| owner::error("Sync upgrade timed out"))?
    .map_err(|_| owner::error("Sync upgrade failed"))?;
    context.event(NetworkEvent::Authenticated {
        peer_id: peer.peer_id.clone(),
    });
    let result = super::session::serve(
        socket,
        None,
        peer.clone(),
        ticket.clone(),
        true,
        config.capabilities.clone(),
        context.clone(),
    )
    .await;
    context.retire_ticket(&ticket);
    let retire = ticket.clone();
    let _ = context
        .backend
        .call(16 * 1024, move |backend| {
            backend.retire_session(&retire);
            Ok(BackendValue::Unit)
        })
        .await;
    context.event(NetworkEvent::PeerClosed {
        peer_id: peer.peer_id,
    });
    Ok((credentials, result?))
}
async fn connect(
    config: &ClientConfig,
    base: &reqwest::Url,
    context: &Context,
) -> SyncResult<Stream> {
    let tcp = tokio::time::timeout(
        IO_TIMEOUT,
        TcpStream::connect(config.endpoint.pinned_address),
    )
    .await
    .map_err(|_| owner::error("Sync connect timed out"))?
    .map_err(|_| owner::error("Cannot connect sync peer"))?;
    bounded_send_buffer(&tcp)?;
    let _ = tcp.set_nodelay(true);
    let tcp = BoundedTcpStream::new(tcp, context);
    if base.scheme() == "https" {
        let name = rustls::pki_types::ServerName::try_from(
            base.host_str()
                .unwrap()
                .trim_matches(['[', ']'])
                .to_string(),
        )
        .map_err(|_| owner::error("Invalid TLS peer name"))?;
        let tls = tokio::time::timeout(
            IO_TIMEOUT,
            tokio_rustls::TlsConnector::from(config.endpoint.tls.as_ref().unwrap().clone())
                .connect(name, tcp),
        )
        .await
        .map_err(|_| owner::error("Sync TLS timed out"))?
        .map_err(|_| owner::error("Sync certificate or hostname rejected"))?;
        Ok(Stream::ClientTls(tls))
    } else {
        Ok(Stream::Plain(tcp))
    }
}
async fn http_get(
    config: &ClientConfig,
    base: &reqwest::Url,
    route: &str,
    headers: &[(&str, &str)],
    context: &Context,
) -> SyncResult<(u16, Vec<u8>)> {
    let _credit = Counter::reserve(context, false)?;
    let mut stream = connect(config, base, context).await?;
    let host = base.host_str().unwrap().trim_matches(['[', ']']);
    let host = if host.contains(':') {
        format!("[{host}]")
    } else {
        host.into()
    };
    let host = format!("{host}:{}", base.port_or_known_default().unwrap());
    let path = format!("{}/{route}", base.path().trim_end_matches('/'));
    let mut request = format!("GET {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n");
    for (key, value) in headers {
        if value.contains(['\r', '\n']) {
            return Err(owner::error("Invalid auth header"));
        }
        request.push_str(&format!("{key}: {value}\r\n"));
    }
    request.push_str("\r\n");
    tokio::time::timeout(Duration::from_secs(5), stream.write_all(request.as_bytes()))
        .await
        .map_err(|_| owner::error("Sync HTTP timed out"))?
        .map_err(|_| owner::error("Sync HTTP write failed"))?;
    http::read_response(&mut stream).await
}

/// A completed application flush is not a peer ACK. Bound the kernel's queued
/// unsent tail so a peer that stops reading reaches the real five-second write
/// deadline, rather than retaining a multi-MiB OS queue until the RPC timeout.
fn bounded_send_buffer(stream: &TcpStream) -> SyncResult<usize> {
    const REQUESTED: i32 = 16 * 1024;
    #[cfg(windows)]
    let configured = {
        use std::os::windows::io::AsRawSocket;
        #[link(name = "ws2_32")]
        unsafe extern "system" {
            fn setsockopt(
                socket: usize,
                level: i32,
                name: i32,
                value: *const u8,
                length: i32,
            ) -> i32;
            fn getsockopt(
                socket: usize,
                level: i32,
                name: i32,
                value: *mut u8,
                length: *mut i32,
            ) -> i32;
        }
        let socket = stream.as_raw_socket() as usize;
        if unsafe { setsockopt(socket, 0xffff, 0x1001, (&REQUESTED as *const i32).cast(), 4) } != 0
        {
            return Err(owner::error("Cannot bound sync socket send buffer"));
        }
        let mut value = 0i32;
        let mut length = 4;
        if unsafe {
            getsockopt(
                socket,
                0xffff,
                0x1001,
                (&mut value as *mut i32).cast(),
                &mut length,
            )
        } != 0
            || length != 4
            || value <= 0
            || value > REQUESTED
        {
            return Err(owner::error(
                "Sync socket send buffer bound was not applied",
            ));
        }
        value
    };
    #[cfg(unix)]
    let configured = {
        use std::os::fd::AsRawFd;
        let socket = stream.as_raw_fd();
        if unsafe {
            libc::setsockopt(
                socket,
                libc::SOL_SOCKET,
                libc::SO_SNDBUF,
                (&REQUESTED as *const i32).cast(),
                std::mem::size_of_val(&REQUESTED) as libc::socklen_t,
            )
        } != 0
        {
            return Err(owner::error("Cannot bound sync socket send buffer"));
        }
        let mut value = 0i32;
        let mut length = std::mem::size_of_val(&value) as libc::socklen_t;
        if unsafe {
            libc::getsockopt(
                socket,
                libc::SOL_SOCKET,
                libc::SO_SNDBUF,
                (&mut value as *mut i32).cast(),
                &mut length,
            )
        } != 0
            || length as usize != std::mem::size_of_val(&value)
            || value <= 0
            || value > REQUESTED * 2
        {
            return Err(owner::error(
                "Sync socket send buffer bound was not applied",
            ));
        }
        value
    };
    #[cfg(not(any(windows, unix)))]
    let configured = {
        let _ = stream;
        return Err(owner::error(
            "Bounded sync sockets are not supported on this platform",
        ));
    };
    Ok(configured as usize)
}

#[cfg(test)]
mod pairing_tests {
    use super::*;
    #[test]
    fn compact_spki_wire_is_wrapped_for_real_strict_rsa_parser() {
        let keys = generate_pairing_key_pair().unwrap();
        let wire = keys
            .public_pem
            .lines()
            .filter(|line| !line.starts_with("-----"))
            .collect::<String>();
        let unwrapped = format!("-----BEGIN PUBLIC KEY-----\n{wire}\n-----END PUBLIC KEY-----");
        assert!(rsa_encrypt(b"fixture", &unwrapped).is_err());
        let wrapped = pairing_public_pem(&wire).unwrap();
        let cipher = rsa_encrypt(b"fixture", &wrapped).unwrap();
        assert_eq!(rsa_decrypt(&cipher, &keys.private_pem).unwrap(), b"fixture");
        assert!(pairing_public_pem("invalid public key bytes").is_err());
    }
    #[test]
    fn connected_socket_has_an_actual_bounded_kernel_send_queue() {
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let peer = std::net::TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (accepted, _) = listener.accept().unwrap();
        accepted.set_nonblocking(true).unwrap();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let _entered = runtime.enter();
        let accepted = TcpStream::from_std(accepted).unwrap();
        let actual = bounded_send_buffer(&accepted).unwrap();
        #[cfg(windows)]
        assert!(actual <= 16 * 1024);
        #[cfg(unix)]
        assert!(actual <= 32 * 1024);
        assert!(actual > 0);
        drop(accepted);
        drop(peer);
    }
}
