//! A reset through one client of a shared pool (spec D6, D8) gives every
//! client on it new connections: after sleep, or a dead longpoll, no vault on
//! that server keeps a connection that would hang until the read timeout.
//! Against a tiny keep-alive HTTP server that counts its connections.

use livesync_remote::{shared_http_client, Couch, RemoteConfig};
use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

/// Serves CouchDB's welcome on every request, keeping connections open;
/// returns its URL and the number of connections accepted so far.
fn server() -> (String, Arc<AtomicUsize>) {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/", l.local_addr().unwrap());
    let accepted = Arc::new(AtomicUsize::new(0));
    let count = accepted.clone();
    std::thread::spawn(move || {
        for s in l.incoming() {
            let Ok(mut s) = s else { return };
            count.fetch_add(1, Ordering::SeqCst);
            std::thread::spawn(move || {
                let mut r = BufReader::new(s.try_clone().unwrap());
                loop {
                    // One request: its head (no body for a GET).
                    let mut line = String::new();
                    loop {
                        line.clear();
                        if r.read_line(&mut line).unwrap_or(0) == 0 {
                            return;
                        }
                        if line == "\r\n" {
                            break;
                        }
                    }
                    let body = r#"{"couchdb":"Welcome"}"#;
                    let head = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n",
                        body.len()
                    );
                    if s.write_all(head.as_bytes())
                        .and_then(|_| s.write_all(body.as_bytes()))
                        .is_err()
                    {
                        return;
                    }
                }
            });
        }
    });
    (url, accepted)
}

#[tokio::test(flavor = "current_thread")]
async fn a_reset_by_one_database_renews_the_connections_of_all_on_the_server() {
    let (url, accepted) = server();
    let cfg = |db: &str| RemoteConfig {
        url: url.clone(),
        database: db.into(),
        username: None,
        password: None,
    };
    let shared = shared_http_client().unwrap();
    let a = Couch::with_shared_client(&cfg("a"), shared.clone()).unwrap();
    let b = Couch::with_shared_client(&cfg("b"), shared).unwrap();
    a.server_root().await.unwrap();
    b.server_root().await.unwrap();
    assert_eq!(accepted.load(Ordering::SeqCst), 1, "one pool, one connection");
    a.reset_connections();
    b.server_root().await.unwrap();
    assert_eq!(accepted.load(Ordering::SeqCst), 2, "b got a new connection");
    a.server_root().await.unwrap();
    assert_eq!(accepted.load(Ordering::SeqCst), 2, "and still shares it with a");
}
