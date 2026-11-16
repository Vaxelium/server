use super::*;

#[test]
fn server_only_requests_are_refused() {
    let login = r#"{"auth":{"type":"login","email":"a@b.c","password":"pw"}}"#;
    assert!(matches!(auth_request(login), Some(Some(AuthRequest::Login { .. }))));
    // What the desktop client asks for is passed on too.
    let profile = r#"{"auth":{"type":"profile","token":"t"}}"#;
    assert!(matches!(auth_request(profile), Some(Some(AuthRequest::Profile { .. }))));
    // Requests only chat servers make are refused from the web.
    let seen = r#"{"auth":{"type":"seen","token":"t","addr":"x"}}"#;
    assert!(matches!(auth_request(seen), Some(None)));
    // Chat messages are not auth requests at all.
    assert!(auth_request(r#"{"type":"say","text":"hi"}"#).is_none());
    assert!(auth_request("not json").is_none());
}

#[test]
fn files_stay_inside_the_folder() {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        assert!(file("/../Cargo.toml").await.is_none());
        assert!(file("/emoji/../../Cargo.toml").await.is_none());
        assert!(file("//etc/passwd").await.is_none());
    });
}
