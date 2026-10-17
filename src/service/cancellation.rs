use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

/// Cancellation of a read request, independent of an Agent run. Consumers
/// retain this token and await the read reply after cancelling: that reply is
/// the acknowledgement that queued/SQL/DTO work has actually stopped.
#[derive(Clone, Default, Debug)]
pub struct RequestCancellation {
    cancelled: Arc<AtomicBool>,
    #[cfg(test)]
    sql_steps: Arc<std::sync::atomic::AtomicUsize>,
}
impl RequestCancellation {
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
    }
    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }
    pub(crate) fn sql_cancelled(&self) -> bool {
        #[cfg(test)]
        self.sql_steps.fetch_add(1, Ordering::Relaxed);
        self.is_cancelled()
    }
    #[cfg(test)]
    pub(crate) fn sql_steps(&self) -> usize {
        self.sql_steps.load(Ordering::Relaxed)
    }
}
