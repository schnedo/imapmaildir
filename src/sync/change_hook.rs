use std::{ffi::OsStr, time::Duration};

use compio::{process::Command, runtime::JoinHandle};
use crossfire::mpsc;

fn debounce<F: Future<Output = ()>>(
    duration: Duration,
    mut fun: impl FnMut() -> F + 'static,
) -> (impl FnMut() + Clone + Send + 'static, JoinHandle<()>) {
    let (call_tx, call_rx) = mpsc::bounded_async(1);
    let handle = compio::runtime::spawn(async move {
        while let Ok(()) = call_rx.recv().await {
            while let Ok(Ok(())) = compio::time::timeout(duration, call_rx.recv()).await {}
            fun().await;
        }
    });
    let debounced = move || {
        let call = call_tx.clone();
        compio::runtime::spawn(async move {
            call.send(())
                .await
                .expect("debounce channel should still be open");
        })
        .detach();
    };

    (debounced, handle)
}

fn subprocess_call(
    cmd: &[impl AsRef<OsStr>],
) -> impl FnMut() -> std::pin::Pin<Box<dyn Future<Output = ()>>> + 'static {
    let mut args = cmd.iter().map(|arg| arg.as_ref().to_os_string());
    let executable = args.next();
    let args: Vec<_> = args.collect();

    move || {
        let command = executable.as_ref().map(|executable| {
            let mut command = Command::new(executable);
            command.args(&args);
            command
        });
        Box::pin(async move {
            if let Some(mut command) = command {
                log::info!("calling on_change hook");
                if let Err(e) = command.status().await {
                    log::warn!("calling on_change hook failed: {e}");
                }
            }
        })
    }
}

pub fn on_local_change(
    cmd: &[impl AsRef<OsStr>],
) -> (impl FnMut() + Clone + Send + 'static, JoinHandle<()>) {
    debounce(Duration::from_millis(100), subprocess_call(cmd))
}

#[cfg(test)]
mod tests {
    use assertables::*;
    use rstest::*;

    use super::*;

    #[rstest]
    #[compio::test]
    async fn test_debounce_debounces() {
        let (called_tx, called_rx) = mpsc::bounded_async(2);
        let (mut debounced, handle) = debounce(Duration::from_millis(50), move || {
            let call = called_tx.clone();
            async move {
                call.send(()).await.expect("channel should be open");
            }
        });

        debounced();
        debounced();
        drop(debounced);

        assert_ok!(handle.await);
        assert_ok!(called_rx.try_recv());
        assert_err!(called_rx.recv().await);
    }
}
