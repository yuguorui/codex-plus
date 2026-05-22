use crate::error::ApiError;
use codex_client::Request;
use codex_client::RequestTelemetry;
use codex_client::Response;
use codex_client::RetryPolicy;
use codex_client::StreamResponse;
use codex_client::TransportError;
use codex_client::run_with_retry_with_hook;
use http::StatusCode;
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;
use tokio::time::Instant;

/// Generic telemetry.
pub trait SseTelemetry: Send + Sync {
    fn on_sse_poll(
        &self,
        result: &Result<
            Option<
                Result<
                    eventsource_stream::Event,
                    eventsource_stream::EventStreamError<TransportError>,
                >,
            >,
            tokio::time::error::Elapsed,
        >,
        duration: Duration,
    );
}

/// Telemetry for Responses WebSocket transport.
pub trait WebsocketTelemetry: Send + Sync {
    /// Reports how long it took to send the request and whether the send failed.
    fn on_ws_request(&self, duration: Duration, error: Option<&ApiError>, connection_reused: bool);

    /// Reports the poll duration for timeouts, stream errors, end of stream,
    /// and binary, close, or raw frames.
    fn on_ws_receive_failure(&self, duration: Duration);

    /// Reports each text frame and its poll duration. `kind` is `None` if decoding the stream event failed.
    fn on_ws_text_event(&self, text: &str, kind: Option<&str>, duration: Duration);
}

pub(crate) trait WithStatus {
    fn status(&self) -> StatusCode;
}

fn http_status(err: &TransportError) -> Option<StatusCode> {
    match err {
        TransportError::Http { status, .. } => Some(*status),
        _ => None,
    }
}

impl WithStatus for Response {
    fn status(&self) -> StatusCode {
        self.status
    }
}

impl WithStatus for StreamResponse {
    fn status(&self) -> StatusCode {
        self.status
    }
}

pub(crate) async fn run_with_request_telemetry<T, F, Fut>(
    policy: RetryPolicy,
    telemetry: Option<Arc<dyn RequestTelemetry>>,
    make_request: impl FnMut() -> Request,
    send: F,
) -> Result<T, TransportError>
where
    T: WithStatus,
    F: Clone + Fn(Request) -> Fut,
    Fut: Future<Output = Result<T, TransportError>>,
{
    // Attach per-attempt request telemetry for both unary and streaming HTTP calls.
    let attempt_telemetry = telemetry.clone();
    run_with_retry_with_hook(
        policy,
        make_request,
        move |req, attempt| {
            let start = Instant::now();
            let send = send.clone();
            let telemetry = attempt_telemetry.clone();
            async move {
                let result = send(req).await;
                if let Some(t) = telemetry.as_ref() {
                    let (status, err) = match &result {
                        Ok(resp) => (Some(resp.status()), None),
                        Err(err) => (http_status(err), Some(err)),
                    };
                    t.on_request(attempt, status, err, start.elapsed());
                }
                result
            }
        },
        move |next_attempt, err, delay| {
            if let Some(t) = telemetry.as_ref() {
                t.on_retry(next_attempt, http_status(err), err, delay);
            }
        },
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use codex_client::RetryAfter;
    use codex_client::RetryOn;
    use http::HeaderMap;
    use http::Method;
    use std::sync::Mutex;
    use std::sync::atomic::AtomicU64;
    use std::sync::atomic::Ordering;

    #[derive(Default)]
    struct RecordingTelemetry {
        requests: Mutex<Vec<(u64, Option<StatusCode>, bool)>>,
        retries: Mutex<Vec<(u64, Option<StatusCode>, Duration)>>,
    }

    impl RequestTelemetry for RecordingTelemetry {
        fn on_request(
            &self,
            attempt: u64,
            status: Option<StatusCode>,
            error: Option<&TransportError>,
            _duration: Duration,
        ) {
            self.requests
                .lock()
                .unwrap()
                .push((attempt, status, error.is_some()));
        }

        fn on_retry(
            &self,
            next_attempt: u64,
            status: Option<StatusCode>,
            _error: &TransportError,
            delay: Duration,
        ) {
            self.retries
                .lock()
                .unwrap()
                .push((next_attempt, status, delay));
        }
    }

    fn retry_policy(max_attempts: u64) -> RetryPolicy {
        RetryPolicy {
            max_attempts,
            base_delay: Duration::from_millis(200),
            max_delay: Duration::from_secs(60),
            retry_on: RetryOn {
                retry_429: true,
                retry_5xx: true,
                retry_transport: false,
            },
        }
    }

    fn make_request() -> Request {
        Request::new(Method::POST, "http://localhost/v1/responses".into())
    }

    fn http_error(status: StatusCode, retry_after: Option<RetryAfter>) -> TransportError {
        TransportError::Http {
            status,
            url: None,
            headers: None,
            body: None,
            retry_after,
        }
    }

    fn ok_response() -> Response {
        Response {
            status: StatusCode::OK,
            headers: HeaderMap::new(),
            body: Bytes::new(),
        }
    }

    fn fake_transport(
        retry_after: Option<Duration>,
        attempt_times: Arc<Mutex<Vec<Instant>>>,
    ) -> impl Clone + Fn(Request) -> std::future::Ready<Result<Response, TransportError>> {
        let calls = Arc::new(AtomicU64::new(0));
        move |_req: Request| {
            attempt_times.lock().unwrap().push(Instant::now());
            let call = calls.fetch_add(1, Ordering::SeqCst);
            std::future::ready(if call == 0 {
                Err(http_error(
                    StatusCode::TOO_MANY_REQUESTS,
                    retry_after.and_then(RetryAfter::from_delay),
                ))
            } else {
                Ok(ok_response())
            })
        }
    }

    #[tokio::test(start_paused = true)]
    async fn request_telemetry_retries_429_then_succeeds_with_backoff() {
        let telemetry = Arc::new(RecordingTelemetry::default());
        let attempt_times = Arc::new(Mutex::new(Vec::new()));
        let result = run_with_request_telemetry(
            retry_policy(3),
            Some(telemetry.clone()),
            make_request,
            fake_transport(None, attempt_times.clone()),
        )
        .await;

        assert_eq!(result.unwrap().status, StatusCode::OK);
        assert_eq!(
            *telemetry.requests.lock().unwrap(),
            vec![
                (0, Some(StatusCode::TOO_MANY_REQUESTS), true),
                (1, Some(StatusCode::OK), false),
            ]
        );
        let retries = telemetry.retries.lock().unwrap();
        assert_eq!(retries.len(), 1);
        let (next_attempt, status, delay) = retries[0];
        assert_eq!(next_attempt, 1);
        assert_eq!(status, Some(StatusCode::TOO_MANY_REQUESTS));
        assert!((Duration::from_millis(180)..=Duration::from_millis(220)).contains(&delay));
        let attempt_times = attempt_times.lock().unwrap();
        assert_eq!(attempt_times.len(), 2);
        assert_eq!(attempt_times[1] - attempt_times[0], delay);
    }

    #[tokio::test(start_paused = true)]
    async fn request_telemetry_honors_retry_after_deadline() {
        let telemetry = Arc::new(RecordingTelemetry::default());
        let attempt_times = Arc::new(Mutex::new(Vec::new()));
        let result = run_with_request_telemetry(
            retry_policy(3),
            Some(telemetry.clone()),
            make_request,
            fake_transport(Some(Duration::from_secs(3)), attempt_times.clone()),
        )
        .await;

        assert_eq!(result.unwrap().status, StatusCode::OK);
        let retries = telemetry.retries.lock().unwrap();
        assert_eq!(retries.len(), 1);
        let (next_attempt, status, delay) = retries[0];
        assert_eq!(next_attempt, 1);
        assert_eq!(status, Some(StatusCode::TOO_MANY_REQUESTS));
        assert_eq!(delay, Duration::from_secs(3));
        let attempt_times = attempt_times.lock().unwrap();
        assert_eq!(attempt_times.len(), 2);
        assert_eq!(attempt_times[1] - attempt_times[0], Duration::from_secs(3));
    }

    #[tokio::test(start_paused = true)]
    async fn request_telemetry_exhaustion_returns_last_error() {
        let telemetry = Arc::new(RecordingTelemetry::default());
        let result = run_with_request_telemetry(
            retry_policy(2),
            Some(telemetry.clone()),
            make_request,
            |_req: Request| {
                std::future::ready(Err::<Response, _>(http_error(
                    StatusCode::SERVICE_UNAVAILABLE,
                    None,
                )))
            },
        )
        .await;

        assert!(
            matches!(result, Err(TransportError::Http { status, .. }) if status == StatusCode::SERVICE_UNAVAILABLE)
        );
        assert_eq!(
            telemetry
                .requests
                .lock()
                .unwrap()
                .iter()
                .map(|(attempt, _, _)| *attempt)
                .collect::<Vec<_>>(),
            vec![0, 1, 2]
        );
        let retries = telemetry.retries.lock().unwrap();
        assert_eq!(
            retries
                .iter()
                .map(|(attempt, _, _)| *attempt)
                .collect::<Vec<_>>(),
            vec![1, 2]
        );
        assert!((Duration::from_millis(180)..=Duration::from_millis(220)).contains(&retries[0].2));
        assert!((Duration::from_millis(360)..=Duration::from_millis(440)).contains(&retries[1].2));
    }

    #[tokio::test(start_paused = true)]
    async fn request_telemetry_without_telemetry_still_retries() {
        let attempt_times = Arc::new(Mutex::new(Vec::new()));
        let result = run_with_request_telemetry(
            retry_policy(3),
            None,
            make_request,
            fake_transport(None, attempt_times.clone()),
        )
        .await;

        assert_eq!(result.unwrap().status, StatusCode::OK);
        assert_eq!(attempt_times.lock().unwrap().len(), 2);
    }
}
