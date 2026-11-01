use super::*;
use std::io::Write;

#[test]
fn a_slow_usage_read_does_not_hold_the_other_sessions_cache_lock() {
    let reader = Arc::new(Reader::default());
    let (entered, waiting) = std::sync::mpsc::channel();
    let (release, blocked) = std::sync::mpsc::channel();
    let mut slow_file = tempfile::tempfile().unwrap();
    append(&mut slow_file, &usage(100, 20, 40, 5));
    let slow_reader = reader.clone();
    let slow = std::thread::spawn(move || {
        slow_reader.read(slow_file, "slow", Kind::Codex, |_| {
            entered.send(()).unwrap();
            blocked.recv().unwrap();
            Ok(())
        })
    });
    waiting
        .recv_timeout(std::time::Duration::from_secs(2))
        .unwrap();
    let mut file = tempfile::tempfile().unwrap();
    append(&mut file, &usage(50, 10, 0, 0));
    let (finished, result) = std::sync::mpsc::channel();
    let other = std::thread::spawn(move || {
        finished
            .send(read(&reader, &file, "other", Kind::Codex))
            .unwrap()
    });
    let snapshot = result.recv_timeout(std::time::Duration::from_secs(1));
    release.send(()).unwrap();
    slow.join().unwrap().unwrap();
    other.join().unwrap();
    assert_eq!(
        snapshot.expect("an unrelated session must not wait for the slow read")["totals"]
            ["outputTokens"],
        10
    );
}
#[test]
#[ignore = "Requires an explicit, read-only native transcript fixture"]
fn readonly_existing_native_usage() {
    let path = std::env::var("CELLS_USAGE_SAMPLE").or_else(|_| std::env::var("MINDOS_NODE_USAGE_SAMPLE")).expect("explicit sample path");
    let file = File::open(path).unwrap();
    let first = read(&Reader::default(), &file, "sample", Kind::Codex);
    assert!(!first["entries"].as_array().unwrap().is_empty());
    assert!(first["totals"]["outputTokens"].as_u64().unwrap() > 0);
    println!(
        "{}",
        json!({"complete":first["complete"],"totals":first["totals"],"records":first["entries"].as_array().unwrap().len(),
        "models":first["entries"].as_array().unwrap().iter().map(|e|json!({"providerId":e["providerId"],"modelId":e["modelId"]})).collect::<Vec<_>>()})
    );
}
fn context(turn: &str, model: &str) -> Value {
    json!({"type":"turn_context","payload":{"turn_id":turn,"model":model,"model_provider_id":"provider"}})
}
fn usage(input: u64, output: u64, cached: u64, reasoning: u64) -> Value {
    let tokens = json!({"input_tokens":input,"output_tokens":output,"cached_input_tokens":cached,"reasoning_output_tokens":reasoning});
    json!({"type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":tokens,"last_token_usage":tokens}}})
}
fn append(file: &mut File, record: &Value) {
    writeln!(file, "{record}").unwrap();
}
fn read(reader: &Reader, file: &File, session: &str, kind: Kind) -> Value {
    reader
        .read(file.try_clone().unwrap(), session, kind, |_| Ok(()))
        .unwrap()
}

#[test]
fn cumulative_replay_incremental_reads_and_restart_do_not_double_count() {
    let mut file = tempfile::tempfile().unwrap();
    let reader = Reader::default();
    append(&mut file, &context("t1", "m1"));
    append(&mut file, &usage(100, 20, 40, 5));
    append(&mut file, &usage(100, 20, 40, 5));
    let first = read(&reader, &file, "a", Kind::Codex);
    assert_eq!(first["totals"]["inputTokens"], 60);
    assert_eq!(first["totals"]["outputTokens"], 20);
    assert_eq!(
        read(&reader, &file, "a", Kind::Codex)["totals"],
        first["totals"]
    );
    append(&mut file, &context("t2", "m2"));
    append(&mut file, &usage(180, 50, 60, 15));
    let next = read(&reader, &file, "a", Kind::Codex);
    assert_eq!(next["totals"]["inputTokens"], 120);
    assert_eq!(next["totals"]["outputTokens"], 50);
    assert_eq!(next["entries"].as_array().unwrap().len(), 2);
    assert_eq!(
        read(&Reader::default(), &file, "a", Kind::Codex)["totals"],
        next["totals"]
    );
}
#[test]
fn incomplete_trailing_record_is_completed_without_losing_or_repeating_usage() {
    let mut file = tempfile::tempfile().unwrap();
    let reader = Reader::default();
    let line = format!("{}\n", usage(100, 20, 40, 5));
    let half = line.len() / 2;
    file.write_all(&line.as_bytes()[..half]).unwrap();
    assert_eq!(read(&reader, &file, "s", Kind::Codex)["complete"], false);
    file.write_all(&line.as_bytes()[half..]).unwrap();
    let next = read(&reader, &file, "s", Kind::Codex);
    assert_eq!(next["complete"], true);
    assert_eq!(next["totals"]["outputTokens"], 20);
}
#[test]
fn claude_repeated_message_frames_replace_usage_and_keep_cache_buckets_disjoint() {
    let mut file = tempfile::tempfile().unwrap();
    let reader = Reader::default();
    for output in [10, 20, 20] {
        append(
            &mut file,
            &json!({"type":"assistant","message":{"id":"m","model":"claude-model","usage":{"input_tokens":50,"output_tokens":output,"cache_read_input_tokens":30,"cache_creation_input_tokens":20}}}),
        );
    }
    let next = read(&reader, &file, "s", Kind::Claude);
    assert_eq!(next["totals"]["inputTokens"], 50);
    assert_eq!(next["totals"]["outputTokens"], 20);
    assert_eq!(next["totals"]["cacheReadTokens"], 30);
    assert_eq!(next["totals"]["cacheWriteTokens"], 20);
    assert_eq!(next["entries"].as_array().unwrap().len(), 1);
}
#[test]
fn session_caches_are_isolated_and_truncation_rebuilds() {
    let reader = Reader::default();
    let mut a = tempfile::tempfile().unwrap();
    let mut b = tempfile::tempfile().unwrap();
    append(&mut a, &usage(100, 20, 0, 0));
    append(&mut b, &usage(300, 50, 0, 0));
    assert_eq!(
        read(&reader, &a, "a", Kind::Codex)["totals"]["inputTokens"],
        100
    );
    assert_eq!(
        read(&reader, &b, "b", Kind::Codex)["totals"]["inputTokens"],
        300
    );
    a.set_len(0).unwrap();
    a.seek(SeekFrom::Start(0)).unwrap();
    append(&mut a, &usage(10, 2, 0, 0));
    assert_eq!(
        read(&reader, &a, "a", Kind::Codex)["totals"]["inputTokens"],
        10
    );
}
#[test]
fn unauthorized_records_never_advance_a_reusable_cache() {
    let reader = Reader::default();
    let mut file = tempfile::tempfile().unwrap();
    append(&mut file, &usage(100, 20, 0, 0));
    for _ in 0..2 {
        let result = reader.read(file.try_clone().unwrap(), "s", Kind::Codex, |_| {
            Err(Error::forbidden("EACCES"))
        });
        assert_eq!(result.unwrap_err().code, "EACCES");
    }
}

#[test]
fn cached_reads_only_authorize_new_records_and_invalid_tokens_remain_partial() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let reader = Reader::default();
    let mut file = tempfile::tempfile().unwrap();
    append(&mut file, &usage(100, 20, 0, 0));
    let count = AtomicUsize::new(0);
    let check = |_: &Value| {
        count.fetch_add(1, Ordering::Relaxed);
        Ok(())
    };
    reader
        .read(file.try_clone().unwrap(), "s", Kind::Codex, check)
        .unwrap();
    reader
        .read(file.try_clone().unwrap(), "s", Kind::Codex, check)
        .unwrap();
    assert_eq!(count.load(Ordering::Relaxed), 1);
    let mut invalid = usage(200, 40, 0, 0);
    invalid["payload"]["info"]["total_token_usage"]["input_tokens"] = json!(-1);
    append(&mut file, &invalid);
    let value = reader
        .read(file.try_clone().unwrap(), "s", Kind::Codex, check)
        .unwrap();
    assert_eq!(value["complete"], false);
    assert_eq!(value["totals"]["inputTokens"], 100);
    assert_eq!(count.load(Ordering::Relaxed), 2);
}

#[test]
fn historical_context_uses_last_request_instead_of_cumulative_tokens_and_clears_unknown_capacity() {
    let reader = Reader::default();
    let mut file = tempfile::tempfile().unwrap();
    let mut record = usage(900000, 4000, 10000, 1000);
    record["payload"]["info"]["model_context_window"] = json!(200000);
    record["payload"]["info"]["last_token_usage"]["total_tokens"] = json!(12000);
    append(&mut file, &record);
    let value = read(&reader, &file, "context", Kind::Codex);
    assert_eq!(value["contextWindow"]["usedTokens"], 12000);
    assert_eq!(value["contextWindow"]["limitTokens"], 200000);
    record["payload"]["info"]["model_context_window"] = Value::Null;
    append(&mut file, &record);
    assert!(read(&reader, &file, "context", Kind::Codex)["contextWindow"].is_null());
}
