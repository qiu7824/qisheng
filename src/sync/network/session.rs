use super::super::{
    Dataset, DislikeAction, ListAction, ListData, MAX_WIRE_BYTES, PeerCapabilities, RpcFrame,
    SyncMode, SyncResult, decode_message, first_sync_dislike, first_sync_lists,
    merge_dislike_three_way, merge_lists_three_way, parse_frame, serialize_frame,
};
use super::backend::*;
use super::io_guard::ReadTrace;
use super::memory::{Budget, Credit, estimate_json_tree, value_footprint};
use super::owner::{
    self, BackendValue, Context, CryptoValue, NetworkEvent, NetworkState, data_footprint,
};
use super::protocol::cancelled;
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicU64, Ordering},
};
use std::time::{Duration, Instant};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinSet;
use tokio_tungstenite::{
    WebSocketStream,
    tungstenite::{
        Message,
        protocol::{CloseFrame, frame::coding::CloseCode},
    },
};

const BACKEND_SNAPSHOT: usize = 32 * 1024 * 1024;
const SMALL: usize = 16 * 1024;
fn index(dataset: Dataset) -> usize {
    if dataset == Dataset::List { 0 } else { 1 }
}
fn enabled(capabilities: &PeerCapabilities, dataset: Dataset) -> bool {
    match dataset {
        Dataset::List => capabilities.list.is_some(),
        Dataset::Dislike => capabilities.dislike.is_some(),
    }
}
fn feature_wire(capabilities: &PeerCapabilities) -> Value {
    let mut object = serde_json::Map::new();
    if let Some(skip) = capabilities.list {
        object.insert("list".into(), json!({"skipSnapshot":skip}));
    }
    if let Some(skip) = capabilities.dislike {
        object.insert("dislike".into(), json!({"skipSnapshot":skip}));
    }
    Value::Object(object)
}
fn intersect(a: &PeerCapabilities, b: &PeerCapabilities) -> PeerCapabilities {
    PeerCapabilities {
        list: a.list.and(b.list),
        dislike: a.dislike.and(b.dislike),
    }
}
struct Outgoing {
    wire: String,
    _global: Credit,
    _peer: Credit,
}
struct Reply {
    data: Option<Value>,
    _credit: Credit,
}
struct Request {
    frame: RpcFrame,
    _credit: Credit,
}
struct Observed {
    data: SyncData,
    revision: u64,
    sequence: u64,
    _credit: Credit,
}
struct State {
    capabilities: Mutex<PeerCapabilities>,
    ready: [AtomicBool; 2],
    synced: AtomicBool,
    observed: Mutex<[Option<Observed>; 2]>,
    covered: [AtomicU64; 2],
    is_client: bool,
}
struct PendingGuard {
    rpc: Arc<Rpc>,
    name: String,
}
impl Drop for PendingGuard {
    fn drop(&mut self) {
        self.rpc
            .pending
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .remove(&self.name);
        self.rpc
            .context
            .shared
            .pending
            .fetch_sub(1, Ordering::AcqRel);
    }
}
struct Rpc {
    context: Context,
    ticket: SessionTicket,
    outgoing: mpsc::Sender<Outgoing>,
    pending: Mutex<HashMap<String, oneshot::Sender<SyncResult<Reply>>>>,
    nonce: AtomicU64,
    bytes: Budget,
    close_code: tokio::sync::watch::Sender<Option<u16>>,
}
impl Rpc {
    async fn send(&self, frame: RpcFrame) -> SyncResult<()> {
        if !self.ticket.is_active() || self.context.shared.closing.load(Ordering::Acquire) {
            return Err(owner::error("Sync session retired"));
        }
        let estimate = match &frame {
            RpcFrame::Request { data, .. } => data.iter().map(value_footprint).sum::<usize>(),
            RpcFrame::Reply { data, .. } => data.as_ref().map(value_footprint).unwrap_or(0),
        };
        let temporary = self
            .context
            .shared
            .workspace
            .reserve(estimate.saturating_mul(4).saturating_add(SMALL))?;
        let mut global = self.context.shared.frames.reserve(MAX_WIRE_BYTES)?;
        let mut peer = self.bytes.reserve(MAX_WIRE_BYTES)?;
        #[cfg(debug_assertions)]
        {
            let mut diag = self
                .context
                .shared
                .diagnostics
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            diag.encode_started = Some(Instant::now());
            diag.encode_bytes = 0;
        }
        let wire = serialize_frame(&frame)?;
        #[cfg(debug_assertions)]
        {
            let mut diag = self
                .context
                .shared
                .diagnostics
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            diag.encode_ms = diag.encode_started.take().map_or(0, |time| {
                time.elapsed().as_millis().min(u64::MAX as u128) as u64
            });
            diag.encode_bytes = wire.len();
        }
        drop(temporary);
        global.shrink(wire.capacity())?;
        peer.shrink(wire.capacity())?;
        self.outgoing
            .try_send(Outgoing {
                wire,
                _global: global,
                _peer: peer,
            })
            .map_err(|_| owner::quota("Sync outgoing frame queue full"))
    }
    async fn call(self: &Arc<Self>, method: &str, data: Vec<Value>) -> SyncResult<Reply> {
        self.context
            .shared
            .pending
            .try_update(Ordering::AcqRel, Ordering::Acquire, |value| {
                (value < 32).then_some(value + 1)
            })
            .map_err(|_| owner::quota("Sync pending RPC capacity exhausted"))?;
        let name = format!(
            "{method}__{}_{}",
            self.ticket.session_epoch,
            self.nonce.fetch_add(1, Ordering::AcqRel)
        );
        let (send, receive) = oneshot::channel();
        self.pending
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .insert(name.clone(), send);
        let _guard = PendingGuard {
            rpc: self.clone(),
            name: name.clone(),
        };
        self.send(RpcFrame::Request {
            name,
            path: vec![method.into()],
            data,
        })
        .await?;
        tokio::select! {_=cancelled(&self.context,Some(&self.ticket))=>Err(owner::error("Sync session retired")),result=tokio::time::timeout(Duration::from_secs(120),receive)=>result.map_err(|_|owner::error("Sync RPC timed out"))?.map_err(|_|owner::error("Sync RPC reply channel closed"))?}
    }
}
pub(super) async fn serve<S>(
    socket: WebSocketStream<S>,
    trace: Option<Arc<ReadTrace>>,
    peer: PeerRecord,
    ticket: SessionTicket,
    is_client: bool,
    capabilities: PeerCapabilities,
    context: Context,
) -> SyncResult<bool>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let (sink, stream) = socket.split();
    let (outgoing, receive) = mpsc::channel(64);
    let (requests, incoming) = mpsc::channel(32);
    let (close_code, _) = tokio::sync::watch::channel(None);
    let rpc = Arc::new(Rpc {
        context: context.clone(),
        ticket: ticket.clone(),
        outgoing,
        pending: Mutex::new(HashMap::new()),
        nonce: AtomicU64::new(1),
        bytes: Budget::new(16 * 1024 * 1024),
        close_code,
    });
    let state = Arc::new(State {
        capabilities: Mutex::new(intersect(&capabilities, &peer.capabilities)),
        ready: [AtomicBool::new(false), AtomicBool::new(false)],
        synced: AtomicBool::new(false),
        observed: Mutex::new([None, None]),
        covered: [AtomicU64::new(0), AtomicU64::new(0)],
        is_client,
    });
    let mut tasks = JoinSet::new();
    let reader_rpc = rpc.clone();
    tasks.spawn(async move { read_loop(stream, trace, requests, reader_rpc).await });
    let writer_rpc = rpc.clone();
    let (done_send, writer_done) = oneshot::channel();
    tasks.spawn(async move {
        let result = write_loop(sink, receive, writer_rpc).await;
        let _ = done_send.send(());
        result
    });
    let handler_rpc = rpc.clone();
    let handler_state = state.clone();
    tasks.spawn(async move { handle_loop(incoming, handler_rpc, handler_state, is_client).await });
    let exporter_rpc = rpc.clone();
    let exporter_state = state.clone();
    tasks.spawn(async move { export_loop(exporter_rpc, exporter_state).await });
    if !is_client {
        let init_rpc = rpc.clone();
        let init_state = state.clone();
        tasks.spawn(async move {
            initialize(init_rpc.clone(), init_state).await?;
            cancelled(&init_rpc.context, Some(&init_rpc.ticket)).await;
            Ok(false)
        });
    }
    let result = tokio::select! {
        _=cancelled(&context,Some(&ticket))=>Ok(false),
        result=tasks.join_next()=>match result {
            Some(Ok(Ok(retry)))=>Ok(retry),
            Some(Ok(Err(error)))=>Err(error),
            Some(Err(_))=>Err(owner::error("Sync connection task panicked")),
            None=>Ok(false),
        }
    };
    let _ = rpc.close_code.send(Some(match &result {
        Err(_) => 4100,
        Ok(true) => 1001,
        Ok(false) => 1000,
    }));
    ticket.retire();
    let _ = tokio::time::timeout(Duration::from_millis(150), writer_done).await;
    tasks.abort_all();
    while tasks.join_next().await.is_some() {}
    rpc.pending
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .clear();
    drop(state);
    drop(rpc);
    result
}
async fn read_loop<S>(
    mut stream: futures_util::stream::SplitStream<WebSocketStream<S>>,
    trace: Option<Arc<ReadTrace>>,
    requests: mpsc::Sender<Request>,
    rpc: Arc<Rpc>,
) -> SyncResult<bool>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let mut credit = None;
    let mut partial_since = None;
    let mut alive = Instant::now();
    loop {
        if !rpc.ticket.is_active() || rpc.context.shared.closing.load(Ordering::Acquire) {
            return Ok(false);
        }
        if credit.is_none() {
            match rpc.context.shared.frames.reserve(MAX_WIRE_BYTES) {
                Ok(value) => credit = Some(value),
                Err(_) => {
                    tokio::time::sleep(Duration::from_millis(5)).await;
                    continue;
                }
            }
        }
        let received = tokio::select! {_=cancelled(&rpc.context,Some(&rpc.ticket))=>return Ok(false),result=tokio::time::timeout(Duration::from_millis(100),stream.next())=>result};
        let message = match received {
            Err(_) => {
                if alive.elapsed() > Duration::from_secs(31) {
                    return Ok(true);
                }
                if trace
                    .as_ref()
                    .is_some_and(|trace| !trace.must_retain_credit())
                {
                    credit = None;
                    partial_since = None;
                } else if trace.is_some() {
                    let start = partial_since.get_or_insert_with(Instant::now);
                    if start.elapsed() > Duration::from_secs(10) {
                        return Err(owner::error("Sync fragmented read timed out"));
                    }
                }
                continue;
            }
            Ok(None) => return Ok(true),
            Ok(Some(Err(_))) => return Ok(true),
            Ok(Some(Ok(message))) => message,
        };
        alive = Instant::now();
        match message {
            Message::Close(frame) => {
                return Ok(frame.is_none_or(|frame| {
                    !matches!(frame.code, CloseCode::Normal | CloseCode::Library(4100))
                }));
            }
            Message::Ping(_) | Message::Pong(_) => {
                if trace
                    .as_ref()
                    .is_some_and(|trace| !trace.must_retain_credit())
                {
                    credit = None;
                }
                continue;
            }
            Message::Text(text) => {
                if text == "ping" {
                    if trace
                        .as_ref()
                        .is_some_and(|trace| !trace.must_retain_credit())
                    {
                        credit = None;
                    }
                    continue;
                }
                let raw_credit = credit.take().unwrap();
                partial_since = None;
                let shared = rpc.context.shared.clone();
                let wire = text;
                let parsed = rpc
                    .context
                    .crypto
                    .call(move || {
                        let _raw = raw_credit;
                        let decoded_credit = shared
                            .workspace
                            .reserve(super::super::MAX_JSON_BYTES * 2 + SMALL)?;
                        let decoded = decode_message(&wire)?;
                        let estimate = estimate_json_tree(&decoded)?;
                        drop(decoded);
                        drop(decoded_credit);
                        let cost = estimate
                            .saturating_mul(3)
                            .saturating_add(super::super::MAX_JSON_BYTES * 2 + SMALL);
                        let start = Instant::now();
                        let mut memory = loop {
                            match shared.workspace.reserve(cost) {
                                Ok(credit) => break credit,
                                Err(error) => {
                                    if shared.closing.load(Ordering::Acquire)
                                        || start.elapsed() > Duration::from_secs(6)
                                    {
                                        return Err(error);
                                    }
                                    std::thread::sleep(Duration::from_millis(2));
                                }
                            }
                        };
                        let frame = parse_frame(&wire)?;
                        let bytes = match &frame {
                            RpcFrame::Request { data, .. } => {
                                data.iter().map(value_footprint).sum::<usize>() + SMALL
                            }
                            RpcFrame::Reply { data, .. } => {
                                data.as_ref().map(value_footprint).unwrap_or(0) + SMALL
                            }
                        };
                        memory.shrink(bytes)?;
                        Ok(CryptoValue::Frame(frame, memory))
                    })
                    .await?;
                let CryptoValue::Frame(frame, memory) = parsed else {
                    return Err(owner::error("Invalid sync frame decode result"));
                };
                match frame {
                    RpcFrame::Reply { name, error, data } => {
                        if let Some(send) = rpc
                            .pending
                            .lock()
                            .unwrap_or_else(|error| error.into_inner())
                            .remove(&name)
                        {
                            let result = if let Some(error) = error {
                                Err(owner::error(&error))
                            } else {
                                Ok(Reply {
                                    data,
                                    _credit: memory,
                                })
                            };
                            let _ = send.send(result);
                        }
                    }
                    frame @ RpcFrame::Request { .. } => {
                        requests
                            .try_send(Request {
                                frame,
                                _credit: memory,
                            })
                            .map_err(|_| owner::quota("Sync request queue full"))?;
                    }
                }
            }
            _ => return Err(owner::error("Sync requires text RPC frames")),
        }
    }
}
async fn write_loop<S>(
    mut sink: futures_util::stream::SplitSink<WebSocketStream<S>, Message>,
    mut outgoing: mpsc::Receiver<Outgoing>,
    rpc: Arc<Rpc>,
) -> SyncResult<bool>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let mut close = rpc.close_code.subscribe();
    let mut heartbeat = tokio::time::interval(Duration::from_secs(30));
    heartbeat.tick().await;
    loop {
        tokio::select! {
            biased;
            _=close.changed()=>{let code=(*close.borrow()).unwrap_or(1000);let _=tokio::time::timeout(Duration::from_millis(100),sink.send(Message::Close(Some(CloseFrame{code:CloseCode::from(code),reason:"".into()})))).await;return Ok(false);},
            _=cancelled(&rpc.context,Some(&rpc.ticket))=>{let code=(*close.borrow()).unwrap_or(1000);let _=tokio::time::timeout(Duration::from_millis(100),sink.send(Message::Close(Some(CloseFrame{code:CloseCode::from(code),reason:"".into()})))).await;return Ok(false);},
            _=heartbeat.tick()=>{tokio::time::timeout(Duration::from_secs(5),sink.send(Message::Ping(Vec::new().into()))).await.map_err(|_|owner::error("Sync heartbeat write timed out"))?.map_err(|_|owner::error("Sync heartbeat write failed"))?;},
            frame=outgoing.recv()=>{let Some(frame)=frame else{return Ok(false);};if !rpc.ticket.is_active(){return Ok(false);}
                #[cfg(debug_assertions)] {let mut diag=rpc.context.shared.diagnostics.lock().unwrap_or_else(|error|error.into_inner());diag.write_started=Some(Instant::now());diag.write_bytes=frame.wire.len();diag.write_complete=false;}
                tokio::select!{_=close.changed()=>{let code=(*close.borrow()).unwrap_or(1000);let _=tokio::time::timeout(Duration::from_millis(100),sink.send(Message::Close(Some(CloseFrame{code:CloseCode::from(code),reason:"".into()})))).await;return Ok(false);},result=tokio::time::timeout(Duration::from_secs(5),sink.send(Message::Text(frame.wire.into())))=>{
                    #[cfg(debug_assertions)] {let mut diag=rpc.context.shared.diagnostics.lock().unwrap_or_else(|error|error.into_inner());diag.write_ms=diag.write_started.take().map_or(0,|time|time.elapsed().as_millis().min(u64::MAX as u128) as u64);diag.write_complete=result.as_ref().is_ok_and(|result|result.is_ok());}
                    result.map_err(|_|owner::error("Sync slow peer write timed out"))?.map_err(|_|owner::error("Sync write failed"))?;}}
            }
        }
    }
}
async fn snapshot(rpc: &Rpc) -> SyncResult<(SyncSnapshot, Credit)> {
    let ticket = rpc.ticket.clone();
    let result = rpc
        .context
        .backend
        .call(BACKEND_SNAPSHOT, move |backend| {
            Ok(BackendValue::Snapshot(backend.snapshot(&ticket)?))
        })
        .await?;
    let BackendValue::Snapshot(snapshot) = result.value else {
        return Err(owner::error("Invalid snapshot response"));
    };
    snapshot.lists.validate()?;
    super::super::dislike_digest(&snapshot.dislike)?;
    Ok((snapshot, result.credit))
}
async fn apply(rpc: &Rpc, data: &SyncData, expected: Option<u64>) -> SyncResult<SyncReceipt> {
    let ticket = rpc.ticket.clone();
    let cost = data_footprint(data).saturating_mul(2).saturating_add(SMALL);
    let copy_credit = rpc
        .context
        .shared
        .workspace
        .reserve(data_footprint(data) + SMALL)?;
    let data = data.clone();
    let result = rpc
        .context
        .backend
        .call(cost, move |backend| {
            let _copy = copy_credit;
            Ok(BackendValue::Receipt(backend.apply_remote(
                &ticket,
                expected,
                SyncOperation::Replace(data),
            )?))
        })
        .await?;
    let BackendValue::Receipt(receipt) = result.value else {
        return Err(owner::error("Invalid commit receipt"));
    };
    Ok(receipt)
}
async fn confirm(rpc: &Rpc, data: &SyncData, revision: u64, sequence: u64) -> SyncResult<()> {
    let hash_credit = rpc
        .context
        .shared
        .workspace
        .reserve(data_footprint(data).saturating_add(super::super::MAX_JSON_BYTES))?;
    let digest = data.digest()?;
    drop(hash_credit);
    let ticket = rpc.ticket.clone();
    let cost = data_footprint(data).saturating_add(SMALL);
    let copy_credit = rpc
        .context
        .shared
        .workspace
        .reserve(data_footprint(data) + SMALL)?;
    let data = data.clone();
    rpc.context
        .backend
        .call(cost, move |backend| {
            let _copy = copy_credit;
            backend.confirm_baseline(&ticket, data, revision, sequence, &digest)?;
            Ok(BackendValue::Unit)
        })
        .await?;
    Ok(())
}
async fn choose_mode(rpc: &Rpc, dataset: Dataset) -> SyncResult<SyncMode> {
    let start = Instant::now();
    loop {
        let ticket = rpc.ticket.clone();
        let result = rpc
            .context
            .backend
            .call(SMALL, move |backend| {
                Ok(BackendValue::Mode(backend.select_mode(&ticket, dataset)?))
            })
            .await?;
        if let BackendValue::Mode(Some(mode)) = result.value {
            return Ok(mode);
        }
        if !rpc.ticket.is_active() || start.elapsed() > Duration::from_secs(120) {
            return Err(owner::error("Sync mode selection cancelled or timed out"));
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}
struct Handled {
    data: Option<Value>,
    _credit: Option<Credit>,
}
impl Handled {
    fn void() -> Self {
        Self {
            data: None,
            _credit: None,
        }
    }
    fn scalar(value: Value) -> Self {
        Self {
            data: Some(value),
            _credit: None,
        }
    }
}
async fn handle_loop(
    mut requests: mpsc::Receiver<Request>,
    rpc: Arc<Rpc>,
    state: Arc<State>,
    is_client: bool,
) -> SyncResult<bool> {
    loop {
        let request = tokio::select! {_=cancelled(&rpc.context,Some(&rpc.ticket))=>return Ok(false),request=requests.recv()=>request.ok_or_else(||owner::error("Sync request channel closed"))?};
        let RpcFrame::Request { name, path, data } = request.frame else {
            return Err(owner::error("Invalid request dispatch"));
        };
        let _small = rpc.context.shared.workspace.reserve(SMALL)?;
        let result = handle(&rpc, &state, is_client, &path[0], data).await;
        let mut response_credit = None;
        let reply = match result {
            Ok(handled) => {
                response_credit = handled._credit;
                RpcFrame::Reply {
                    name,
                    error: None,
                    data: handled.data,
                }
            }
            Err(error) => {
                rpc.context.fail(error.clone());
                RpcFrame::Reply {
                    name,
                    error: Some(error.message),
                    data: None,
                }
            }
        };
        rpc.send(reply).await?;
        drop(response_credit);
    }
}
fn dataset_footprint(snapshot: &SyncSnapshot, dataset: Dataset) -> usize {
    match dataset {
        Dataset::List => super::memory::list_footprint(&snapshot.lists),
        Dataset::Dislike => snapshot.dislike.capacity() + 64,
    }
}
async fn observe(rpc: &Rpc, state: &State, dataset: Dataset) -> SyncResult<(SyncData, Credit)> {
    let (snapshot, mut credit) = snapshot(rpc).await?;
    let revision = snapshot.repository_revision;
    let sequence = snapshot.sequence;
    let data = match dataset {
        Dataset::List => SyncData::List(snapshot.lists),
        Dataset::Dislike => SyncData::Dislike(snapshot.dislike),
    };
    credit.shrink(
        data_footprint(&data) + SMALL.min(credit.bytes().saturating_sub(data_footprint(&data))),
    )?;
    let response_credit = rpc
        .context
        .shared
        .workspace
        .reserve(data_footprint(&data).saturating_add(SMALL))?;
    let response = data.clone();
    state
        .observed
        .lock()
        .unwrap_or_else(|error| error.into_inner())[index(dataset)] = Some(Observed {
        data,
        revision,
        sequence,
        _credit: credit,
    });
    Ok((response, response_credit))
}
async fn handle(
    rpc: &Arc<Rpc>,
    state: &Arc<State>,
    is_client: bool,
    method: &str,
    mut args: Vec<Value>,
) -> SyncResult<Handled> {
    let dataset = if method.starts_with("dislike_") || method == "onDislikeSyncAction" {
        Dataset::Dislike
    } else {
        Dataset::List
    };
    match method {
        "getEnabledFeatures" if is_client => {
            let supported = &args[1];
            let configured = state
                .capabilities
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .clone();
            let features = PeerCapabilities {
                list: if supported["list"].as_u64() == Some(1) {
                    configured.list
                } else {
                    None
                },
                dislike: if supported["dislike"].as_u64() == Some(1) {
                    configured.dislike
                } else {
                    None
                },
            };
            *state
                .capabilities
                .lock()
                .unwrap_or_else(|error| error.into_inner()) = features.clone();
            Ok(Handled::scalar(feature_wire(&features)))
        }
        "onFeatureChanged" if !is_client => {
            let requested = PeerCapabilities::from_wire(&args.remove(0))?;
            let approved = state
                .capabilities
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .clone();
            *state
                .capabilities
                .lock()
                .unwrap_or_else(|error| error.into_inner()) = intersect(&approved, &requested);
            state.synced.store(false, Ordering::Release);
            let capabilities = state
                .capabilities
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .clone();
            for dataset in [Dataset::List, Dataset::Dislike] {
                if enabled(&capabilities, dataset) {
                    state.ready[index(dataset)].store(false, Ordering::Release);
                    initialize_dataset(rpc, state, dataset).await?;
                    state.ready[index(dataset)].store(true, Ordering::Release);
                }
            }
            state.synced.store(true, Ordering::Release);
            Ok(Handled::void())
        }
        "finished" if is_client => {
            let features = state
                .capabilities
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .clone();
            for dataset in [Dataset::List, Dataset::Dislike] {
                if enabled(&features, dataset)
                    && !state.ready[index(dataset)].load(Ordering::Acquire)
                {
                    return Err(owner::error("Enabled sync feature did not finish"));
                }
            }
            state.synced.store(true, Ordering::Release);
            rpc.context.state(NetworkState::Synced);
            rpc.context.event(NetworkEvent::Synced {
                peer_id: rpc.ticket.peer_id.clone(),
            });
            Ok(Handled::void())
        }
        "list_sync_get_md5" | "dislike_sync_get_md5" if is_client => {
            let (data, _data) = observe(rpc, state, dataset).await?;
            let _hash = rpc
                .context
                .shared
                .workspace
                .reserve(data_footprint(&data) + super::super::MAX_JSON_BYTES)?;
            Ok(Handled::scalar(Value::String(data.digest()?)))
        }
        "list_sync_get_list_data" | "dislike_sync_get_list_data" if is_client => {
            let (data, _data) = observe(rpc, state, dataset).await?;
            let credit = rpc
                .context
                .shared
                .workspace
                .reserve(data_footprint(&data) + SMALL)?;
            Ok(Handled {
                data: Some(data.wire()),
                _credit: Some(credit),
            })
        }
        "list_sync_get_sync_mode" | "dislike_sync_get_sync_mode" if is_client => {
            Ok(Handled::scalar(
                serde_json::to_value(choose_mode(rpc, dataset).await?)
                    .map_err(|_| owner::error("Invalid sync mode"))?,
            ))
        }
        "list_sync_set_list_data" | "dislike_sync_set_list_data" if is_client => {
            if !enabled(
                &state
                    .capabilities
                    .lock()
                    .unwrap_or_else(|error| error.into_inner()),
                dataset,
            ) {
                return Err(owner::error("Sync feature disabled"));
            }
            let value = args.remove(0);
            let mut credit = rpc
                .context
                .shared
                .workspace
                .reserve(value_footprint(&value).saturating_mul(2) + SMALL)?;
            let data = parse_data(dataset, value)?;
            credit.shrink(
                data_footprint(&data)
                    + SMALL.min(credit.bytes().saturating_sub(data_footprint(&data))),
            )?;
            let expected = state
                .observed
                .lock()
                .unwrap_or_else(|error| error.into_inner())[index(dataset)]
            .as_ref()
            .map(|observed| observed.sequence);
            let receipt = apply(rpc, &data, expected).await?;
            let _hash = rpc
                .context
                .shared
                .workspace
                .reserve(data_footprint(&data) + super::super::MAX_JSON_BYTES)?;
            if receipt.digest != data.digest()? {
                return Err(owner::error("Committed sync data differs from wire"));
            }
            state
                .observed
                .lock()
                .unwrap_or_else(|error| error.into_inner())[index(dataset)] = Some(Observed {
                data,
                revision: receipt.repository_revision,
                sequence: receipt.sequence,
                _credit: credit,
            });
            Ok(Handled::void())
        }
        "list_sync_finished" | "dislike_sync_finished" if is_client => {
            let observed = state
                .observed
                .lock()
                .unwrap_or_else(|error| error.into_inner())[index(dataset)]
            .take()
            .ok_or_else(|| owner::error("Sync feature lacks a coherent cut"))?;
            confirm(rpc, &observed.data, observed.revision, observed.sequence).await?;
            state.covered[index(dataset)].store(observed.sequence, Ordering::Release);
            state.ready[index(dataset)].store(true, Ordering::Release);
            Ok(Handled::void())
        }
        "onListSyncAction" | "onDislikeSyncAction" => {
            if !state.ready[index(dataset)].load(Ordering::Acquire) {
                return Err(owner::error("Sync feature not ready"));
            }
            let value = args.remove(0);
            let _copy = rpc
                .context
                .shared
                .workspace
                .reserve(value_footprint(&value).saturating_mul(2) + SMALL)?;
            let operation = if dataset == Dataset::List {
                SyncOperation::List(ListAction::from_wire(&value)?)
            } else {
                SyncOperation::Dislike(DislikeAction::from_wire(&value)?)
            };
            let ticket = rpc.ticket.clone();
            rpc.context
                .backend
                .call(
                    value_footprint(&value)
                        .saturating_mul(2)
                        .saturating_add(SMALL),
                    move |backend| {
                        Ok(BackendValue::Receipt(
                            backend.apply_remote(&ticket, None, operation)?,
                        ))
                    },
                )
                .await?;
            Ok(Handled::void())
        }
        _ => Err(owner::error("RPC method is not available for this role")),
    }
}
fn parse_data(dataset: Dataset, value: Value) -> SyncResult<SyncData> {
    match dataset {
        Dataset::List => Ok(SyncData::List(ListData::from_wire(&value)?)),
        Dataset::Dislike => {
            let text = value
                .as_str()
                .ok_or_else(|| owner::error("Dislike data must be a string"))?;
            super::super::dislike_digest(text)?;
            Ok(SyncData::Dislike(text.into()))
        }
    }
}
async fn remote_data(rpc: &Arc<Rpc>, dataset: Dataset) -> SyncResult<(SyncData, Credit)> {
    let method = if dataset == Dataset::List {
        "list_sync_get_list_data"
    } else {
        "dislike_sync_get_list_data"
    };
    let reply = rpc.call(method, Vec::new()).await?;
    let value = reply
        .data
        .ok_or_else(|| owner::error("Remote sync data missing"))?;
    let memory = rpc.context.shared.workspace.reserve(
        value_footprint(&value)
            .saturating_mul(2)
            .saturating_add(SMALL),
    )?;
    let data = parse_data(dataset, value)?;
    Ok((data, memory))
}
async fn initialize(rpc: Arc<Rpc>, state: Arc<State>) -> SyncResult<()> {
    rpc.context.state(NetworkState::Syncing);
    let reply = rpc
        .call(
            "getEnabledFeatures",
            vec![json!("desktop-app"), json!({"list":1,"dislike":1})],
        )
        .await?;
    let requested = PeerCapabilities::from_wire(
        reply
            .data
            .as_ref()
            .ok_or_else(|| owner::error("Remote features missing"))?,
    )?;
    let approved = state
        .capabilities
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .clone();
    let capabilities = intersect(&approved, &requested);
    *state
        .capabilities
        .lock()
        .unwrap_or_else(|error| error.into_inner()) = capabilities.clone();
    for dataset in [Dataset::List, Dataset::Dislike] {
        if !enabled(&capabilities, dataset) {
            continue;
        }
        initialize_dataset(&rpc, &state, dataset).await?;
        state.ready[index(dataset)].store(true, Ordering::Release);
    }
    rpc.call("finished", Vec::new()).await?;
    state.synced.store(true, Ordering::Release);
    rpc.context.state(NetworkState::Synced);
    rpc.context.event(NetworkEvent::Synced {
        peer_id: rpc.ticket.peer_id.clone(),
    });
    Ok(())
}
async fn initialize_dataset(rpc: &Arc<Rpc>, state: &State, dataset: Dataset) -> SyncResult<()> {
    let (local_snapshot, _local_snapshot_credit) = snapshot(rpc).await?;
    let copy = rpc
        .context
        .shared
        .workspace
        .reserve(dataset_footprint(&local_snapshot, dataset).saturating_add(SMALL))?;
    let local = local_snapshot.data(dataset);
    let ticket = rpc.ticket.clone();
    let baseline_result = rpc
        .context
        .backend
        .call(BACKEND_SNAPSHOT, move |backend| {
            Ok(BackendValue::Baseline(backend.baseline(&ticket, dataset)?))
        })
        .await?;
    let BackendValue::Baseline(baseline) = baseline_result.value else {
        return Err(owner::error("Invalid baseline response"));
    };
    let skip = match dataset {
        Dataset::List => state
            .capabilities
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .list
            .unwrap_or(false),
        Dataset::Dislike => state
            .capabilities
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .dislike
            .unwrap_or(false),
    };
    if baseline.is_some() && !skip {
        let method = if dataset == Dataset::List {
            "list_sync_get_md5"
        } else {
            "dislike_sync_get_md5"
        };
        let md5 = rpc.call(method, Vec::new()).await?;
        let _hash = rpc
            .context
            .shared
            .workspace
            .reserve(data_footprint(&local) + super::super::MAX_JSON_BYTES)?;
        if md5.data.as_ref().and_then(Value::as_str) == Some(local.digest()?.as_str()) {
            confirm(
                rpc,
                &local,
                local_snapshot.repository_revision,
                local_snapshot.sequence,
            )
            .await?;
            state.covered[index(dataset)].store(local_snapshot.sequence, Ordering::Release);
            finish_feature(rpc, dataset).await?;
            return Ok(());
        }
    }
    let (remote, _remote_credit) = remote_data(rpc, dataset).await?;
    let merge_credit = rpc.context.shared.workspace.reserve(
        data_footprint(&local)
            .saturating_add(data_footprint(&remote))
            .saturating_add(SMALL),
    )?;
    let (data, update_local, update_remote) = if let Some(baseline) = baseline.filter(|_| !skip) {
        match (&local, &remote, &baseline.data) {
            (SyncData::List(local), SyncData::List(remote), SyncData::List(base)) => (
                SyncData::List(merge_lists_three_way(
                    local,
                    remote,
                    base,
                    super::super::AddLocation::Top,
                )?),
                true,
                true,
            ),
            (SyncData::Dislike(local), SyncData::Dislike(remote), SyncData::Dislike(base)) => (
                SyncData::Dislike(merge_dislike_three_way(local, remote, base)?),
                true,
                true,
            ),
            _ => return Err(owner::error("Baseline dataset mismatch")),
        }
    } else {
        let both_nonempty = match (&local, &remote) {
            (SyncData::List(local), SyncData::List(remote)) => {
                !local.is_empty() && !remote.is_empty()
            }
            (SyncData::Dislike(local), SyncData::Dislike(remote)) => {
                !local.is_empty() && !remote.is_empty()
            }
            _ => false,
        };
        let mode = if both_nonempty {
            choose_mode(rpc, dataset).await?
        } else {
            SyncMode::Cancel
        };
        match (&local, &remote) {
            (SyncData::List(local), SyncData::List(remote)) => {
                let plan = first_sync_lists(local, remote, mode, super::super::AddLocation::Top)?;
                (
                    SyncData::List(plan.data),
                    plan.update_local,
                    plan.update_remote,
                )
            }
            (SyncData::Dislike(local), SyncData::Dislike(remote)) => {
                let plan = first_sync_dislike(local, remote, mode)?;
                (
                    SyncData::Dislike(plan.data),
                    plan.update_local,
                    plan.update_remote,
                )
            }
            _ => return Err(owner::error("Dataset mismatch")),
        }
    };
    let receipt = if update_local {
        apply(rpc, &data, Some(local_snapshot.sequence)).await?
    } else {
        let _hash = rpc
            .context
            .shared
            .workspace
            .reserve(data_footprint(&data) + super::super::MAX_JSON_BYTES)?;
        SyncReceipt {
            repository_revision: local_snapshot.repository_revision,
            sequence: local_snapshot.sequence,
            digest: data.digest()?,
        }
    };
    if update_remote {
        let method = if dataset == Dataset::List {
            "list_sync_set_list_data"
        } else {
            "dislike_sync_set_list_data"
        };
        let _wire = rpc
            .context
            .shared
            .workspace
            .reserve(data_footprint(&data) + SMALL)?;
        rpc.call(method, vec![data.wire()]).await?;
    }
    confirm(rpc, &data, receipt.repository_revision, receipt.sequence).await?;
    state.covered[index(dataset)].store(receipt.sequence, Ordering::Release);
    drop(merge_credit);
    drop(copy);
    finish_feature(rpc, dataset).await
}
async fn finish_feature(rpc: &Arc<Rpc>, dataset: Dataset) -> SyncResult<()> {
    rpc.call(
        if dataset == Dataset::List {
            "list_sync_finished"
        } else {
            "dislike_sync_finished"
        },
        Vec::new(),
    )
    .await?;
    Ok(())
}
async fn export_loop(rpc: Arc<Rpc>, state: Arc<State>) -> SyncResult<bool> {
    loop {
        tokio::select! {_=cancelled(&rpc.context,Some(&rpc.ticket))=>return Ok(false),_=tokio::time::sleep(Duration::from_millis(100))=>{}}
        if !state.synced.load(Ordering::Acquire) {
            continue;
        }
        let capabilities = state
            .capabilities
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone();
        let datasets = [Dataset::List, Dataset::Dislike]
            .into_iter()
            .filter(|dataset| enabled(&capabilities, *dataset))
            .collect::<Vec<_>>();
        if datasets.is_empty() {
            continue;
        }
        let covered = datasets
            .iter()
            .map(|dataset| state.covered[index(*dataset)].load(Ordering::Acquire))
            .min()
            .unwrap();
        let ticket = rpc.ticket.clone();
        let changes = rpc
            .context
            .backend
            .call(BACKEND_SNAPSHOT, move |backend| {
                Ok(BackendValue::Changes(
                    backend.changes_after(&ticket, covered, 32)?,
                ))
            })
            .await?;
        let BackendValue::Changes(changes_value) = changes.value else {
            return Err(owner::error("Invalid change journal response"));
        };
        let SyncChanges::Delta { records, snapshot } = changes_value else {
            if state.is_client {
                state.synced.store(false, Ordering::Release);
                rpc.call("onFeatureChanged", vec![feature_wire(&capabilities)])
                    .await?;
                state.synced.store(true, Ordering::Release);
                continue;
            }
            return Ok(true);
        };
        if snapshot.sequence <= covered {
            continue;
        }
        if records.len() > 32
            || records
                .windows(2)
                .any(|pair| pair[0].sequence >= pair[1].sequence)
            || records
                .iter()
                .any(|record| record.sequence <= covered || record.sequence > snapshot.sequence)
        {
            return Err(owner::error("Incoherent sync journal cut"));
        }
        for record in records {
            if !enabled(&capabilities, record.dataset) {
                continue;
            }
            rpc.call(
                if record.dataset == Dataset::List {
                    "onListSyncAction"
                } else {
                    "onDislikeSyncAction"
                },
                vec![record.action],
            )
            .await?;
        }
        for dataset in datasets {
            let _copy = rpc
                .context
                .shared
                .workspace
                .reserve(dataset_footprint(&snapshot, dataset) + SMALL)?;
            let data = snapshot.data(dataset);
            confirm(&rpc, &data, snapshot.repository_revision, snapshot.sequence).await?;
            state.covered[index(dataset)].store(snapshot.sequence, Ordering::Release);
        }
    }
}
