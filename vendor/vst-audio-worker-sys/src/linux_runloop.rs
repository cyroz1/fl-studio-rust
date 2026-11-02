#![allow(unsafe_code, non_snake_case)]
//! VST3 Linux main-thread timers and descriptor callbacks, independent of editors.
use std::{
    cell::RefCell,
    rc::Rc,
    time::{Duration, Instant},
};
use vst3::{
    ComPtr, ComRef,
    Steinberg::{Linux::*, *},
};

const LIMIT: usize = 128;
// Retained COM references live behind `Rc` so that snapshots taken while a
// RefCell is borrowed only touch Rust counts: `ComPtr::clone` would call the
// plugin's `addRef`, which may re-enter this registry.
struct Timer {
    handler: Rc<ComPtr<ITimerHandler>>,
    period: Duration,
    next: Instant,
}
#[derive(Default)]
pub(crate) struct RunLoop {
    events: RefCell<Vec<(Rc<ComPtr<IEventHandler>>, i32)>>,
    timers: RefCell<Vec<Timer>>,
}
impl RunLoop {
    pub unsafe fn register_event(&self, pointer: *mut IEventHandler, fd: i32) -> tresult {
        if fd < 0 {
            return kInvalidArgument;
        }
        // SAFETY: SDK supplies a live interface; retain it for registration lifetime.
        let Some(handler) = (unsafe { ComRef::from_raw(pointer) }) else {
            return kInvalidArgument;
        };
        // addRef runs here, before any borrow, so a re-entrant plugin cannot
        // observe a live RefCell borrow; a rejected retain is released after it.
        let retained = Rc::new(handler.to_com_ptr());
        let mut events = self.events.borrow_mut();
        if events.len() >= LIMIT || events.iter().any(|(p, _)| p.as_ptr() == pointer) {
            return kResultFalse;
        }
        events.push((retained, fd));
        kResultOk
    }
    pub fn unregister_event(&self, pointer: *mut IEventHandler) -> tresult {
        // The removed entry drops (and may release the plugin's reference) only
        // after the borrow ends.
        let removed = {
            let mut events = self.events.borrow_mut();
            let index = events.iter().position(|(p, _)| p.as_ptr() == pointer);
            index.map(|index| events.swap_remove(index))
        };
        if removed.is_some() {
            kResultOk
        } else {
            kResultFalse
        }
    }
    pub unsafe fn register_timer(&self, pointer: *mut ITimerHandler, milliseconds: u64) -> tresult {
        if milliseconds == 0 || milliseconds > 86_400_000 {
            return kInvalidArgument;
        }
        // SAFETY: SDK supplies a live timer interface; retain until unregistered.
        let Some(handler) = (unsafe { ComRef::from_raw(pointer) }) else {
            return kInvalidArgument;
        };
        // See register_event: retain before borrowing.
        let retained = Rc::new(handler.to_com_ptr());
        let mut timers = self.timers.borrow_mut();
        if timers.len() >= LIMIT || timers.iter().any(|t| t.handler.as_ptr() == pointer) {
            return kResultFalse;
        }
        let period = Duration::from_millis(milliseconds);
        timers.push(Timer {
            handler: retained,
            period,
            next: Instant::now() + period,
        });
        kResultOk
    }
    pub fn unregister_timer(&self, pointer: *mut ITimerHandler) -> tresult {
        let removed = {
            let mut timers = self.timers.borrow_mut();
            let index = timers.iter().position(|t| t.handler.as_ptr() == pointer);
            index.map(|index| timers.swap_remove(index))
        };
        if removed.is_some() {
            kResultOk
        } else {
            kResultFalse
        }
    }
    pub fn has_event(&self, fd: i32) -> bool {
        self.events
            .borrow()
            .iter()
            .any(|(_, registered)| *registered == fd)
    }
    pub fn poll(&self) {
        // Main-thread work may allocate. Snapshot the Rc handles (no plugin call)
        // and end all RefCell borrows before calling foreign code, which may
        // register or unregister handlers.
        let events = self.events.borrow().clone();
        let mut fds: Vec<_> = events
            .iter()
            .map(|(_, fd)| libc::pollfd {
                fd: *fd,
                events: libc::POLLIN,
                revents: 0,
            })
            .collect();
        if !fds.is_empty() {
            // SAFETY: initialized bounded pollfd array; zero timeout does not block.
            unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, 0) };
        }
        for ((handler, fd), poll) in events.iter().zip(fds.iter()) {
            let registered = self
                .events
                .borrow()
                .iter()
                .any(|(h, d)| h.as_ptr() == handler.as_ptr() && d == fd);
            if registered && poll.revents & libc::POLLIN != 0 {
                // SAFETY: main-thread callback, retained live registered handler and descriptor.
                unsafe { handler.onFDIsSet(*fd) };
            }
        }
        let now = Instant::now();
        let due: Vec<_> = {
            let mut timers = self.timers.borrow_mut();
            timers
                .iter_mut()
                .filter_map(|timer| {
                    if now < timer.next {
                        return None;
                    }
                    timer.next = now + timer.period;
                    Some(timer.handler.clone())
                })
                .collect()
        };
        for handler in due {
            let registered = self
                .timers
                .borrow()
                .iter()
                .any(|t| t.handler.as_ptr() == handler.as_ptr());
            if registered {
                // SAFETY: main-thread callback, retained timer is still registered.
                unsafe { handler.onTimer() };
            }
        }
    }
    pub fn clear(&self) {
        let events = std::mem::take(&mut *self.events.borrow_mut());
        let timers = std::mem::take(&mut *self.timers.borrow_mut());
        drop((events, timers));
    }
}

macro_rules! implement {
    ($ty:ty, $field:ident) => {
        impl vst3::Steinberg::Linux::IRunLoopTrait for $ty {
            unsafe fn registerEventHandler(
                &self,
                handler: *mut vst3::Steinberg::Linux::IEventHandler,
                fd: i32,
            ) -> vst3::Steinberg::tresult {
                // SAFETY: forward the SDK's live interface contract to the retaining registry.
                unsafe { self.$field.register_event(handler, fd) }
            }
            unsafe fn unregisterEventHandler(
                &self,
                handler: *mut vst3::Steinberg::Linux::IEventHandler,
            ) -> vst3::Steinberg::tresult {
                self.$field.unregister_event(handler)
            }
            unsafe fn registerTimer(
                &self,
                handler: *mut vst3::Steinberg::Linux::ITimerHandler,
                milliseconds: u64,
            ) -> vst3::Steinberg::tresult {
                // SAFETY: forward the SDK's live interface contract to the retaining registry.
                unsafe { self.$field.register_timer(handler, milliseconds) }
            }
            unsafe fn unregisterTimer(
                &self,
                handler: *mut vst3::Steinberg::Linux::ITimerHandler,
            ) -> vst3::Steinberg::tresult {
                self.$field.unregister_timer(handler)
            }
        }
    };
}
pub(crate) use implement;

#[cfg(test)]
mod tests {
    use super::*;
    use std::{cell::Cell, rc::Rc};
    use vst3::{Class, ComWrapper};
    struct TimerFixture {
        count: Cell<u32>,
        registry: Rc<RunLoop>,
        pointer: Cell<*mut ITimerHandler>,
    }
    impl Class for TimerFixture {
        type Interfaces = (ITimerHandler,);
    }
    impl ITimerHandlerTrait for TimerFixture {
        unsafe fn onTimer(&self) {
            self.count.set(self.count.get() + 1);
            assert_eq!(
                self.registry.unregister_timer(self.pointer.get()),
                kResultOk
            );
        }
    }
    #[test]
    fn timer_can_unregister_itself_without_borrowing_across_foreign_call() {
        let registry = Rc::new(RunLoop::default());
        let timer = ComWrapper::new(TimerFixture {
            count: Cell::new(0),
            registry: registry.clone(),
            pointer: Cell::new(std::ptr::null_mut()),
        });
        let pointer = timer.to_com_ptr::<ITimerHandler>().unwrap();
        timer.pointer.set(pointer.as_ptr());
        assert_eq!(
            // SAFETY: test retains a live ITimerHandler through registration and callbacks.
            unsafe { registry.register_timer(pointer.as_ptr(), 1) },
            kResultOk
        );
        std::thread::sleep(Duration::from_millis(3));
        registry.poll();
        registry.poll();
        assert_eq!(timer.count.get(), 1);
    }
}
