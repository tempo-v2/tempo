//! Bounded loopback HTTP/1 transport. Only detached requests cross into Tempo.
use super::read_context::ContextReader;
use crate::{diagnostics, game_model::AuthoringSurface};
use http_body_util::{BodyExt, Full, Limited};
use hyper::{
    Method, StatusCode,
    body::{Bytes, Incoming},
    header::{CONTENT_LENGTH, CONTENT_TYPE, HeaderName, HeaderValue},
    http::HeaderMap,
    server::conn::http1,
    service::service_fn,
};
use hyper_util::rt::{TokioIo, TokioTimer};
use std::{
    convert::Infallible,
    io,
    net::TcpListener,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::{Receiver, RecvTimeoutError, SyncSender, sync_channel},
    },
    time::{Duration, Instant},
};
use tokio::{
    sync::{Semaphore, oneshot},
    time::timeout_at,
};

const CONNECTIONS: usize = 16;
const BODY_READERS: usize = 4;
const WAITING_REQUESTS: usize = 8;
const MAX_HEAD_BYTES: usize = 16 * 1024;
const MAX_HEAD_FIELDS: usize = 64;
const HEADER_WAIT: Duration = Duration::from_secs(2);
const ADMISSION_WAIT: Duration = Duration::from_secs(10);
const QUEUE_WAIT: Duration = Duration::from_secs(2);
const CONNECTION_WAIT: Duration = Duration::from_secs(12);
pub(super) const MAX_REPLY_BYTES: usize = 4 * 1024 * 1024;

pub(super) type Header = (HeaderName, HeaderValue);

pub(super) struct Head {
    method: Method,
    url: String,
    headers: HeaderMap,
}

impl Head {
    #[cfg(test)]
    pub(super) fn fixture(method: Method, url: &str, headers: HeaderMap) -> Self {
        Self {
            method,
            url: url.into(),
            headers,
        }
    }

    pub(super) fn method(&self) -> &Method {
        &self.method
    }
    pub(super) fn url(&self) -> &str {
        &self.url
    }
    pub(super) fn headers(&self) -> &HeaderMap {
        &self.headers
    }
    pub(super) fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name)?.to_str().ok()
    }
}

pub(super) enum Admission {
    Immediate(Response),
    Context(ContextReader),
    Foreground {
        surface: AuthoringSurface,
        body_bytes: Option<usize>,
    },
}

/// The sync owner receives bytes and a one-way response channel, never a socket.
pub(super) struct Request {
    head: Head,
    surface: AuthoringSurface,
    body: Bytes,
    delivery_id: String,
    queued: Instant,
    take_by: Instant,
    claimed_or_expired: Arc<AtomicBool>,
    claimed: Option<oneshot::Sender<()>>,
    reply: Option<oneshot::Sender<Response>>,
}

impl Request {
    pub(super) fn method(&self) -> &Method {
        self.head.method()
    }
    pub(super) fn url(&self) -> &str {
        self.head.url()
    }
    pub(super) fn header(&self, name: &str) -> Option<&str> {
        self.head.header(name)
    }
    pub(super) fn surface(&self) -> AuthoringSurface {
        self.surface
    }
    pub(super) fn body(&self) -> &[u8] {
        &self.body
    }
    pub(super) fn delivery_id(&self) -> &str {
        &self.delivery_id
    }
    pub(super) fn queue_wait(&self) -> Duration {
        self.queued.elapsed()
    }

    /// Claim the request once. Transport closure is independent after this point.
    pub(super) fn claim(&mut self) -> bool {
        if self.claimed_or_expired.swap(true, Ordering::AcqRel)
            || Instant::now() >= self.take_by
            || self.reply.as_ref().is_none_or(oneshot::Sender::is_closed)
        {
            return false;
        }
        if let Some(claimed) = self.claimed.take() {
            let _ = claimed.send(());
        }
        true
    }

    pub(super) fn send(mut self, response: Response) -> bool {
        self.reply
            .take()
            .is_some_and(|reply| reply.send(response).is_ok())
    }

    pub(super) fn busy(self) {
        let _ = self.send(busy());
    }
}

impl Drop for Request {
    fn drop(&mut self) {
        if let Some(reply) = self.reply.take() {
            let _ = reply.send(uncertain());
        }
    }
}

pub(super) struct Response(hyper::Response<Full<Bytes>>);

impl Response {
    pub(super) fn empty(status: StatusCode) -> Self {
        Self::from_data(Bytes::new()).with_status_code(status)
    }

    pub(super) fn from_data(bytes: impl Into<Bytes>) -> Self {
        Self(hyper::Response::new(Full::new(bytes.into())))
    }

    pub(super) fn with_status_code(mut self, status: StatusCode) -> Self {
        *self.0.status_mut() = status;
        self
    }

    pub(super) fn with_header(mut self, header: Header) -> Self {
        self.0.headers_mut().insert(header.0, header.1);
        self
    }

    pub(super) fn status_code(&self) -> StatusCode {
        self.0.status()
    }

    fn into_http(mut self, request_id: &str) -> hyper::Response<Full<Bytes>> {
        if let Ok(id) = HeaderValue::from_str(request_id) {
            self.0.headers_mut().insert("X-Tempo-Request-Id", id);
        }
        if let Some(id) = diagnostics::run_id().and_then(|id| HeaderValue::from_str(&id).ok()) {
            self.0.headers_mut().insert("X-Tempo-Run-Id", id);
        }
        self.0
    }
}

fn rejection(status: StatusCode, code: &str, message: &str, retryable: bool) -> Response {
    let body =
        serde_json::json!({"status":"error","code":code,"message":message,"retryable":retryable});
    Response::from_data(body.to_string())
        .with_status_code(status)
        .with_header((
            CONTENT_TYPE,
            HeaderValue::from_static("application/json; charset=utf-8"),
        ))
        .with_header((
            hyper::header::CACHE_CONTROL,
            HeaderValue::from_static("no-store"),
        ))
}

fn busy() -> Response {
    rejection(
        StatusCode::SERVICE_UNAVAILABLE,
        "request_busy",
        "Tempo is busy. Retry this request.",
        true,
    )
}

fn uncertain() -> Response {
    rejection(
        StatusCode::GATEWAY_TIMEOUT,
        "delivery_uncertain",
        "Tempo did not deliver the result. Retry the original request.",
        true,
    )
}

fn invalid_length(limit: usize) -> Response {
    rejection(
        StatusCode::PAYLOAD_TOO_LARGE,
        "invalid_command",
        &format!("Send a command body of at most {} KiB.", limit / 1024),
        false,
    )
}

fn read_timeout() -> Response {
    rejection(
        StatusCode::REQUEST_TIMEOUT,
        "request_expired",
        "The request did not arrive before its deadline.",
        true,
    )
}

pub(super) struct Server(Receiver<Request>);

impl Server {
    pub(super) fn start(
        listener: TcpListener,
        authorize: impl Fn(&Head) -> Result<Admission, StatusCode> + Send + Sync + 'static,
    ) -> io::Result<Self> {
        let (send, receive) = sync_channel(WAITING_REQUESTS);
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_io()
            .enable_time()
            .build()?;
        listener.set_nonblocking(true)?;
        let listener = {
            let _runtime = runtime.enter();
            tokio::net::TcpListener::from_std(listener)?
        };
        std::thread::Builder::new()
            .name("tempo-http-ingress".into())
            .spawn(move || {
                if let Err(error) = runtime.block_on(accept(listener, Arc::new(authorize), send)) {
                    eprintln!("Tempo HTTP ingress: {error}");
                }
            })?;
        Ok(Self(receive))
    }

    pub(super) fn recv_timeout(&self, wait: Duration) -> io::Result<Option<Request>> {
        match self.0.recv_timeout(wait) {
            Ok(request) => Ok(Some(request)),
            Err(RecvTimeoutError::Timeout) => Ok(None),
            Err(RecvTimeoutError::Disconnected) => {
                Err(io::Error::other("Tempo HTTP ingress stopped"))
            }
        }
    }
}

async fn accept(
    listener: tokio::net::TcpListener,
    authorize: Arc<impl Fn(&Head) -> Result<Admission, StatusCode> + Send + Sync + 'static>,
    queue: SyncSender<Request>,
) -> io::Result<()> {
    let connections = Arc::new(Semaphore::new(CONNECTIONS));
    let body_readers = Arc::new(Semaphore::new(BODY_READERS));
    loop {
        let (socket, _) = listener.accept().await?;
        let Ok(slot) = connections.clone().try_acquire_owned() else {
            // No task is spawned and the extra accepted socket closes immediately.
            continue;
        };
        let accepted = Instant::now();
        let authorize = authorize.clone();
        let queue = queue.clone();
        let body_readers = body_readers.clone();
        tokio::spawn(async move {
            let _slot = slot;
            let service = service_fn(move |request| {
                serve_request(
                    request,
                    accepted,
                    authorize.clone(),
                    queue.clone(),
                    body_readers.clone(),
                )
            });
            let connection = http1::Builder::new()
                .timer(TokioTimer::new())
                .keep_alive(false)
                .max_headers(MAX_HEAD_FIELDS)
                .max_buf_size(MAX_HEAD_BYTES)
                .header_read_timeout(HEADER_WAIT)
                .serve_connection(TokioIo::new(socket), service);
            // Dropping the connection future closes its socket before _slot drops.
            let _ = timeout_at((accepted + CONNECTION_WAIT).into(), connection).await;
        });
    }
}

async fn serve_request(
    request: hyper::Request<Incoming>,
    accepted: Instant,
    authorize: Arc<impl Fn(&Head) -> Result<Admission, StatusCode>>,
    queue: SyncSender<Request>,
    body_readers: Arc<Semaphore>,
) -> Result<hyper::Response<Full<Bytes>>, Infallible> {
    let (parts, body) = request.into_parts();
    let head = Head {
        method: parts.method,
        url: parts.uri.to_string(),
        headers: parts.headers,
    };
    let request_id = diagnostics::RequestContext::new(head.header("X-Tempo-Request-Id"))
        .id()
        .to_owned();
    let response = prepare(
        head,
        body,
        accepted,
        &*authorize,
        queue,
        body_readers,
        &request_id,
    )
    .await;
    Ok(response.into_http(&request_id))
}

async fn prepare(
    head: Head,
    body: Incoming,
    accepted: Instant,
    authorize: &impl Fn(&Head) -> Result<Admission, StatusCode>,
    queue: SyncSender<Request>,
    body_readers: Arc<Semaphore>,
    request_id: &str,
) -> Response {
    let deadline = accepted + ADMISSION_WAIT;
    let (surface, body_bytes) = match authorize(&head) {
        Ok(Admission::Immediate(response)) => return trace_read_response(response, request_id),
        Ok(Admission::Context(context)) => {
            return trace_read_response(context.read(deadline).await, request_id);
        }
        Ok(Admission::Foreground {
            surface,
            body_bytes,
        }) => (surface, body_bytes),
        Err(status) => return Response::empty(status),
    };
    let body = if let Some(limit) = body_bytes {
        match read_body(&head, body, limit, deadline, &body_readers, request_id).await {
            Ok(body) => body,
            Err(response) => return response,
        }
    } else {
        Bytes::new()
    };
    let queued = Instant::now();
    if queued >= deadline {
        return read_timeout();
    }

    let (reply, delivered) = oneshot::channel();
    let (claimed, admitted) = oneshot::channel();
    let claimed_or_expired = Arc::new(AtomicBool::new(false));
    let take_by = (queued + QUEUE_WAIT).min(deadline);
    let request = Request {
        head,
        surface,
        body,
        delivery_id: request_id.to_owned(),
        queued,
        take_by,
        claimed_or_expired: claimed_or_expired.clone(),
        claimed: Some(claimed),
        reply: Some(reply),
    };
    if queue.try_send(request).is_err() {
        return busy();
    }
    if timeout_at(take_by.into(), admitted).await.is_err()
        && !claimed_or_expired.swap(true, Ordering::AcqRel)
    {
        return busy();
    }
    match timeout_at(deadline.into(), delivered).await {
        Ok(Ok(response)) => response,
        _ => uncertain(),
    }
}

fn trace_read_response(response: Response, request_id: &str) -> Response {
    diagnostics::emit(
        "http.response",
        serde_json::json!({
            "request_id": request_id,
            "operation": "http.write_response",
            "status_code": response.status_code().as_u16(),
            "outcome": "queued_for_delivery",
        }),
    );
    response
}

async fn read_body(
    head: &Head,
    body: Incoming,
    limit: usize,
    deadline: Instant,
    body_readers: &Semaphore,
    request_id: &str,
) -> Result<Bytes, Response> {
    let Some(size) = head
        .header(CONTENT_LENGTH.as_str())
        .and_then(|value| value.parse::<u64>().ok())
    else {
        return Err(rejection(
            StatusCode::LENGTH_REQUIRED,
            "invalid_command",
            "Send a fixed-length command body.",
            false,
        ));
    };
    if size > limit as u64 {
        return Err(invalid_length(limit));
    }
    let Ok(_reader) = body_readers.try_acquire() else {
        return Err(busy());
    };

    let started = Instant::now();
    diagnostics::emit(
        "http.body_started",
        serde_json::json!({
            "operation":"http.read_body", "request_id":request_id, "request_bytes":size,
        }),
    );
    let body = match timeout_at(deadline.into(), Limited::new(body, limit).collect()).await {
        Ok(Ok(body)) => Ok(body.to_bytes()),
        Ok(Err(_)) => Err(rejection(
            StatusCode::BAD_REQUEST,
            "invalid_command",
            "Could not read the command.",
            false,
        )),
        Err(_) => Err(read_timeout()),
    };
    diagnostics::emit(
        "http.body_finished",
        serde_json::json!({
            "operation":"http.read_body", "request_id":request_id,
            "elapsed_ms":started.elapsed().as_millis() as u64, "bytes":body.as_ref().map_or(0, Bytes::len),
            "outcome":if body.is_ok() {"ready"} else {"rejected"},
        }),
    );
    body
}

#[cfg(test)]
mod tests {
    use super::*;

    fn packet(take_by: Instant) -> (Request, oneshot::Receiver<()>, oneshot::Receiver<Response>) {
        let (reply, delivered) = oneshot::channel();
        let (claimed, admitted) = oneshot::channel();
        let request = Request {
            head: Head {
                method: Method::POST,
                url: "/api/study".into(),
                headers: HeaderMap::new(),
            },
            surface: AuthoringSurface::Desktop,
            body: Bytes::new(),
            delivery_id: "ingress-fixture".into(),
            queued: Instant::now(),
            take_by,
            claimed_or_expired: Arc::new(AtomicBool::new(false)),
            claimed: Some(claimed),
            reply: Some(reply),
        };
        (request, admitted, delivered)
    }

    #[test]
    fn transport_admission_expired_request_cannot_enter_the_app() {
        let (mut request, mut admitted, mut delivered) = packet(Instant::now());
        assert!(!request.claim());
        request.busy();
        assert!(matches!(
            admitted.try_recv(),
            Err(oneshot::error::TryRecvError::Closed)
        ));
        assert_eq!(
            delivered
                .try_recv()
                .unwrap_or_else(|_| panic!("expected rejection"))
                .status_code(),
            StatusCode::SERVICE_UNAVAILABLE
        );
    }

    #[test]
    fn transport_admission_disconnected_delivery_cannot_reopen_a_claim() {
        let (mut request, mut admitted, delivered) = packet(Instant::now() + QUEUE_WAIT);
        assert!(request.claim());
        assert!(admitted.try_recv().is_ok());
        drop(delivered);
        assert!(!request.claim());
        assert!(!request.send(Response::empty(StatusCode::NO_CONTENT)));
    }
}
