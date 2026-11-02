//! What a plugin tells the application between control calls: parameter edit
//! notifications and the per-poll event summary, shared by both plugin formats.
use std::{
    borrow::Cow,
    cell::Cell,
    marker::PhantomData,
    sync::atomic::{AtomicBool, Ordering},
};

pub use vst_audio_protocol::EditPhase;

/// One parameter gesture or value change reported by the plugin.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ParameterEdit {
    pub id: u32,
    pub phase: EditPhase,
    pub value: f64,
}

/// Notifications kept between two main-thread polls.
pub const NOTIFICATION_CAPACITY: usize = 1024;

/// Bounded and lock-free: VST 2 automation may push from the audio thread while
/// the main thread pops. A full queue drops the oldest edit and remembers the
/// overflow so the application refreshes every parameter instead of trusting a
/// gapped stream. Pushes are ignored while a [`SuppressNotifications`] guard is
/// alive on the pushing thread, so application-driven set_parameter/set_state
/// calls do not echo back as plugin edits.
#[derive(Debug)]
pub struct Notifications {
    queue: crossbeam_queue::ArrayQueue<ParameterEdit>,
    overflow: AtomicBool,
}

impl Default for Notifications {
    fn default() -> Self {
        Self::new()
    }
}

impl Notifications {
    pub fn new() -> Self {
        Self {
            queue: crossbeam_queue::ArrayQueue::new(NOTIFICATION_CAPACITY),
            overflow: AtomicBool::new(false),
        }
    }

    pub fn push(&self, edit: ParameterEdit) {
        if SUPPRESSED.get() != 0 {
            return;
        }
        if self.queue.push(edit).is_err() {
            self.overflow.store(true, Ordering::Release);
            let _ = self.queue.pop();
            let _ = self.queue.push(edit);
        }
    }

    pub fn pop(&self) -> Option<ParameterEdit> {
        self.queue.pop()
    }

    pub fn take_overflow(&self) -> bool {
        self.overflow.swap(false, Ordering::AcqRel)
    }

    pub fn drain(&self) -> Vec<ParameterEdit> {
        let mut edits = Vec::new();
        for _ in 0..NOTIFICATION_CAPACITY {
            let Some(edit) = self.pop() else {
                break;
            };
            edits.push(edit);
        }
        edits
    }
}

thread_local! {
    static SUPPRESSED: Cell<u32> = const { Cell::new(0) };
}

/// Suppresses notifications on the current thread while alive. Nests.
///
/// The guard must be dropped on the thread that entered suppression.
///
/// ```compile_fail
/// use vst_audio_worker_sys::notifications::SuppressNotifications;
/// let guard = SuppressNotifications::enter();
/// std::thread::spawn(move || drop(guard));
/// ```
#[derive(Debug)]
pub struct SuppressNotifications(PhantomData<*const ()>);

impl SuppressNotifications {
    pub fn enter() -> Self {
        SUPPRESSED.set(SUPPRESSED.get() + 1);
        Self(PhantomData)
    }
}

impl Drop for SuppressNotifications {
    fn drop(&mut self) {
        SUPPRESSED.set(SUPPRESSED.get() - 1);
    }
}

/// Everything a plugin reported since the previous poll.
#[derive(Clone, Debug, Default)]
pub struct PluginEvents {
    pub params_changed: bool,
    pub parameter_edits: Vec<ParameterEdit>,
    pub latency_changed: Option<u32>,
    pub editor_closed: bool,
    pub editor_resized: Option<(u32, u32)>,
    /// Why the plugin can no longer run; owned when it carries plugin-reported detail.
    pub fault: Option<Cow<'static, str>>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn edit(id: u32) -> ParameterEdit {
        ParameterEdit {
            id,
            phase: EditPhase::Perform,
            value: f64::from(id) / NOTIFICATION_CAPACITY as f64,
        }
    }

    #[test]
    fn overflow_drops_oldest_and_reports_once() {
        let notifications = Notifications::new();
        for id in 0..=NOTIFICATION_CAPACITY as u32 {
            notifications.push(edit(id));
        }

        assert_eq!(notifications.pop(), Some(edit(1)));
        assert!(notifications.take_overflow());
        assert!(!notifications.take_overflow());
    }

    #[test]
    fn suppression_nests_on_the_current_thread() {
        let notifications = Notifications::new();
        let outer = SuppressNotifications::enter();
        notifications.push(edit(1));
        {
            let _inner = SuppressNotifications::enter();
            notifications.push(edit(2));
        }
        notifications.push(edit(3));
        drop(outer);
        notifications.push(edit(4));

        assert_eq!(notifications.drain(), vec![edit(4)]);
    }

    #[test]
    fn drain_preserves_push_order() {
        let notifications = Notifications::new();
        notifications.push(edit(3));
        notifications.push(edit(1));
        notifications.push(edit(2));

        assert_eq!(notifications.drain(), vec![edit(3), edit(1), edit(2)]);
        assert!(notifications.drain().is_empty());
    }
}
