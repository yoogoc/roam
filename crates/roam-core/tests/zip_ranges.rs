//! Run over real HTTP via the committed Postman simulation:
//! postman simulation run postman/simulations/s3-zip.sim.yaml --port 4900 --no-history --output ndjson
//! ROAM_ZIP_S3_ENDPOINT=http://127.0.0.1:4900/s3 cargo test -p roam-core --test zip_ranges -- --nocapture
//! No credentials or data from a real account are used.
use roam_core::{Profile, Rt, Vfs};

#[tokio::test]
async fn s3_zip_preview_uses_only_conditional_metadata_ranges() {
    let Ok(endpoint) = std::env::var("ROAM_ZIP_S3_ENDPOINT") else {
        eprintln!("skipping: ROAM_ZIP_S3_ENDPOINT is not set; see test module instructions");
        return;
    };
    let mut profile = Profile::new("zip-fixture", "S3 ZIP fixture", "s3://bucket/");
    for (key, value) in [
        ("endpoint", endpoint.as_str()),
        ("region", "us-east-1"),
        ("access_key_id", "fixture"),
        ("secret_access_key", "fixture-secret"),
        ("enable_virtual_host_style", "false"),
    ] {
        profile.options.insert(key.into(), value.into());
    }
    let vfs = Vfs::from_profile(Rt::from_current().unwrap(), &profile).unwrap();
    // The mock rejects full GETs, missing If-Match and payload reads, and uses
    // a directory larger than the tail so a second ranged GET is mandatory.
    for name in ["large.zip", "zip64.zip", "zip64-extra.zip"] {
        let tree = vfs.read_zip_tree(name).await.unwrap();
        assert!(tree.body.contains("docs/"));
        assert!(tree.body.contains("file-0000.txt"));
        assert!(tree.body.contains("file-1999.txt"));
        assert!(!tree.body.contains("file-2000.txt"));
        assert!(tree.truncated);
    }
    let changed = vfs.read_zip_tree("changed.zip").await.unwrap_err();
    assert_eq!(changed.kind(), Some(opendal::ErrorKind::ConditionNotMatch));
    assert!(vfs.read_zip_tree("short.zip").await.is_err());
}
