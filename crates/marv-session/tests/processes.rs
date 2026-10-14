//! Marv's `tests/processes.test.ts`, in a real sandbox (skipped where bwrap doesn't work). The relay is the
//! `marv-process-fixture` binary (`marv __relay` in Marv's own binary), mounted at `/run/marv-bin`, which also serves
//! the test pages from inside the sandbox (Marv's tests used its bun there).

mod support;

use std::time::Duration;

use marv_tools::ProcessState;
use support::{Dirs, free_port, get, manager, pgrep, serve};

#[tokio::test]
async fn start_returns_the_forwarded_port_and_the_first_lines_the_hosts_127_0_0_1_gets_the_page_stop_ends_it() {
    require_bwrap!();
    let dirs = Dirs::new("start");
    let manager = manager(&dirs, 1000);
    let port = free_port();
    let started =
        manager.start(&format!("echo starting; {}", serve(&[port], "hello from the process")), None, false).await.unwrap();
    assert_eq!(started.info.name, "echo-starting");
    assert_eq!(started.info.state, ProcessState::Running);
    assert_eq!(started.info.ports, [port]);
    assert!(started.output.contains("starting"), "{}", started.output);
    assert_eq!(get(port).await.unwrap(), "hello from the process");
    let forwards = manager.forwards();
    assert_eq!(forwards.len(), 1);
    assert_eq!(forwards[0].port, port);
    assert_eq!(manager.list().len(), 1);
    assert!(manager.stop("echo-starting").await);
    assert!(manager.list().is_empty());
    assert!(get(port).await.is_err());
    assert!(!manager.stop("echo-starting").await);
    manager.close_all().await;
}

#[tokio::test]
async fn a_forwarder_that_fails_later_is_closed_and_its_port_dropped_with_a_note_marv_doesnt_crash() {
    require_bwrap!();
    let dirs = Dirs::new("fail");
    let manager = manager(&dirs, 1000);
    let port = free_port();
    let started = manager.start(&serve(&[port], "x"), Some("web"), false).await.unwrap();
    assert_eq!(started.info.ports, [port]);
    manager.fail_forward("web", port, "EMFILE: too many open files");
    let info = manager.list().into_iter().find(|p| p.name == "web").unwrap();
    assert!(info.ports.is_empty());
    assert!(info.notes.join(",").contains(&format!("port {port} stopped being forwarded (EMFILE: too many open files)")));
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(get(port).await.is_err());
    manager.close_all().await;
}

#[tokio::test]
async fn a_port_taken_on_the_host_isnt_forwarded_and_the_model_is_told() {
    require_bwrap!();
    let dirs = Dirs::new("taken");
    let manager = manager(&dirs, 1000);
    let blocker = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = blocker.local_addr().unwrap().port();
    let started = manager.start(&serve(&[port], "x"), Some("web"), false).await.unwrap();
    assert!(started.info.ports.is_empty());
    assert!(started.info.notes.join(",").contains(&format!("port {port} is in use on the user's machine")));
    manager.close_all().await;
}

#[tokio::test]
async fn at_most_4_ports_per_process_4_processes_at_once_names_are_unique() {
    require_bwrap!();
    let dirs = Dirs::new("limits");
    let manager = manager(&dirs, 1000);
    let ports: Vec<u16> = (0..5).map(|_| free_port()).collect();
    let many = manager.start(&format!("{}; sleep 100", serve(&ports, "x")), Some("many"), false).await.unwrap();
    tokio::time::sleep(Duration::from_millis(500)).await;
    let info = manager.list().into_iter().find(|p| p.name == "many").unwrap();
    assert_eq!(info.ports.len(), 4, "{info:?}");
    assert!(info.notes.join(",").contains("at most 4 ports per process"));
    assert_eq!(many.info.state, ProcessState::Running);
    manager.close_all().await;

    let quick = support::manager(&dirs, 200);
    for _ in 0..4 {
        quick.start("sleep 100", None, false).await.unwrap();
    }
    let names: Vec<String> = quick.list().into_iter().map(|p| p.name).collect();
    assert_eq!(names, ["sleep-100", "sleep-100-2", "sleep-100-3", "sleep-100-4"]);
    assert!(quick.start("sleep 100", None, false).await.unwrap_err().0.contains("At most 4 processes"));
    assert!(quick.start("true", Some("sleep-100"), false).await.unwrap_err().0.contains("already running"));
    assert!(quick.start("true", Some("-bad"), false).await.unwrap_err().0.contains("A process name uses letters"));
    quick.close_all().await;
}

#[tokio::test]
async fn two_managers_in_one_program_each_get_their_own_socket_folder_closing_one_leaves_the_others() {
    // Greptile on Marv #71: two SDK sessions in one process.
    require_bwrap!();
    let dirs = Dirs::new("two");
    let (a, b) = (manager(&dirs, 1000), manager(&dirs, 1000));
    let (port_a, port_b) = (free_port(), free_port());
    assert_eq!(a.start(&serve(&[port_a], "from a"), None, false).await.unwrap().info.ports, [port_a]);
    assert_eq!(b.start(&serve(&[port_b], "from b"), None, false).await.unwrap().info.ports, [port_b]);
    assert_eq!(get(port_b).await.unwrap(), "from b");
    a.close_all().await;
    assert_eq!(get(port_b).await.unwrap(), "from b");
    b.close_all().await;
}

#[tokio::test]
async fn an_exited_process_stays_listed_with_its_code_and_logs_until_replaced() {
    require_bwrap!();
    let dirs = Dirs::new("exited");
    let manager = manager(&dirs, 1000);
    let started = manager.start("echo bye; exit 3", Some("short"), false).await.unwrap();
    assert_eq!(started.info.state, ProcessState::Exited);
    assert_eq!(started.info.exit_code, Some(3));
    assert_eq!(started.output, "bye");
    assert_eq!(manager.logs("short", 50).unwrap(), "bye");
    let again = manager.start("echo again; exit 0", Some("short"), false).await.unwrap();
    assert_eq!(again.output, "again");
    assert_eq!(manager.list().len(), 1);
    manager.close_all().await;
}

#[tokio::test]
async fn logs_keep_the_last_megabyte_and_logs_lines_is_the_tail() {
    require_bwrap!();
    let dirs = Dirs::new("logs");
    let manager = manager(&dirs, 1000);
    manager.start("seq 1 300000; sleep 100", Some("noisy"), false).await.unwrap();
    tokio::time::sleep(Duration::from_millis(1000)).await;
    assert_eq!(manager.logs("noisy", 3).unwrap(), "299998\n299999\n300000");
    assert_eq!(manager.logs("noisy", 10_000).unwrap().split('\n').count(), 400);
    assert!(manager.logs("nope", 1).unwrap_err().0.contains("No process named \"nope\""));
    manager.close_all().await;
}

#[tokio::test]
async fn stop_and_close_all_end_everything_the_process_started() {
    require_bwrap!();
    let dirs = Dirs::new("stop");
    let manager = manager(&dirs, 1000);
    let marker = format!("marv-proc-marker-{}", std::process::id());
    manager.start(&format!("sleep 100 & sleep 101; echo {marker}"), Some("a"), false).await.unwrap();
    manager.start(&format!("sleep 102; echo {marker}"), Some("b"), false).await.unwrap();
    assert!(!pgrep(&marker).is_empty());
    manager.stop("a").await;
    let names: Vec<String> = manager.list().into_iter().map(|p| p.name).collect();
    assert_eq!(names, ["b"]);
    manager.close_all().await;
    assert!(pgrep(&marker).is_empty());
    assert!(manager.start("true", None, false).await.unwrap_err().0.contains("shut down"));
}
