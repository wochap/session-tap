//! Socket-level tests driving `server::handle` over a Unix socket pair.

mod common;

use common::{app, connected_handle, read_line, read_stream_line, send_request, snapshot};
use sessiontap_core::{
    SCHEMA_VERSION,
    domain::PublicStatus,
    protocol::{Request, Response, StreamEnvelope},
};
use sessiontap_storage::Storage;
use std::time::Duration;
use tokio::io::BufReader;

async fn round_trip(app: &sessiontapd::app::App, request: &Request) -> Response {
    let (mut stream, task) = connected_handle(app.clone());
    send_request(&mut stream, request).await;
    let mut reader = BufReader::new(stream);
    let response = read_line(&mut reader).await;
    task.await.unwrap().unwrap();
    response
}

#[tokio::test]
async fn health_status_and_register_round_trip() {
    let app = app(Storage::memory().unwrap());
    assert!(matches!(
        round_trip(&app, &Request::Health).await,
        Response::Health { version } if version == SCHEMA_VERSION
    ));
    assert!(matches!(
        round_trip(&app, &Request::Status).await,
        Response::Status { views, .. } if views.is_empty()
    ));
    let initial = snapshot();
    assert!(matches!(
        round_trip(
            &app,
            &Request::Register {
                snapshot: Box::new(initial.clone()),
                credential: "credential".into(),
            }
        )
        .await,
        Response::Ok
    ));
    assert!(matches!(
        round_trip(&app, &Request::Status).await,
        Response::Status { views, .. }
            if views.len() == 1 && views[0].invocation_id == initial.invocation_id
    ));
    assert!(matches!(
        round_trip(
            &app,
            &Request::Capture {
                invocation_id: initial.invocation_id,
            }
        )
        .await,
        Response::Error(error) if error.code == "request_failed"
    ));
}

#[tokio::test]
async fn subscriber_race_never_omits_child_binding() {
    let app = app(Storage::memory().unwrap());
    let expected = snapshot();
    app.register(expected.clone(), "credential").unwrap();
    let (mut stream, task) = connected_handle(app.clone());
    send_request(&mut stream, &Request::Listen).await;
    // Race the binding and the exit it enables against the listener's
    // snapshot read; the stopped view must appear in one or the other.
    let id = expected.invocation_id.clone();
    app.bind_child(&id, "credential", 42, Some("start".into()))
        .unwrap();
    app.lifecycle_exit(&id, "credential", Some(0), None)
        .unwrap();
    let mut reader = BufReader::new(stream);
    let stopped = |status| status == PublicStatus::Stopped;
    let observed = match read_stream_line(&mut reader).await {
        StreamEnvelope::Snapshot { views, .. } => views.iter().any(|view| stopped(view.status)),
        StreamEnvelope::Update { .. } => false,
    } || matches!(
        read_stream_line(&mut reader).await,
        StreamEnvelope::Update { view, .. } if stopped(view.status)
    );
    assert!(observed);
    drop(reader);
    task.await.unwrap().unwrap();
}

#[tokio::test]
async fn reconnect_starts_with_a_fresh_snapshot() {
    let app = app(Storage::memory().unwrap());
    app.register(snapshot(), "credential").unwrap();
    for expected in 1..=2 {
        let (mut stream, task) = connected_handle(app.clone());
        send_request(&mut stream, &Request::Listen).await;
        let mut reader = BufReader::new(stream);
        assert!(matches!(
            read_stream_line(&mut reader).await,
            StreamEnvelope::Snapshot { views, .. } if views.len() == expected
        ));
        drop(reader);
        task.await.unwrap().unwrap();
        app.register(snapshot(), "credential").unwrap();
    }
}

#[tokio::test]
async fn client_disconnect_cleans_up_handler() {
    let app = app(Storage::memory().unwrap());
    let (mut stream, task) = connected_handle(app);
    send_request(&mut stream, &Request::Listen).await;
    drop(stream);
    let _ = tokio::time::timeout(Duration::from_secs(1), task)
        .await
        .expect("disconnected handler should stop")
        .unwrap();
}
