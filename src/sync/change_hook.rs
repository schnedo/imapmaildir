use std::{ffi::OsStr, time::Duration};

use tokio::{process::Command, sync::mpsc};

fn debounce(
    duration: Duration,
    mut fun: impl FnMut() + Send + 'static,
) -> impl FnMut() + Clone + Send + 'static {
    let (call_tx, mut call_rx) = mpsc::channel(1);
    tokio::spawn(async move {
        while let Some(()) = call_rx.recv().await {
            loop {
                let called = call_rx.recv();
                tokio::select! {
                    Some(()) = called => {},
                    () = tokio::time::sleep(duration) => break,
                    else => break
                }
            }
            fun();
        }
    });
    move || {
        let call = call_tx.clone();
        tokio::spawn(async move {
            call.send(())
                .await
                .expect("debounce channel should still be open");
        });
    }
}

fn subprocess_call(cmd: &[impl AsRef<OsStr>]) -> Box<dyn FnMut() + Send> {
    let mut args = cmd.iter();
    if let Some(executable) = args.next() {
        let mut cmd = Command::new(executable);
        cmd.args(args);

        Box::new(move || {
            tokio::spawn(cmd.status());
        })
    } else {
        Box::new(|| {})
    }
}
pub fn on_local_change(cmd: &[impl AsRef<OsStr>]) -> impl FnMut() + Clone + Send + 'static {
    log::info!("calling on_change hook");
    let mut cmd = subprocess_call(cmd);
    debounce(Duration::from_millis(100), move || {
        cmd();
    })
}

#[cfg(test)]
mod tests {
    use assertables::*;
    use rstest::*;

    use super::*;

    #[rstest]
    #[tokio::test]
    async fn test_debounce_debounces() {
        let (called_tx, mut called_rx) = mpsc::channel(1);
        let mut debounced = debounce(Duration::from_micros(50), move || {
            let call = called_tx.clone();
            tokio::spawn(async move {
                call.send(()).await.expect("channel should be open");
            });
        });

        debounced();
        debounced();
        drop(debounced);

        assert_some!(called_rx.recv().await);
        assert_none!(called_rx.recv().await);
    }
}
