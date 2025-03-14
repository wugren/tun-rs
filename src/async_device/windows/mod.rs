use std::future::Future;
use std::io;
use std::ops::Deref;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};

use crate::platform::DeviceImpl;
use crate::SyncDevice;

/// An async Tun/Tap device wrapper around a Tun/Tap device.
///
/// This type does not provide a split method, because this functionality can be achieved by instead wrapping the socket in an Arc.
///
/// # Streams
///
/// If you need to produce a [`Stream`], you can look at [`DeviceFramed`](crate::async_framed::DeviceFramed).
///
/// **Note:** `DeviceFramed` is only available when the `async_framed` feature is enabled.
///
/// [`Stream`]: https://docs.rs/futures/0.3/futures/stream/trait.Stream.html
pub struct AsyncDevice {
    inner: Arc<DeviceImpl>,
    recv_task_lock: Arc<Mutex<Option<RecvTask>>>,
    send_task_lock: Arc<Mutex<Option<SendTask>>>,
}
type RecvTask = blocking::Task<io::Result<(Vec<u8>, usize)>>;
type SendTask = blocking::Task<io::Result<usize>>;
impl Deref for AsyncDevice {
    type Target = DeviceImpl;
    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}
impl Drop for AsyncDevice {
    fn drop(&mut self) {
        _ = self.inner.shutdown();
    }
}
impl AsyncDevice {
    pub fn new(device: SyncDevice) -> io::Result<AsyncDevice> {
        AsyncDevice::new_dev(device.0)
    }
    /// Create a new `AsyncDevice` wrapping around a `Device`.
    pub(crate) fn new_dev(device: DeviceImpl) -> io::Result<AsyncDevice> {
        let inner = Arc::new(device);

        Ok(AsyncDevice {
            inner,
            recv_task_lock: Arc::new(Mutex::new(None)),
            send_task_lock: Arc::new(Mutex::new(None)),
        })
    }
    /// Attempts to receive a single packet from the device
    ///
    /// # Caveats
    ///
    /// Two different tasks should not call this method concurrently. Otherwise, conflicting tasks
    /// will just keep waking each other in turn, thus wasting CPU time.
    ///
    /// # Return value
    ///
    /// The function returns:
    ///
    /// * `Poll::Pending` if the device is not ready to read
    /// * `Poll::Ready(Ok(()))` reads data `buf` if the device is ready
    /// * `Poll::Ready(Err(e))` if an error is encountered.
    ///
    /// # Errors
    ///
    /// This function may encounter any standard I/O error except `WouldBlock`.
    pub fn poll_recv(&self, cx: &mut Context<'_>, mut buf: &mut [u8]) -> Poll<io::Result<usize>> {
        match self.try_recv(buf) {
            Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {}
            rs => return Poll::Ready(rs),
        }
        let mut guard = self.recv_task_lock.lock().unwrap();
        let mut task = if let Some(task) = guard.take() {
            task
        } else {
            let device = self.inner.clone();
            let size = buf.len();
            blocking::unblock(move || {
                let mut in_buf = vec![0; size];
                let n = device.recv(&mut in_buf)?;
                Ok((in_buf, n))
            })
        };
        match Pin::new(&mut task).poll(cx) {
            Poll::Ready(rs) => {
                drop(guard);
                match rs {
                    Ok((packet, n)) => {
                        let mut packet: &[u8] = &packet[..n];
                        match io::copy(&mut packet, &mut buf) {
                            Ok(n) => Poll::Ready(Ok(n as usize)),
                            Err(e) => Poll::Ready(Err(e)),
                        }
                    }
                    Err(e) => Poll::Ready(Err(e)),
                }
            }
            Poll::Pending => {
                guard.replace(task);
                Poll::Pending
            }
        }
    }
    /// Attempts to send packet to the device
    ///
    /// # Caveats
    ///
    /// Two different tasks should not call this method concurrently. Otherwise, conflicting tasks
    /// will just keep waking each other in turn, thus wasting CPU time.
    ///
    /// # Return value
    ///
    /// The function returns:
    ///
    /// * `Poll::Pending` if the device is not available to write
    /// * `Poll::Ready(Ok(n))` `n` is the number of bytes sent
    /// * `Poll::Ready(Err(e))` if an error is encountered.
    ///
    /// # Errors
    ///
    /// This function may encounter any standard I/O error except `WouldBlock`.
    pub fn poll_send(&self, cx: &mut Context<'_>, src: &[u8]) -> Poll<io::Result<usize>> {
        match self.try_send(src) {
            Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {}
            rs => return Poll::Ready(rs),
        }
        let mut guard = self.send_task_lock.lock().unwrap();
        loop {
            if let Some(mut task) = guard.take() {
                match Pin::new(&mut task).poll(cx) {
                    Poll::Ready(rs) => {
                        // If the previous write was successful, continue.
                        // Otherwise, error.
                        rs?;
                        continue;
                    }
                    Poll::Pending => {
                        guard.replace(task);
                        return Poll::Pending;
                    }
                }
            } else {
                let device = self.inner.clone();
                let buf = src.to_vec();
                let task = blocking::unblock(move || device.send(&buf));
                guard.replace(task);
                drop(guard);
                return Poll::Ready(Ok(src.len()));
            };
        }
    }

    /// Recv a packet from the device
    pub async fn recv(&self, mut buf: &mut [u8]) -> io::Result<usize> {
        match self.try_recv(buf) {
            Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {}
            rs => return rs,
        }
        let device = self.inner.clone();
        let size = buf.len();
        let (packet, n) = blocking::unblock(move || {
            let mut in_buf = vec![0; size];
            let n = device.recv(&mut in_buf)?;
            Ok::<(Vec<u8>, usize), io::Error>((in_buf, n))
        })
        .await?;
        let mut packet: &[u8] = &packet[..n];

        match io::copy(&mut packet, &mut buf) {
            Ok(n) => Ok(n as usize),
            Err(e) => Err(e),
        }
    }
    pub fn try_recv(&self, buf: &mut [u8]) -> io::Result<usize> {
        self.inner.try_recv(buf)
    }

    /// Send a packet to the device
    pub async fn send(&self, buf: &[u8]) -> io::Result<usize> {
        match self.inner.try_send(buf) {
            Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {}
            rs => return rs,
        }
        let buf = buf.to_vec();
        let device = self.inner.clone();
        blocking::unblock(move || device.send(&buf)).await
    }
    pub fn try_send(&self, buf: &[u8]) -> io::Result<usize> {
        self.inner.try_send(buf)
    }
}
