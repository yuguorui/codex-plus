use codex_client::Request;
use codex_client::RetryOn;
use codex_client::RetryPolicy;
use codex_client::TransportError;
use codex_client::run_with_retry;
use codex_client::run_with_retry_with_hook;
use http::Method;
use http::StatusCode;
use std::future::ready;
use std::sync::Mutex;
use std::time::Duration;
use std::time::Instant;

fn retry_policy(max_attempts: u64) -> RetryPolicy {
    RetryPolicy {
        max_attempts,
        base_delay: Duration::from_millis(10),
        max_delay: Duration::from_secs(1),
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

fn http_error(status: StatusCode) -> TransportError {
    TransportError::Http {
        status,
        url: None,
        headers: None,
        body: None,
        retry_after: None,
    }
}

fn error_status(err: &TransportError) -> Option<StatusCode> {
    match err {
        TransportError::Http { status, .. } => Some(*status),
        _ => None,
    }
}

#[tokio::test]
async fn hook_receives_attempt_status_and_backoff_delay() {
    let hook_calls = Mutex::new(Vec::new());
    let started = Instant::now();
    let result = run_with_retry_with_hook(
        retry_policy(3),
        make_request,
        |_req, attempt| {
            ready(if attempt == 0 {
                Err::<(), _>(http_error(StatusCode::TOO_MANY_REQUESTS))
            } else {
                Ok(())
            })
        },
        |next_attempt, err, delay| {
            hook_calls
                .lock()
                .unwrap()
                .push((next_attempt, error_status(err), delay));
        },
    )
    .await;

    assert!(result.is_ok());
    let hook_calls = hook_calls.into_inner().unwrap();
    assert_eq!(hook_calls.len(), 1);
    let (next_attempt, status, delay) = hook_calls[0];
    assert_eq!(next_attempt, 1);
    assert_eq!(status, Some(StatusCode::TOO_MANY_REQUESTS));
    assert!((Duration::from_millis(9)..=Duration::from_millis(11)).contains(&delay));
    assert!(started.elapsed() >= Duration::from_millis(9));
}

#[tokio::test]
async fn hook_not_called_for_non_retryable_error() {
    let hook_calls = Mutex::new(Vec::new());
    let result: Result<(), _> = run_with_retry_with_hook(
        retry_policy(3),
        make_request,
        |_req, _attempt| ready(Err(http_error(StatusCode::BAD_REQUEST))),
        |next_attempt, _err, delay| hook_calls.lock().unwrap().push((next_attempt, delay)),
    )
    .await;

    assert!(
        matches!(result, Err(TransportError::Http { status, .. }) if status == StatusCode::BAD_REQUEST)
    );
    assert!(hook_calls.into_inner().unwrap().is_empty());
}

#[tokio::test]
async fn run_with_retry_still_retries_without_hook() {
    let result = run_with_retry(retry_policy(3), make_request, |_req, attempt| {
        ready(if attempt == 0 {
            Err::<(), _>(http_error(StatusCode::TOO_MANY_REQUESTS))
        } else {
            Ok(())
        })
    })
    .await;

    assert!(result.is_ok());
}
