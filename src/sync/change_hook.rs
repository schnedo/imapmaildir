use std::{ffi::OsStr, time::Duration};

use compio::process::Command;
use crossfire::mpsc;

fn debounce(
    duration: Duration,
    mut fun: impl FnMut() + Send + 'static,
) -> impl FnMut() + Clone + Send + 'static {
    let (call_tx, call_rx) = mpsc::bounded_async(1);
    compio::runtime::spawn(async move {
        while let Ok(()) = call_rx.recv().await {
            while let Ok(Ok(())) = compio::time::timeout(duration, call_rx.recv()).await {}
            fun();
        }
    })
    .detach();
    move || {
        let call = call_tx.clone();
        compio::runtime::spawn(async move {
            call.send(())
                .await
                .expect("debounce channel should still be open");
        })
        .detach();
    }
}

fn subprocess_call(cmd: &[impl AsRef<OsStr>]) -> Box<dyn FnMut() + Send> {
    let mut args = cmd.iter().map(|arg| arg.as_ref().to_os_string());
    if let Some(executable) = args.next() {
        let args: Vec<_> = args.collect();

        Box::new(move || {
            let mut cmd = Command::new(&executable);
            cmd.args(&args);
            compio::runtime::spawn(async move {
                if let Err(e) = cmd.status().await {
                    log::warn!("calling on_change hook failed: {e}");
                }
            })
            .detach();
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
    #[compio::test]
    async fn test_debounce_debounces() {
        let (called_tx, called_rx) = mpsc::bounded_async(1);
        let mut debounced = debounce(Duration::from_micros(50), move || {
            let call = called_tx.clone();
            compio::runtime::spawn(async move {
                call.send(()).await.expect("channel should be open");
            })
            .detach();
        });

        debounced();
        debounced();
        drop(debounced);

        assert_ok!(called_rx.recv().await);
        assert_err!(called_rx.recv().await);
    }
}
