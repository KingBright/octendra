#[cfg(unix)]
#[tokio::test]
async fn cleanup_checks_publication_inode_and_preserves_replacement_file() {
    use std::os::unix::fs::MetadataExt;
    let (_d,c,ws,_job,_store,mut j)=fixture().await;
    let name=format!(".remote-hosts-transfer-{}.tmp",crate::random());let path=ws.root.join(&name);std::fs::write(&path,b"owned").unwrap();
    let meta=std::fs::metadata(&path).unwrap();j.publication_temp=Some(name);j.publication_identity=Some((meta.dev(),meta.ino()));
    // Keep original inode allocated to avoid inode-reuse ambiguity in the test.
    std::fs::rename(&path,ws.root.join("kept-original")).unwrap();std::fs::write(&path,b"user replacement").unwrap();
    assert!(clean_publication(&c,&j).is_err());assert_eq!(std::fs::read(&path).unwrap(),b"user replacement");
    let meta=std::fs::metadata(&path).unwrap();j.publication_identity=Some((meta.dev(),meta.ino()));clean_publication(&c,&j).unwrap();assert!(!path.exists());
}
#[tokio::test]
async fn completed_private_files_still_count_if_cleanup_did_not_finish() {
    let (_d,c,ws,job,store,mut j)=fixture().await;
    let file=transfer_journal::open_data(&j.data_path(&c),true).unwrap();file.set_len(crate::transfers::DISK_CAP).unwrap();
    j.phase="completed".into();j.result=Some(json!({"state":"completed"}));j.save(&store).await.unwrap();
    let mut next=job;next.id=uuid::Uuid::new_v4().to_string();assert!(Journal::load_or_create(&c,&ws,&next,&store,Arc::default()).await.is_err());
}
#[tokio::test]
async fn upload_default_create_only_refuses_overwrite_and_requires_current_sha() {
    let (_d, mut c, ws, mut job, store, _j) = fixture().await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    c.gateway_url = format!("http://{}", listener.local_addr().unwrap());
    let router = Router::new().route(
        "/device/transfer-control/{id}",
        get(|| async { axum::Json(json!({"protocol":2,"revision":0,"cancel_requested":false})) }),
    );
    let server = tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });

    // Existing file on disk: omitting expected_version must refuse overwrite before network fetch.
    let initial = b"initial-forty-byte-synthetic-payload-v1\n";
    std::fs::write(ws.root.join("received.bin"), initial).unwrap();
    job.id = uuid::Uuid::new_v4().to_string();
    job.arguments.as_object_mut().unwrap().remove("expected_version");
    let omitted = run(
        &c,
        &ws,
        &job,
        &Progress::new(&job.id),
        &store,
        Arc::default(),
    )
    .await
    .unwrap();
    assert_eq!(omitted["state"], "failed");
    assert_eq!(omitted["error_code"], "version_conflict");
    assert_eq!(omitted["execution_state"], "not_started");
    assert_eq!(omitted["execution_started"], false);
    assert_eq!(omitted["destination_changed"], false);
    assert_eq!(std::fs::read(ws.root.join("received.bin")).unwrap(), initial);

    // Stale SHA-256 must also refuse overwrite and preserve destination bytes.
    job.id = uuid::Uuid::new_v4().to_string();
    job.arguments["expected_version"] = json!(crate::hash(b"stale-version"));
    let stale = run(
        &c,
        &ws,
        &job,
        &Progress::new(&job.id),
        &store,
        Arc::default(),
    )
    .await
    .unwrap();
    assert_eq!(stale["state"], "failed");
    assert_eq!(stale["error_code"], "version_conflict");
    assert_eq!(stale["destination_changed"], false);
    assert_eq!(std::fs::read(ws.root.join("received.bin")).unwrap(), initial);

    // Matching current SHA-256 replaces the file cleanly when pre-staged checkpoint is ready.
    let replacement = b"replacement-forty-eight-byte-synthetic-payload!\n";
    job.id = uuid::Uuid::new_v4().to_string();
    job.arguments["expected_version"] = json!(crate::hash(initial));
    job.arguments["sha256"] = json!(crate::hash(replacement));
    let mut replace_j = Journal::load_or_create(&c, &ws, &job, &store, Arc::default())
        .await
        .unwrap();
    prefix(&c, &mut replace_j, replacement);
    replace_j.total = Some(replacement.len() as u64);
    replace_j.save(&store).await.unwrap();
    let replaced = run(
        &c,
        &ws,
        &job,
        &Progress::new(&job.id),
        &store,
        Arc::default(),
    )
    .await
    .unwrap();
    assert_eq!(replaced["state"], "completed");
    assert_eq!(
        std::fs::read(ws.root.join("received.bin")).unwrap(),
        replacement
    );

    // Generic mid-transfer failure (e.g. sha256_mismatch after staged bytes) must remain unknown, not not_started.
    job.id = uuid::Uuid::new_v4().to_string();
    job.arguments["expected_version"] = json!(crate::hash(replacement));
    job.arguments["sha256"] = json!(crate::hash(b"different-expected-payload"));
    let mut bad_j = Journal::load_or_create(&c, &ws, &job, &store, Arc::default())
        .await
        .unwrap();
    prefix(&c, &mut bad_j, b"corrupted-staged-bytes");
    bad_j.total = Some(b"corrupted-staged-bytes".len() as u64);
    bad_j.save(&store).await.unwrap();
    let failed = run(
        &c,
        &ws,
        &job,
        &Progress::new(&job.id),
        &store,
        Arc::default(),
    )
    .await
    .unwrap();
    assert_eq!(failed["state"], "failed");
    assert_eq!(failed["error_code"], "transfer_failed");
    assert_eq!(failed["outcome"], "unknown");
    assert_eq!(failed["execution_state"], "unknown");
    assert_eq!(failed["execution_started"], true);
    assert_eq!(failed["retry_policy"], "do_not_replay_observe_original");
    server.abort();
}
