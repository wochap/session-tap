//! Registry-driven regression oracle: every sanitized provider fixture is
//! normalized through the built-in adapter selected by its file prefix and
//! compared with a committed snapshot. Set `UPDATE_SNAPSHOTS=1` to rewrite the
//! snapshots after an intentional behavior change.

use serde_json::{Value, json};
use sessiontap_adapters::{AdapterRegistry, NormalizeContext};
use sessiontap_core::{
    config::Config,
    domain::{AdapterOutcome, EventEvidence, InvocationId},
};
use std::{fs, path::Path};

const INVOCATION: &str = "00000000-0000-4000-8000-000000000001";

fn payloads(fixture: &Value) -> Vec<Value> {
    match fixture {
        Value::Array(cases) => cases
            .iter()
            .map(|case| case.get("payload").cloned().unwrap_or_else(|| case.clone()))
            .collect(),
        other => vec![other.clone()],
    }
}

/// Replaces values that are generated at normalization time with stable
/// markers. Provider-supplied event ids and timestamps are kept verbatim.
fn mask(outcome: AdapterOutcome) -> Value {
    let Some(normalized) = outcome.into_event() else {
        return json!({"outcome": "ignored"});
    };
    let mut event = serde_json::to_value(&normalized.event).unwrap();
    let generated_id = normalized.event.provider_event_id.is_none();
    let generated_observed = normalized.event.observed_at == normalized.event.received_at;
    let object = event.as_object_mut().unwrap();
    if generated_id {
        object.insert("event_id".into(), json!("<generated>"));
    }
    if generated_observed {
        object.insert("observed_at".into(), json!("<received_at>"));
    }
    object.insert("received_at".into(), json!("<now>"));
    json!({
        "outcome": "event",
        "event": event,
        "status_reason": normalized.status_reason,
        "collection_context": normalized.collection_context,
    })
}

#[test]
fn provider_fixtures_match_snapshots() {
    let workspace = Path::new(env!("CARGO_MANIFEST_DIR")).to_path_buf();
    let root = workspace.join("tests");
    let update = std::env::var_os("UPDATE_SNAPSHOTS").is_some();
    let registry = AdapterRegistry::new(&Config::default());
    let invocation = InvocationId(uuid::Uuid::parse_str(INVOCATION).unwrap());
    let mut fixtures = fs::read_dir(root.join("fixtures"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "json"))
        .collect::<Vec<_>>();
    fixtures.sort();
    assert!(!fixtures.is_empty());
    fs::create_dir_all(root.join("snapshots")).unwrap();
    let mut mismatched = Vec::new();
    for path in fixtures {
        let name = path.file_name().unwrap().to_str().unwrap().to_owned();
        let provider = name.split('-').next().unwrap();
        let (adapter, _) = registry
            .resolve(provider)
            .unwrap_or_else(|| panic!("fixture {name} names no built-in provider"));
        let fixture: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        let actual = Value::Array(
            payloads(&fixture)
                .iter()
                .map(|payload| {
                    mask(
                        adapter
                            .normalize_with_evidence(
                                &invocation,
                                payload,
                                EventEvidence::managed_hook(1),
                                &NormalizeContext {
                                    workspace: Some(&workspace),
                                },
                            )
                            .unwrap(),
                    )
                })
                .collect(),
        );
        let rendered = serde_json::to_string_pretty(&actual).unwrap() + "\n";
        let snapshot = root.join("snapshots").join(&name);
        if update {
            fs::write(&snapshot, &rendered).unwrap();
            continue;
        }
        let expected = fs::read_to_string(&snapshot)
            .unwrap_or_else(|_| panic!("missing snapshot for {name}; run with UPDATE_SNAPSHOTS=1"));
        if expected != rendered {
            mismatched.push(name);
        }
    }
    assert!(mismatched.is_empty(), "snapshot drift: {mismatched:?}");
}
