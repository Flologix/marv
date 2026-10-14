//! `kill_running` on Marv's way out, in a test binary of its own: it ends every process in the program, so it can't
//! share one with tests whose processes must keep running.

mod support;

use std::time::Duration;

use support::{Dirs, manager, pgrep};

#[tokio::test]
async fn kill_running_ends_every_process_on_the_way_out() {
    require_bwrap!();
    let dirs = Dirs::new("kill");
    let manager = manager(&dirs, 300);
    let marker = format!("marv-proc-kill-{}", std::process::id());
    manager.start(&format!("sleep 103; echo {marker}"), None, false).await.unwrap();
    assert!(!pgrep(&marker).is_empty());
    marv_session::processes::kill_running();
    assert!(support::until(|| pgrep(&marker).is_empty(), Duration::from_secs(5)).await);
    manager.close_all().await;
}
