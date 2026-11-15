//! `HARS_SHADOW=1` on the real binary: it starts, marks the shadow log, and
//! stops cleanly.

use std::net::{TcpListener, TcpStream};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

#[test]
fn shadow_mode_marks_its_log_and_stops_cleanly() {
    let port = TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let dir = std::env::temp_dir().join(format!("hars-shadow-test-{port}"));
    let _ = std::fs::remove_dir_all(&dir);
    let log = dir.join("custom-shadow.jsonl");
    let child = Command::new(env!("CARGO_BIN_EXE_hars"))
        .env_remove("MIMALLOC_ALLOW_THP")
        .env("HARS_CONFIG", &dir)
        .env("HARS_SHADOW", "1")
        .env("HARS_SHADOW_LOG", &log)
        .env("HARS_BIND", format!("127.0.0.1:{port}"))
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while TcpStream::connect(("127.0.0.1", port)).is_err() {
        assert!(Instant::now() < deadline, "hars did not start listening");
        std::thread::sleep(Duration::from_millis(5));
    }
    let text = std::fs::read_to_string(&log).expect("the shadow log was not created");
    let first: serde_json::Value = serde_json::from_str(text.lines().next().unwrap()).unwrap();
    assert_eq!(first["type"], "shadow_started");
    assert!(first["t"].as_f64().unwrap() > 1.7e9);

    assert!(
        Command::new("kill")
            .args(["-TERM", &child.id().to_string()])
            .status()
            .unwrap()
            .success()
    );
    let output = child.wait_with_output().unwrap();
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("SHADOW MODE"), "{stdout}");
    let _ = std::fs::remove_dir_all(&dir);
}
