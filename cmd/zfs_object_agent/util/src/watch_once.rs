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

    pub fn subscribe(&self) -> Receiver<T> {
        Receiver(self.0.subscribe())
    }
}

impl<T: Clone> Receiver<T> {
    /// Receive value, consuming this Receiver.  Returns an error if the Sender was dropped before
    /// calling .send().
    pub async fn recv(mut self) -> Result<T, watch::error::RecvError> {
        match self.0.changed().await {
            // Unwrap is safe because it's only changed to Some
            Ok(()) => Ok(self.0.borrow().as_ref().unwrap().clone()),
            Err(e) => Err(e),
        }
    }

    pub fn same_channel(&self, other: &Receiver<T>) -> bool {
        self.0.same_channel(&other.0)
    }
}
