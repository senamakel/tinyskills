use super::*;

#[test]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
fn text_records_keep_large_unicode_scalars_contiguous() {
    let text = format!("{}🙂tail", "x".repeat(RECORD_CHUNK_BYTES - 1));
    let records = text_records(&text).unwrap();
    let reconstructed = records
        .into_iter()
        .map(|value| serde_json::from_value::<StreamRecord>(value).unwrap())
        .map(|record| match record {
            StreamRecord::Text { text } => text,
            _ => panic!("expected text record"),
        })
        .collect::<String>();
    assert_eq!(reconstructed, text);
}

#[test]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
fn summary_updates_only_from_footer_and_tracks_completion_fields() {
    let path = "stream://test";
    let mut summary = run_log::read_run_log_summary_from_text(
        &run_log::format_header(
            "workflow",
            "run-summary",
            "2026-01-01T00:00:00Z",
            &serde_json::json!({}),
            "prompt",
        ),
        path,
    )
    .unwrap();
    update_summary(&mut summary, "status: FAILED\nduration: 99 ms\n");
    assert_eq!(summary.run.status, "RUNNING");
    assert_eq!(summary.run.duration_ms, None);

    update_summary(
        &mut summary,
        "--- result ---\nstatus: FAILED\nduration: invalid ms\nfinished: 2026-01-01 UTC\n",
    );
    assert_eq!(summary.run.status, "FAILED");
    assert_eq!(summary.run.duration_ms, None);
    assert_eq!(summary.run.finished.as_deref(), Some("2026-01-01"));
    assert!(summary.complete);
}

#[test]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
fn record_values_roundtrip_versioned_headers() {
    let record = StreamRecord::Header {
        schema: 1,
        scope_id: "tenant-a".to_owned(),
        run_id: "run-roundtrip".to_owned(),
        workflow_id: "workflow-a".to_owned(),
        generation: "gen-a".to_owned(),
    };
    let encoded = record_value(&record).unwrap();
    assert!(matches!(
        serde_json::from_value::<StreamRecord>(encoded).unwrap(),
        StreamRecord::Header { schema: 1, .. }
    ));
}

#[test]
#[allow(clippy::unwrap_used, clippy::expect_used)]
fn empty_chunks_and_footer_without_status_remain_incomplete() {
    assert_eq!(text_records("").unwrap(), Vec::<serde_json::Value>::new());
    let mut summary = run_log::read_run_log_summary_from_text(
        &run_log::format_header(
            "workflow",
            "run-incomplete-footer",
            "2026-01-01T00:00:00Z",
            &serde_json::json!({}),
            "prompt",
        ),
        "stream://test",
    )
    .unwrap();
    update_summary(
        &mut summary,
        "--- result ---\nunknown: ignored\nfinished: 2026-01-01 UTC\n",
    );
    assert_eq!(summary.run.duration_ms, None);
    assert_eq!(summary.run.finished.as_deref(), Some("2026-01-01"));
    assert!(!summary.complete);
}
