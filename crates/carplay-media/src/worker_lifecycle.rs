use std::{
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc::Receiver,
    },
    thread::JoinHandle,
    time::Duration,
};

use crate::{Error, Result};

pub(crate) fn receive_startup_or_join<T, Wake>(
    receiver: Receiver<Result<()>>,
    timeout: Duration,
    component: &'static str,
    cancel: &AtomicBool,
    wake: Wake,
    worker: JoinHandle<T>,
) -> Result<JoinHandle<T>>
where
    Wake: FnOnce(),
{
    let startup = receiver
        .recv_timeout(timeout)
        .map_err(|error| {
            Error::Backend(format!(
                "{component} did not initialize within deadline: {error}"
            ))
        })
        .and_then(|result| result);
    if let Err(error) = startup {
        cancel.store(true, Ordering::Release);
        wake();
        worker.join().map_err(|_| {
            Error::Backend(format!(
                "{component} thread panicked while startup failure was reclaimed"
            ))
        })?;
        return Err(error);
    }
    Ok(worker)
}

#[cfg(test)]
mod tests {
    use std::{
        sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
            mpsc,
        },
        thread,
        time::Duration,
    };

    use super::*;

    #[test]
    fn stalled_startup_is_cancelled_and_joined_before_error_returns() {
        let (sender, receiver) = mpsc::sync_channel(1);
        let cancel = Arc::new(AtomicBool::new(false));
        let worker_cancel = Arc::clone(&cancel);
        let finished = Arc::new(AtomicBool::new(false));
        let worker_finished = Arc::clone(&finished);
        let worker = thread::spawn(move || {
            while !worker_cancel.load(Ordering::Acquire) {
                thread::yield_now();
            }
            drop(sender);
            worker_finished.store(true, Ordering::Release);
        });
        let result = receive_startup_or_join(
            receiver,
            Duration::from_millis(1),
            "test worker",
            &cancel,
            || {},
            worker,
        );
        assert!(result.is_err());
        assert!(cancel.load(Ordering::Acquire));
        assert!(finished.load(Ordering::Acquire));
    }

    #[test]
    fn explicit_initializer_failure_is_joined() {
        let (sender, receiver) = mpsc::sync_channel(1);
        let cancel = Arc::new(AtomicBool::new(false));
        let finished = Arc::new(AtomicBool::new(false));
        let worker_finished = Arc::clone(&finished);
        let worker = thread::spawn(move || {
            sender
                .send(Err(Error::Backend("injected failure".to_owned())))
                .expect("startup receiver");
            worker_finished.store(true, Ordering::Release);
        });
        let result = receive_startup_or_join(
            receiver,
            Duration::from_secs(1),
            "test worker",
            &cancel,
            || {},
            worker,
        );
        assert!(matches!(result, Err(Error::Backend(message)) if message == "injected failure"));
        assert!(finished.load(Ordering::Acquire));
    }
}
