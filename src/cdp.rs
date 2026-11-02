//! One DevTools WebSocket: request/response correlation and per-session event routing.

use std::borrow::Cow;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use futures_util::{SinkExt, StreamExt};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use serde_json::value::RawValue;
use tokio::sync::{mpsc, oneshot};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;

/// Default bound on a single protocol call so a wedged renderer cannot hang a command forever.
pub const CALL_TIMEOUT: Duration = Duration::from_secs(30);

/// Largest message accepted (full-page screenshots and heap chunks are large).
const MAX_MESSAGE: usize = 512 << 20;

/// A protocol event. `session` is `None` for browser-level events.
#[derive(Debug)]
pub struct Event {
    pub session: Option<Arc<str>>,
    pub method: String,
    pub params: Box<RawValue>,
}

impl Event {
    pub fn parse<T: DeserializeOwned>(&self) -> Result<T> {
        serde_json::from_str(self.params.get()).with_context(|| format!("decoding {}", self.method))
    }
}

/// An error reported by the browser for a call.
#[derive(Debug, Clone, Deserialize)]
pub struct ProtocolError {
    pub message: String,
}

impl std::fmt::Display for ProtocolError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for ProtocolError {}

type Pending = HashMap<u64, oneshot::Sender<Result<Box<RawValue>, ProtocolError>>>;

/// Cheap to clone; all clones share the socket.
#[derive(Clone)]
pub struct Cdp {
    inner: Arc<Inner>,
}

struct Inner {
    outgoing: mpsc::UnboundedSender<String>,
    pending: Mutex<Pending>,
    routes: Mutex<HashMap<Arc<str>, mpsc::UnboundedSender<Event>>>,
    next_id: AtomicU64,
    closed: AtomicBool,
}

#[derive(Serialize)]
struct Outgoing<'a, P> {
    id: u64,
    method: &'a str,
    #[serde(rename = "sessionId", skip_serializing_if = "Option::is_none")]
    session: Option<&'a str>,
    params: P,
}

#[derive(Deserialize)]
struct Incoming<'a> {
    id: Option<u64>,
    #[serde(borrow)]
    method: Option<Cow<'a, str>>,
    #[serde(rename = "sessionId", borrow)]
    session: Option<Cow<'a, str>>,
    #[serde(borrow)]
    params: Option<&'a RawValue>,
    #[serde(borrow)]
    result: Option<&'a RawValue>,
    error: Option<ProtocolError>,
}

impl Cdp {
    /// Connects and returns the connection plus the stream of browser-level events.
    pub async fn connect(url: &str) -> Result<(Self, mpsc::UnboundedReceiver<Event>)> {
        let config = WebSocketConfig::default().max_message_size(Some(MAX_MESSAGE)).max_frame_size(Some(MAX_MESSAGE));
        let (socket, _) = tokio::time::timeout(
            Duration::from_secs(5),
            tokio_tungstenite::connect_async_with_config(url, Some(config), true),
        )
        .await
        .context("timed out connecting to Helium's DevTools endpoint")?
        .context("connecting to Helium's DevTools endpoint")?;
        let (mut sink, mut stream) = socket.split();
        let (outgoing, mut queue) = mpsc::unbounded_channel::<String>();
        let (browser_tx, browser_rx) = mpsc::unbounded_channel();
        let inner = Arc::new(Inner {
            outgoing,
            pending: Mutex::new(HashMap::new()),
            routes: Mutex::new(HashMap::new()),
            next_id: AtomicU64::new(1),
            closed: AtomicBool::new(false),
        });
        tokio::spawn(async move {
            while let Some(text) = queue.recv().await {
                if sink.send(Message::text(text)).await.is_err() {
                    break;
                }
            }
            let _ = sink.close().await;
        });
        let reader = Arc::clone(&inner);
        tokio::spawn(async move {
            while let Some(Ok(message)) = stream.next().await {
                match message {
                    Message::Text(text) => reader.dispatch(text.as_str(), &browser_tx),
                    Message::Close(_) => break,
                    _ => {}
                }
            }
            reader.shut_down();
        });
        Ok((Self { inner }, browser_rx))
    }

    pub fn is_closed(&self) -> bool {
        self.inner.closed.load(Ordering::Acquire)
    }

    /// Sends a command and returns the raw `result` object.
    pub async fn raw(&self, method: &str, params: impl Serialize, session: Option<&str>) -> Result<Box<RawValue>> {
        self.raw_within(method, params, session, CALL_TIMEOUT).await
    }

    pub async fn raw_within(
        &self,
        method: &str,
        params: impl Serialize,
        session: Option<&str>,
        timeout: Duration,
    ) -> Result<Box<RawValue>> {
        let (reply, slot) = self.post(method, params, session)?;
        answer(reply, slot, method, timeout).await
    }

    /// Sends a command at once and returns a future for its result, so several commands can go
    /// out in order before any reply is awaited. Dropping the future abandons the reply only.
    pub fn start(
        &self,
        method: &str,
        params: &Value,
        session: Option<&str>,
        timeout: Duration,
    ) -> Result<impl Future<Output = Result<Box<RawValue>>> + Send + use<>> {
        let (reply, slot) = self.post(method, params, session)?;
        let method = method.to_owned();
        Ok(async move { answer(reply, slot, &method, timeout).await })
    }

    /// Writes a command to the socket and registers for its reply.
    fn post(&self, method: &str, params: impl Serialize, session: Option<&str>) -> Result<(Reply, Slot)> {
        if self.is_closed() {
            bail!("the connection to Helium is closed");
        }
        let id = self.inner.next_id.fetch_add(1, Ordering::Relaxed);
        let text = serde_json::to_string(&Outgoing { id, method, session, params })?;
        let (tx, rx) = oneshot::channel();
        self.inner.pending.lock().expect("pending lock").insert(id, tx);
        // Removes the entry however the call ends, including when its future is dropped.
        let slot = Slot { inner: Arc::clone(&self.inner), id };
        if self.inner.outgoing.send(text).is_err() {
            bail!("the connection to Helium is closed");
        }
        Ok((rx, slot))
    }

    /// Sends a command and decodes its result.
    pub async fn call<T: DeserializeOwned>(
        &self,
        method: &str,
        params: impl Serialize,
        session: Option<&str>,
    ) -> Result<T> {
        let raw = self.raw(method, params, session).await?;
        serde_json::from_str(raw.get()).with_context(|| format!("decoding the {method} result"))
    }

    /// Sends a command whose result is not needed.
    pub async fn send(&self, method: &str, params: impl Serialize, session: Option<&str>) -> Result<()> {
        self.raw(method, params, session).await.map(drop)
    }

    /// Delivers events of `session` to `events` until [`Cdp::unroute`].
    pub fn route(&self, session: &str, events: mpsc::UnboundedSender<Event>) {
        self.inner.routes.lock().expect("routes lock").insert(Arc::from(session), events);
    }

    pub fn unroute(&self, session: &str) {
        self.inner.routes.lock().expect("routes lock").remove(session);
    }
}

/// The receiving end of a command's reply.
type Reply = oneshot::Receiver<Result<Box<RawValue>, ProtocolError>>;

/// Waits for a posted command's reply; `slot` keeps its pending entry until then.
async fn answer(reply: Reply, slot: Slot, method: &str, timeout: Duration) -> Result<Box<RawValue>> {
    let outcome = tokio::time::timeout(timeout, reply).await;
    drop(slot);
    match outcome {
        Ok(Ok(Ok(result))) => Ok(result),
        Ok(Ok(Err(error))) => Err(anyhow::Error::new(error).context(method.to_owned())),
        Ok(Err(_)) => Err(anyhow!("{method}: the connection to Helium closed")),
        Err(_) => Err(anyhow!("{method} timed out after {}s", timeout.as_secs_f32())),
    }
}

/// A call's entry in the pending table, removed when the call ends or is abandoned.
struct Slot {
    inner: Arc<Inner>,
    id: u64,
}

impl Drop for Slot {
    fn drop(&mut self) {
        self.inner.pending.lock().expect("pending lock").remove(&self.id);
    }
}

impl Inner {
    fn dispatch(&self, text: &str, browser: &mpsc::UnboundedSender<Event>) {
        let Ok(message) = serde_json::from_str::<Incoming<'_>>(text) else {
            return;
        };
        if let Some(id) = message.id {
            let Some(waiter) = self.pending.lock().expect("pending lock").remove(&id) else {
                return;
            };
            let outcome = match (message.error, message.result) {
                (Some(error), _) => Err(error),
                (None, Some(result)) => Ok(result.to_owned()),
                (None, None) => Ok(RawValue::from_string("{}".to_owned()).expect("valid JSON")),
            };
            let _ = waiter.send(outcome);
            return;
        }
        let (Some(method), Some(params)) = (message.method, message.params) else {
            return;
        };
        match message.session {
            None => {
                let _ = browser.send(Event { session: None, method: method.into_owned(), params: params.to_owned() });
            }
            Some(session) => {
                let routes = self.routes.lock().expect("routes lock");
                if let Some((key, route)) = routes.get_key_value(session.as_ref()) {
                    let event = Event {
                        session: Some(Arc::clone(key)),
                        method: method.into_owned(),
                        params: params.to_owned(),
                    };
                    let _ = route.send(event);
                }
            }
        }
    }

    fn shut_down(&self) {
        self.closed.store(true, Ordering::Release);
        // Dropping the senders fails every waiter and ends every routed event stream.
        self.pending.lock().expect("pending lock").clear();
        self.routes.lock().expect("routes lock").clear();
    }
}
