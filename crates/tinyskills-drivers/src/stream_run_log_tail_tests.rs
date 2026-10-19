use super::*;

#[tokio::test]
async fn stream_small_tail_pages_preserve_utf8_boundaries() {
    let store = store("tenant-a");
    store
        .start(run(
            "workflow-a",
            "run-utf8pages",
            "2026-01-01T00:00:00Z",
            "ascii-🙂-café-end",
        ))
        .await
        .unwrap();
    let expected = store
        .tail("run-utf8pages", None, 4096)
        .await
        .unwrap()
        .content;

    for max_bytes in 1..=3 {
        let mut cursor = None;
        let mut joined = String::new();
        let mut eof = false;
        for _ in 0..expected.len() + 4 {
            let page = store
                .tail("run-utf8pages", cursor.as_ref(), max_bytes)
                .await
                .unwrap();
            assert!(page.content.len() <= max_bytes + 3);
            assert!(std::str::from_utf8(page.content.as_bytes()).is_ok());
            joined.push_str(&page.content);
            eof = page.eof;
            if eof {
                break;
            }
            let next = page.cursor;
            assert_ne!(cursor.as_ref(), Some(&next), "tail cursor made no progress");
            cursor = Some(next);
        }
        assert!(eof, "small-page tail did not reach EOF");
        assert_eq!(joined, expected, "max_bytes={max_bytes}");
    }
}

#[tokio::test]
async fn stream_rejects_a_cursor_inside_a_utf8_scalar() {
    let store = store("tenant-a");
    store
        .start(run(
            "workflow-a",
            "run-utf8cursor",
            "2026-01-01T00:00:00Z",
            "before 🙂 after",
        ))
        .await
        .unwrap();
    let initial = store.tail("run-utf8cursor", None, 1).await.unwrap();
    let bytes = decode_hex(initial.cursor.as_opaque()).unwrap();
    let state: StreamCursorState = serde_json::from_slice(&bytes).unwrap();
    let stream = StreamRunLogStore::stream_name("run-utf8cursor").unwrap();
    let length = store.streams.len(&stream).await.unwrap();
    let mut emoji = None;
    for from in 0..length {
        let Some(entry) = store
            .streams
            .read_window(&stream, from, 1)
            .await
            .unwrap()
            .into_iter()
            .next()
        else {
            continue;
        };
        let Ok(StreamRecord::Text { text: value }) = serde_json::from_value(entry.value) else {
            continue;
        };
        if let Some(within) = value.find("🙂") {
            emoji = Some((entry.offset, within));
            break;
        }
    }
    let (offset, within) = emoji.unwrap();
    let forged = RunCursor::from_opaque(
        encode_cursor(&StreamCursorState {
            position: offset,
            within: within + 1,
            ..state
        })
        .unwrap(),
    );

    assert!(matches!(
        store.tail("run-utf8cursor", Some(&forged), 8).await,
        Err(RunLogError::InvalidCursor)
    ));
}

#[tokio::test]
async fn stream_rejects_cursor_positions_outside_the_run() {
    let store = store("tenant-a");
    store
        .start(run(
            "workflow-a",
            "run-cursor-bounds",
            "2026-01-01T00:00:00Z",
            "prompt",
        ))
        .await
        .unwrap();
    let initial = store.tail("run-cursor-bounds", None, 1).await.unwrap();
    let mut state: StreamCursorState =
        serde_json::from_slice(&decode_hex(initial.cursor.as_opaque()).unwrap()).unwrap();

    for position in [0, u64::MAX] {
        state.position = position;
        let forged = RunCursor::from_opaque(encode_cursor(&state).unwrap());
        assert!(matches!(
            store.tail("run-cursor-bounds", Some(&forged), 8).await,
            Err(RunLogError::InvalidCursor)
        ));
    }

    state.position = store
        .streams
        .len(&StreamRunLogStore::stream_name("run-cursor-bounds").unwrap())
        .await
        .unwrap()
        - 1;
    state.within = 1;
    let forged = RunCursor::from_opaque(encode_cursor(&state).unwrap());
    assert!(matches!(
        store.tail("run-cursor-bounds", Some(&forged), 8).await,
        Err(RunLogError::InvalidCursor)
    ));
}

#[tokio::test]
async fn stream_listing_ignores_names_outside_the_encoded_run_id_format() {
    let store = store("tenant-a");
    store
        .streams
        .append(
            &format!("{RUN_LOG_PREFIX}/not-hex"),
            serde_json::json!({"kind":"text"}),
        )
        .await
        .unwrap();

    assert_eq!(
        store.list(None, 10).await.unwrap(),
        Vec::<tinyskills::run_log::ScannedRun>::new()
    );
}

#[test]
fn stream_store_debug_does_not_expose_scope() {
    let store = store("private-tenant-label");
    let debug = format!("{store:?}");
    assert!(debug.contains("StreamRunLogStore"));
    assert!(!debug.contains("private-tenant-label"));
}
