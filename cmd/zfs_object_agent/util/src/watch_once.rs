use tokio::sync::watch;

pub struct Sender<T: Clone>(watch::Sender<Option<T>>);

#[derive(Clone)]
pub struct Receiver<T: Clone>(watch::Receiver<Option<T>>);

/// Create a new "watch once" channel.  This channel allows one value to be sent from one
/// sender to multiple receivers.  The value is cloned for each receiver.  This is like a
/// hybrid of tokio::sync::oneshot and tokio::sync::watch.
pub fn channel<T: Clone>() -> (Sender<T>, Receiver<T>) {
    let (tx, rx) = watch::channel(None);
    (Sender(tx), Receiver(rx))
}

impl<T: Clone> Sender<T> {
    /// Send value, consuming this Sender.  Returns an error if every receiver has been dropped.
    pub fn send(self, value: T) -> Result<(), watch::error::SendError<Option<T>>> {
        self.0.send(Some(value))
    }
}

impl<T: Clone> Receiver<T> {
    /// Receive value, consuming this Receiver.  Returns an error if the Sender was dropped before calling .send().
    pub async fn recv(mut self) -> Result<T, watch::error::RecvError> {
        /*
        if let Some(value) = self.0.borrow_and_update().as_ref().cloned() {
            return Ok(value);
        }
        // Note: "else" or "match" statement not allowed here because the
        // .borrow()'ed Ref would not be dropped until the end of the
        // else/match.
        */

        match self.0.changed().await {
            // Unwrap is safe because it's only changed to Some.
            Ok(()) => Ok(self.0.borrow().as_ref().unwrap().clone()),
            // Sender doesn't have a value for us.  Retry.
            Err(e) => Err(e),
        }
    }
}
