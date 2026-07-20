use super::*;

fn jwt(payload: &Value) -> String {
    let header = URL_SAFE_NO_PAD.encode(br#"{"alg":"none"}"#);
    let body = URL_SAFE_NO_PAD.encode(serde_json::to_vec(payload).unwrap());
    format!("{header}.{body}.")
}

#[test]
fn jwt_exp_is_decoded() {
    let token = jwt(&json!({ "exp": 1_900_000_000u64 }));
    assert_eq!(jwt_exp_secs(&token), Some(1_900_000_000));
    assert_eq!(jwt_exp_secs("not-a-jwt"), None);
    assert_eq!(jwt_exp_secs(""), None);
}

#[test]
fn chatgpt_account_id_reads_openai_claim() {
    let token = jwt(&json!({
        "https://api.openai.com/auth": { "chatgpt_account_id": "acct-123" }
    }));
    assert_eq!(chatgpt_account_id(&token), Some("acct-123".to_string()));
    let no_claim = jwt(&json!({ "sub": "user" }));
    assert_eq!(chatgpt_account_id(&no_claim), None);
}

#[test]
fn civil_from_days_matches_known_dates() {
    assert_eq!(civil_from_days(0), (1970, 1, 1));
    assert_eq!(civil_from_days(19_723), (2024, 1, 1)); // 2024-01-01
    // rfc3339_now is well-formed and sorts lexically as a timestamp.
    let stamp = rfc3339_now();
    assert_eq!(stamp.len(), 20);
    assert!(stamp.ends_with('Z'));
    assert_eq!(&stamp[4..5], "-");
}

#[test]
fn atomic_write_replaces_file_and_leaves_no_temp() {
    let dir = std::env::temp_dir().join(format!("llmproxy-oauth-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("creds.json");
    write_json_atomic(&path, &json!({ "a": 1 })).unwrap();
    write_json_atomic(&path, &json!({ "a": 2, "b": "x" })).unwrap();
    let readback: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    assert_eq!(readback["a"], 2);
    assert_eq!(readback["b"], "x");
    assert!(!path.with_extension("json.llmproxy.tmp").exists());
    std::fs::remove_dir_all(&dir).ok();
}
