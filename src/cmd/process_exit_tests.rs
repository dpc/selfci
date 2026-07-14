use super::{ProcessExitWatcher, WaitOutcome, WatchState};
use std::process::Command;
use std::time::Duration;

#[test]
fn watcher_times_out_then_observes_exit() {
    let mut child = Command::new("sleep").arg("10").spawn().unwrap();
    let watcher = match ProcessExitWatcher::arm(child.id() as libc::pid_t).unwrap() {
        WatchState::Watching(watcher) => watcher,
        WatchState::AlreadyExited => panic!("new child had already exited"),
    };

    assert_eq!(
        watcher.wait(Duration::from_millis(1)).unwrap(),
        WaitOutcome::TimedOut
    );
    child.kill().unwrap();
    assert_eq!(
        watcher.wait(Duration::from_secs(5)).unwrap(),
        WaitOutcome::Exited
    );
    child.wait().unwrap();
}

#[test]
fn watcher_queues_exit_before_wait() {
    let mut child = Command::new("sleep").arg("10").spawn().unwrap();
    let watcher = match ProcessExitWatcher::arm(child.id() as libc::pid_t).unwrap() {
        WatchState::Watching(watcher) => watcher,
        WatchState::AlreadyExited => panic!("new child had already exited"),
    };
    child.kill().unwrap();
    child.wait().unwrap();

    assert_eq!(
        watcher.wait(Duration::from_secs(5)).unwrap(),
        WaitOutcome::Exited
    );
}

#[test]
fn reaped_process_is_already_exited() {
    let mut child = Command::new("true").spawn().unwrap();
    let pid = child.id() as libc::pid_t;
    child.wait().unwrap();

    assert!(matches!(
        ProcessExitWatcher::arm(pid).unwrap(),
        WatchState::AlreadyExited
    ));
}

#[test]
fn rejects_nonpositive_process_ids() {
    for pid in [0, -1] {
        assert_eq!(
            ProcessExitWatcher::arm(pid).unwrap_err().kind(),
            std::io::ErrorKind::InvalidInput
        );
    }
}

#[test]
fn rejects_unrepresentable_timeout() {
    let mut child = Command::new("sleep").arg("10").spawn().unwrap();
    let watcher = match ProcessExitWatcher::arm(child.id() as libc::pid_t).unwrap() {
        WatchState::Watching(watcher) => watcher,
        WatchState::AlreadyExited => panic!("new child had already exited"),
    };
    assert_eq!(
        watcher.wait(Duration::MAX).unwrap_err().kind(),
        std::io::ErrorKind::InvalidInput
    );
    child.kill().unwrap();
    child.wait().unwrap();
}
