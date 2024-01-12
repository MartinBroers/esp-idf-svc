//! High resolution hardware timer based task scheduling
//!
//! Although FreeRTOS provides software timers, these timers have a few
//! limitations:
//!
//! - Maximum resolution is equal to RTOS tick period
//! - Timer callbacks are dispatched from a low-priority task
//!
//! EspTimer is a set of APIs that provides one-shot and periodic timers,
//! microsecond time resolution, and 52-bit range.

use core::result::Result;
use core::time::Duration;
use core::{ffi, ptr};

extern crate alloc;
use alloc::boxed::Box;

use embedded_svc::sys_time::SystemTime;
use embedded_svc::timer::{self, ErrorType, OnceTimer, PeriodicTimer, Timer, TimerService};

use crate::sys::*;

use ::log::info;

#[allow(unused_imports)]
pub use asyncify::*;

#[cfg(esp_idf_esp_timer_supports_isr_dispatch_method)]
pub use isr::*;

use crate::handle::RawHandle;

struct UnsafeCallback<'a>(*mut Box<dyn FnMut() + Send + 'a>);

impl<'a> UnsafeCallback<'a> {
    fn from(boxed: &mut Box<dyn FnMut() + Send + 'a>) -> Self {
        Self(boxed)
    }

    unsafe fn from_ptr(ptr: *mut ffi::c_void) -> Self {
        Self(ptr as *mut _)
    }

    fn as_ptr(&self) -> *mut ffi::c_void {
        self.0 as *mut _
    }

    unsafe fn call(&self) {
        let reference = self.0.as_mut().unwrap();

        (reference)();
    }
}

pub struct EspTimer<'a> {
    handle: esp_timer_handle_t,
    _callback: Box<dyn FnMut() + Send + 'a>,
}

impl<'a> EspTimer<'a> {
    pub fn is_scheduled(&self) -> Result<bool, EspError> {
        Ok(unsafe { esp_timer_is_active(self.handle) })
    }

    pub fn cancel(&self) -> Result<bool, EspError> {
        let res = unsafe { esp_timer_stop(self.handle) };

        Ok(res != ESP_OK)
    }

    pub fn after(&self, duration: Duration) -> Result<(), EspError> {
        self.cancel()?;

        esp!(unsafe { esp_timer_start_once(self.handle, duration.as_micros() as _) })?;

        Ok(())
    }

    pub fn every(&self, duration: Duration) -> Result<(), EspError> {
        self.cancel()?;

        esp!(unsafe { esp_timer_start_periodic(self.handle, duration.as_micros() as _) })?;

        Ok(())
    }

    extern "C" fn handle(arg: *mut ffi::c_void) {
        if crate::hal::interrupt::active() {
            #[cfg(esp_idf_esp_timer_supports_isr_dispatch_method)]
            {
                let signaled = crate::hal::interrupt::with_isr_yield_signal(move || unsafe {
                    UnsafeCallback::from_ptr(arg).call();
                });

                if signaled {
                    unsafe {
                        crate::sys::esp_timer_isr_dispatch_need_yield();
                    }
                }
            }

            #[cfg(not(esp_idf_esp_timer_supports_isr_dispatch_method))]
            {
                unreachable!();
            }
        } else {
            unsafe {
                UnsafeCallback::from_ptr(arg).call();
            }
        }
    }
}

unsafe impl<'a> Send for EspTimer<'a> {}

impl<'a> Drop for EspTimer<'a> {
    fn drop(&mut self) {
        self.cancel().unwrap();

        while unsafe { esp_timer_delete(self.handle) } != ESP_OK {
            // Timer is still running, busy-loop
        }

        info!("Timer dropped");
    }
}

impl<'a> RawHandle for EspTimer<'a> {
    type Handle = esp_timer_handle_t;

    fn handle(&self) -> Self::Handle {
        self.handle
    }
}

impl<'a> ErrorType for EspTimer<'a> {
    type Error = EspError;
}

impl<'a> timer::Timer for EspTimer<'a> {
    fn is_scheduled(&self) -> Result<bool, Self::Error> {
        EspTimer::is_scheduled(self)
    }

    fn cancel(&mut self) -> Result<bool, Self::Error> {
        EspTimer::cancel(self)
    }
}

impl<'a> OnceTimer for EspTimer<'a> {
    fn after(&mut self, duration: Duration) -> Result<(), Self::Error> {
        EspTimer::after(self, duration)
    }
}

impl<'a> PeriodicTimer for EspTimer<'a> {
    fn every(&mut self, duration: Duration) -> Result<(), Self::Error> {
        EspTimer::every(self, duration)
    }
}

pub trait EspTimerServiceType {
    fn is_isr() -> bool;
}

#[derive(Clone, Debug)]
pub struct Task;

impl EspTimerServiceType for Task {
    fn is_isr() -> bool {
        false
    }
}

pub struct EspTimerService<T>(T)
where
    T: EspTimerServiceType;

impl<T> EspTimerService<T>
where
    T: EspTimerServiceType,
{
    pub fn now(&self) -> Duration {
        Duration::from_micros(unsafe { esp_timer_get_time() as _ })
    }

    pub fn timer<F>(&self, callback: F) -> Result<EspTimer<'static>, EspError>
    where
        F: FnMut() + Send + 'static,
    {
        self.internal_timer(callback)
    }

    /// # Safety
    ///
    /// This method - in contrast to method `timer` - allows the user to pass
    /// a non-static callback/closure. This enables users to borrow
    /// - in the closure - variables that live on the stack - or more generally - in the same
    /// scope where the service is created.
    ///
    /// HOWEVER: care should be taken NOT to call `core::mem::forget()` on the service,
    /// as that would immediately lead to an UB (crash).
    /// Also note that forgetting the service might happen with `Rc` and `Arc`
    /// when circular references are introduced: https://github.com/rust-lang/rust/issues/24456
    ///
    /// The reason is that the closure is actually sent to a hidden ESP IDF thread.
    /// This means that if the service is forgotten, Rust is free to e.g. unwind the stack
    /// and the closure now owned by this other thread will end up with references to variables that no longer exist.
    ///
    /// The destructor of the service takes care - prior to the service being dropped and e.g.
    /// the stack being unwind - to remove the closure from the hidden thread and destroy it.
    /// Unfortunately, when the service is forgotten, the un-subscription does not happen
    /// and invalid references are left dangling.
    ///
    /// This "local borrowing" will only be possible to express in a safe way once/if `!Leak` types
    /// are introduced to Rust (i.e. the impossibility to "forget" a type and thus not call its destructor).
    pub unsafe fn timer_nonstatic<'a, F>(&self, callback: F) -> Result<EspTimer<'a>, EspError>
    where
        F: FnMut() + Send + 'a,
    {
        self.internal_timer(callback)
    }

    fn internal_timer<'a, F>(&self, callback: F) -> Result<EspTimer<'a>, EspError>
    where
        F: FnMut() + Send + 'a,
    {
        let mut handle: esp_timer_handle_t = ptr::null_mut();

        let boxed_callback: Box<dyn FnMut() + Send + 'a> = Box::new(callback);

        let mut callback = Box::new(boxed_callback);
        let unsafe_callback = UnsafeCallback::from(&mut callback);

        #[cfg(esp_idf_esp_timer_supports_isr_dispatch_method)]
        let dispatch_method = if T::is_isr() {
            esp_timer_dispatch_t_ESP_TIMER_ISR
        } else {
            esp_timer_dispatch_t_ESP_TIMER_TASK
        };

        #[cfg(not(esp_idf_esp_timer_supports_isr_dispatch_method))]
        let dispatch_method = esp_timer_dispatch_t_ESP_TIMER_TASK;

        esp!(unsafe {
            esp_timer_create(
                &esp_timer_create_args_t {
                    callback: Some(EspTimer::handle),
                    name: b"rust\0" as *const _ as *const _, // TODO
                    arg: unsafe_callback.as_ptr(),
                    dispatch_method,
                    skip_unhandled_events: false, // TODO
                },
                &mut handle as *mut _,
            )
        })?;

        Ok(EspTimer {
            handle,
            _callback: callback,
        })
    }
}

pub type EspTaskTimerService = EspTimerService<Task>;

impl EspTimerService<Task> {
    pub fn new() -> Result<Self, EspError> {
        Ok(Self(Task))
    }
}

impl<T> Clone for EspTimerService<T>
where
    T: EspTimerServiceType + Clone,
{
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}

impl<T> ErrorType for EspTimerService<T>
where
    T: EspTimerServiceType,
{
    type Error = EspError;
}

impl<T> TimerService for EspTimerService<T>
where
    T: EspTimerServiceType,
{
    type Timer<'a> = EspTimer<'static> where Self: 'a;

    fn timer<F>(&self, callback: F) -> Result<Self::Timer<'_>, Self::Error>
    where
        F: FnMut() + Send + 'static,
    {
        EspTimerService::timer(self, callback)
    }
}

impl<T> SystemTime for EspTimerService<T>
where
    T: EspTimerServiceType,
{
    fn now(&self) -> Duration {
        EspTimerService::now(self)
    }
}

#[cfg(esp_idf_esp_timer_supports_isr_dispatch_method)]
mod isr {
    use crate::sys::EspError;

    #[derive(Clone, Debug)]
    pub struct ISR;

    impl super::EspTimerServiceType for ISR {
        fn is_isr() -> bool {
            true
        }
    }

    pub type EspISRTimerService = super::EspTimerService<ISR>;

    impl EspISRTimerService {
        /// # Safety
        /// TODO
        pub unsafe fn new() -> Result<Self, EspError> {
            Ok(Self(ISR))
        }
    }
}

mod asyncify {
    use embedded_svc::utils::asyncify::timer::AsyncTimerService;
    use embedded_svc::utils::asyncify::Asyncify;

    impl Asyncify for super::EspTimerService<super::Task> {
        type AsyncWrapper<S> = AsyncTimerService<S>;
    }

    #[cfg(esp_idf_esp_timer_supports_isr_dispatch_method)]
    impl Asyncify for super::EspTimerService<super::ISR> {
        type AsyncWrapper<S> = AsyncTimerService<S>;
    }
}

pub mod embassy_time {
    #[cfg(any(feature = "embassy-time-driver", feature = "embassy-time-queue-driver"))]
    pub mod driver {
        use core::cell::UnsafeCell;

        use heapless::Vec;

        use ::embassy_time_driver::{AlarmHandle, Driver};

        use crate::hal::task::CriticalSection;

        use crate::sys::*;

        use crate::timer::*;

        struct Alarm {
            timer: Option<EspTimer<'static>>,
            #[allow(clippy::type_complexity)]
            callback: Option<(fn(*mut ()), *mut ())>,
        }

        struct EspDriver<const MAX_ALARMS: usize = 16> {
            alarms: UnsafeCell<Vec<Alarm, MAX_ALARMS>>,
            cs: CriticalSection,
        }

        impl<const MAX_ALARMS: usize> EspDriver<MAX_ALARMS> {
            const fn new() -> Self {
                Self {
                    alarms: UnsafeCell::new(Vec::new()),
                    cs: CriticalSection::new(),
                }
            }

            fn call(&self, id: u8) {
                let callback = {
                    let _guard = self.cs.enter();

                    let alarm = self.alarm(id);

                    alarm.callback
                };

                if let Some((func, arg)) = callback {
                    func(arg)
                }
            }

            #[allow(clippy::mut_from_ref)]
            fn alarm(&self, id: u8) -> &mut Alarm {
                &mut unsafe { self.alarms.get().as_mut() }.unwrap()[id as usize]
            }
        }

        unsafe impl<const MAX_ALARMS: usize> Send for EspDriver<MAX_ALARMS> {}
        unsafe impl<const MAX_ALARMS: usize> Sync for EspDriver<MAX_ALARMS> {}

        impl<const MAX_ALARMS: usize> Driver for EspDriver<MAX_ALARMS> {
            fn now(&self) -> u64 {
                unsafe { esp_timer_get_time() as _ }
            }

            unsafe fn allocate_alarm(&self) -> Option<AlarmHandle> {
                let id = {
                    let _guard = self.cs.enter();

                    let id = self.alarms.get().as_mut().unwrap().len();

                    if id < MAX_ALARMS {
                        self.alarms
                            .get()
                            .as_mut()
                            .unwrap()
                            .push(Alarm {
                                timer: None,
                                callback: None,
                            })
                            .unwrap_or_else(|_| unreachable!());

                        id as u8
                    } else {
                        return None;
                    }
                };

                let service = EspTimerService::<Task>::new().unwrap();

                // Driver is always statically allocated, so this is safe
                let static_self: &'static Self = core::mem::transmute(self);

                self.alarm(id).timer = Some(service.timer(move || static_self.call(id)).unwrap());

                Some(AlarmHandle::new(id))
            }

            fn set_alarm_callback(&self, handle: AlarmHandle, callback: fn(*mut ()), ctx: *mut ()) {
                let _guard = self.cs.enter();

                let alarm = self.alarm(handle.id());

                alarm.callback = Some((callback, ctx));
            }

            fn set_alarm(&self, handle: AlarmHandle, timestamp: u64) -> bool {
                let alarm = self.alarm(handle.id());

                let now = self.now();

                if now < timestamp {
                    alarm
                        .timer
                        .as_mut()
                        .unwrap()
                        .after(Duration::from_micros(timestamp - now))
                        .unwrap();
                    true
                } else {
                    false
                }
            }
        }

        pub type LinkWorkaround = [*mut (); 4];

        static mut __INTERNAL_REFERENCE: LinkWorkaround = [
            _embassy_time_now as *mut _,
            _embassy_time_allocate_alarm as *mut _,
            _embassy_time_set_alarm_callback as *mut _,
            _embassy_time_set_alarm as *mut _,
        ];

        pub fn link() -> LinkWorkaround {
            unsafe { __INTERNAL_REFERENCE }
        }

        ::embassy_time_driver::time_driver_impl!(static DRIVER: EspDriver = EspDriver::new());
    }

    #[cfg(feature = "embassy-time-queue-driver")]
    pub mod queue {
        #[cfg(esp_idf_esp_timer_supports_isr_dispatch_method)]
        use crate::hal::interrupt::embassy_sync::IsrRawMutex as RawMutexImpl;

        #[cfg(not(esp_idf_esp_timer_supports_isr_dispatch_method))]
        use crate::hal::task::embassy_sync::EspRawMutex as RawMutexImpl;

        use crate::sys::*;

        use generic_queue::*;

        struct AlarmImpl(esp_timer_handle_t);

        impl AlarmImpl {
            unsafe extern "C" fn handle_isr(alarm_context: *mut core::ffi::c_void) {
                let alarm_context = (alarm_context as *const AlarmContext).as_ref().unwrap();

                if crate::hal::interrupt::active() {
                    #[cfg(esp_idf_esp_timer_supports_isr_dispatch_method)]
                    {
                        let signaled = crate::hal::interrupt::with_isr_yield_signal(move || {
                            (alarm_context.callback)(alarm_context.ctx);
                        });

                        if signaled {
                            crate::sys::esp_timer_isr_dispatch_need_yield();
                        }
                    }

                    #[cfg(not(esp_idf_esp_timer_supports_isr_dispatch_method))]
                    unreachable!();
                } else {
                    (alarm_context.callback)(alarm_context.ctx);
                }
            }
        }

        unsafe impl Send for AlarmImpl {}

        impl Alarm for AlarmImpl {
            fn new(context: &AlarmContext) -> Self {
                #[cfg(esp_idf_esp_timer_supports_isr_dispatch_method)]
                let dispatch_method = esp_timer_dispatch_t_ESP_TIMER_ISR;

                #[cfg(not(esp_idf_esp_timer_supports_isr_dispatch_method))]
                let dispatch_method = esp_timer_dispatch_t_ESP_TIMER_TASK;

                let mut handle: esp_timer_handle_t = core::ptr::null_mut();

                esp!(unsafe {
                    esp_timer_create(
                        &esp_timer_create_args_t {
                            callback: Some(AlarmImpl::handle_isr),
                            name: b"embassy-time-queue\0" as *const _ as *const _,
                            arg: context as *const _ as *mut _,
                            dispatch_method,
                            skip_unhandled_events: false,
                        },
                        &mut handle as *mut _,
                    )
                })
                .unwrap();

                Self(handle)
            }

            fn schedule(&mut self, timestamp: u64) {
                let now = unsafe { esp_timer_get_time() as _ };
                let after = if timestamp <= now { 0 } else { timestamp - now };

                unsafe {
                    esp_timer_stop(self.0);
                }

                if timestamp < u64::MAX {
                    esp!(unsafe { esp_timer_start_once(self.0, after as _) }).unwrap();
                }
            }
        }

        pub type LinkWorkaround = [*mut (); 1];

        static mut __INTERNAL_REFERENCE: LinkWorkaround = [_embassy_time_schedule_wake as *mut _];

        pub fn link() -> (LinkWorkaround, super::driver::LinkWorkaround) {
            (unsafe { __INTERNAL_REFERENCE }, super::driver::link())
        }

        ::embassy_time_queue_driver::timer_queue_impl!(static QUEUE: Queue<RawMutexImpl, AlarmImpl> = Queue::new());

        mod generic_queue {
            use core::cell::RefCell;
            use core::cmp::Ordering;
            use core::task::Waker;

            use embassy_sync::blocking_mutex::{raw::RawMutex, Mutex};

            use embassy_time::Instant;
            use embassy_time_queue_driver::TimerQueue;

            use heapless::sorted_linked_list::{LinkedIndexU8, Min, SortedLinkedList};

            #[derive(Debug)]
            struct Timer {
                at: Instant,
                waker: Waker,
            }

            impl PartialEq for Timer {
                fn eq(&self, other: &Self) -> bool {
                    self.at == other.at
                }
            }

            impl Eq for Timer {}

            impl PartialOrd for Timer {
                fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
                    Some(self.cmp(other))
                }
            }

            impl Ord for Timer {
                fn cmp(&self, other: &Self) -> Ordering {
                    self.at.cmp(&other.at)
                }
            }

            pub struct AlarmContext {
                pub callback: fn(*mut ()),
                pub ctx: *mut (),
            }

            impl AlarmContext {
                const fn new() -> Self {
                    Self {
                        callback: Self::noop,
                        ctx: core::ptr::null_mut(),
                    }
                }

                fn set(&mut self, callback: fn(*mut ()), ctx: *mut ()) {
                    self.callback = callback;
                    self.ctx = ctx;
                }

                fn noop(_ctx: *mut ()) {}
            }

            unsafe impl Send for AlarmContext {}

            pub trait Alarm {
                fn new(context: &AlarmContext) -> Self;
                fn schedule(&mut self, timestamp: u64);
            }

            struct InnerQueue<A> {
                queue: SortedLinkedList<Timer, LinkedIndexU8, Min, 128>,
                alarm: Option<A>,
                alarm_context: AlarmContext,
                alarm_at: Instant,
            }

            impl<A: Alarm> InnerQueue<A> {
                const fn new() -> Self {
                    Self {
                        queue: SortedLinkedList::new_u8(),
                        alarm: None,
                        alarm_context: AlarmContext::new(),
                        alarm_at: Instant::MAX,
                    }
                }

                fn schedule_wake(&mut self, at: Instant, waker: &Waker) {
                    self.initialize();

                    self.queue
                        .find_mut(|timer| timer.waker.will_wake(waker))
                        .map(|mut timer| {
                            timer.at = at;
                            timer.finish();
                        })
                        .unwrap_or_else(|| {
                            let mut timer = Timer {
                                waker: waker.clone(),
                                at,
                            };

                            loop {
                                match self.queue.push(timer) {
                                    Ok(()) => break,
                                    Err(e) => timer = e,
                                }

                                self.queue.pop().unwrap().waker.wake();
                            }
                        });

                    // Don't wait for the alarm callback to trigger and directly
                    // dispatch all timers that are already due
                    //
                    // Then update the alarm if necessary
                    self.dispatch();
                }

                fn dispatch(&mut self) {
                    let now = Instant::now();

                    while self.queue.peek().filter(|timer| timer.at <= now).is_some() {
                        self.queue.pop().unwrap().waker.wake();
                    }

                    self.update_alarm();
                }

                fn update_alarm(&mut self) {
                    if let Some(timer) = self.queue.peek() {
                        let new_at = timer.at;

                        if self.alarm_at != new_at {
                            self.alarm_at = new_at;
                            self.alarm.as_mut().unwrap().schedule(new_at.as_ticks());
                        }
                    } else {
                        self.alarm_at = Instant::MAX;
                        self.alarm
                            .as_mut()
                            .unwrap()
                            .schedule(Instant::MAX.as_ticks());
                    }
                }

                fn handle_alarm(&mut self) {
                    self.alarm_at = Instant::MAX;

                    self.dispatch();
                }

                fn initialize(&mut self) {
                    if self.alarm.is_none() {
                        self.alarm = Some(A::new(&self.alarm_context));
                    }
                }
            }

            pub struct Queue<R: RawMutex, A: Alarm> {
                inner: Mutex<R, RefCell<InnerQueue<A>>>,
            }

            impl<R: RawMutex, A: Alarm> Queue<R, A> {
                pub const fn new() -> Self {
                    Self {
                        inner: Mutex::new(RefCell::new(InnerQueue::new())),
                    }
                }

                fn schedule_wake(&'static self, at: Instant, waker: &Waker) {
                    self.inner.lock(|inner| {
                        let mut inner = inner.borrow_mut();
                        inner
                            .alarm_context
                            .set(Self::handle_alarm_callback, self as *const _ as _);
                        inner.schedule_wake(at, waker);
                    });
                }

                fn handle_alarm(&self) {
                    self.inner.lock(|inner| inner.borrow_mut().handle_alarm());
                }

                fn handle_alarm_callback(ctx: *mut ()) {
                    unsafe { (ctx as *const Self).as_ref().unwrap() }.handle_alarm();
                }
            }

            impl<R: RawMutex, A: Alarm> TimerQueue for Queue<R, A> {
                fn schedule_wake(&'static self, at: u64, waker: &Waker) {
                    Queue::schedule_wake(self, Instant::from_ticks(at), waker);
                }
            }
        }
    }
}
