//! Bundled Oracle Instant Client test. Must run in its own process (OCI loads once per process):
//!   DBFORGE_IT=1 DBFORGE_IT_BUNDLED=1 cargo test it4 -- --nocapture

use crate::drivers::connect;
use crate::model::ConnectionProfile;

#[tokio::test(flavor = "multi_thread")]
async fn it_oracle_bundled_client() {
    if std::env::var("DBFORGE_IT").is_err() || std::env::var("DBFORGE_IT_BUNDLED").is_err() {
        return;
    }
    let bundled = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("resources").join("instantclient");
    assert!(bundled.join("oci.dll").exists(), "run `npm run fetch:oracle` first");
    // Like the app at startup: the resource dir contains `instantclient/`.
    // Like the app: Tauri hands over a verbatim (\\?\) path, which must work too.
    let verbatim = std::path::PathBuf::from(format!(r"\\?\{}", bundled.parent().unwrap().display()));
    crate::oracle_env::set_resource_dir(Some(verbatim));
    assert_eq!(crate::oracle_env::bundled_dir().as_deref(), Some(bundled.as_path()));
    let homes = crate::oracle_env::list_homes();
    assert!(homes.first().map(|h| h.bundled).unwrap_or(false), "bundled client listed first");
    eprintln!("homes: {}", homes.iter().map(|h| format!("{} [{}] tns={:?}", h.name, h.lib_dir, h.config_dir)).collect::<Vec<_>>().join(" | "));

    // Empty clientLibDir = "Bundled Instant Client" in the login form.
    let p: ConnectionProfile = serde_json::from_value(serde_json::json!({"id":"it","engine":"oracle","host":"127.0.0.1","port":51521,
        "username":"test","oracle":{"method":"direct","service":"FREEPDB1"}}))
    .unwrap();
    let s = connect(&p, "Passw0rd!", "").await.unwrap().session;
    let v = s.server_version().await.unwrap();
    eprintln!("version: {v}");
    assert!(v.contains("(bundled Instant Client)"), "{v}");
    assert!(v.contains("client 23."), "{v}");
    let mut sink = crate::drivers::CollectSink::new(0);
    s.run("select 'bundled ok' from dual", &mut sink).await.unwrap();
    assert_eq!(sink.rows[0][0], crate::model::Cell::Text("bundled ok".into()));
}
