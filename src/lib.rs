#![doc = include_str!("../README.md")]

use std::{
    alloc::{Layout, dealloc},
    cell::Cell,
    collections::HashMap,
    fmt,
    marker::PhantomData,
    mem::{self, ManuallyDrop},
    ops::{Deref, DerefMut},
    ptr::NonNull,
    sync::{Arc, Mutex, RwLock},
    thread::{self, JoinHandle},
};

#[derive(Default)]
pub struct GcPool {
    all: RwLock<Vec<NonNull<GcInner<()>>>>,
    grey: RwLock<Vec<NonNull<GcInner<()>>>>,
}

impl GcPool {
    pub fn new() -> Self {
        Self::default()
    }

    /// Create a garbage-collection pool and spawn a thread that continously calls [`Self::gc`].
    /// You will still need to call [`Self::sweep`] every now and then.
    pub fn new_with_thread() -> ThreadedGcPool {
        /// Ignore safety restrictions and pass data to a thread anyway.
        struct Pass<T>(Arc<T>);
        unsafe impl<T> Send for Pass<T> {}
        unsafe impl<T> Sync for Pass<T> {}

        #[expect(clippy::arc_with_non_send_sync)]
        let pool = Arc::new(Self::new());
        let pass = Pass(pool.clone());
        let is_alive = Arc::new(Mutex::new(true));
        let pass_is_alive = is_alive.clone();

        let thread = thread::spawn(move || {
            let pass = pass;
            let pool = pass.0;
            loop {
                pool.gc();
                if !*pass_is_alive.lock().unwrap() {
                    break;
                }
            }
        });

        ThreadedGcPool {
            pool,
            is_alive,
            is_freed: false,
            thread: ManuallyDrop::new(thread),
        }
    }

    pub fn object_count(&self) -> usize {
        self.all.read().unwrap().len()
    }

    /// Keep garbage-collecting until objects are deallocated.
    ///
    /// # Safety
    /// See [`Self::sweep`]
    pub unsafe fn gc_and_sweep(&self) {
        while !self.grey.read().unwrap().is_empty() {
            self.gc();
        }
        unsafe { self.sweep() };
    }

    /// Attempt a garbage-collection sweep.
    /// If the conditions are right this will deallocate objects.
    /// If the conditions are not right this is a NOOP.
    ///
    /// # Safety
    /// Will create dangling pointers if `Trace` is implemented incorrectly or pointers that aren't referenced by root objects are still held on-to.
    pub unsafe fn sweep(&self) {
        if self.grey.read().unwrap().is_empty() {
            let mut all = self.all.write().unwrap();
            let mut grey = self.grey.write().unwrap();
            let src = Vec::with_capacity(all.len());

            for ptr in mem::replace(all.as_mut(), src) {
                let value = unsafe { ptr.as_ref() };
                let color = unsafe { *value.color.as_ptr() };

                match color {
                    Color::White => unsafe { drop_ptr(ptr) },
                    Color::Root => {
                        grey.push(ptr);
                        all.push(ptr);
                    }
                    Color::Black => {
                        value.color.set(Color::White);
                        all.push(ptr);
                    }
                }
            }
        }
    }

    /// Advance the garbage-collection algorithm.
    /// This should be called occassionaly and paired with `Self::sweep`.
    pub fn gc(&self) {
        if !self.grey.read().unwrap().is_empty() {
            let mut grey = self.grey.write().unwrap();
            let src = Vec::with_capacity(grey.len());

            for ptr in mem::replace(grey.as_mut(), src) {
                let value = unsafe { ptr.as_ref() };
                unsafe { value.vtable.grey_children(&value.value, &mut grey) };
            }
        }
    }

    pub fn alloc<T: Trace + 'static>(&self, value: T) -> Gc<T> {
        // Lock serves a double purpose. Because self.all is locked the garbage collector can't sweep,
        // which means that it won't delete things while we're setting up a new allocation.
        let mut all = self.all.write().unwrap();

        let ptr = Box::leak(Box::new(GcInner {
            color: Cell::new(Color::White),
            vtable: GcVtable::new::<T>(),
            value,
        }));

        let ptr = unsafe { NonNull::new_unchecked(ptr) };

        all.push(ptr.cast());

        Gc {
            ptr,
            phantom: PhantomData,
        }
    }
}

impl Drop for GcPool {
    fn drop(&mut self) {
        let all = self.all.write().unwrap();
        let grey = self.grey.write().unwrap();

        for ptr in all.iter() {
            unsafe { drop_ptr(*ptr) };
        }

        // Lock it forever so it's never used again.
        mem::forget(all);
        mem::forget(grey);
    }
}

unsafe fn drop_ptr(ptr: NonNull<GcInner<()>>) {
    unsafe {
        ptr.drop_in_place();
        dealloc(ptr.as_ptr() as *mut u8, Layout::new::<GcInner<()>>());
    };
}

pub struct ThreadedGcPool {
    pool: Arc<GcPool>,
    is_alive: Arc<Mutex<bool>>,
    is_freed: bool,
    thread: ManuallyDrop<JoinHandle<()>>,
}

impl Deref for ThreadedGcPool {
    type Target = GcPool;

    fn deref(&self) -> &Self::Target {
        self.pool.as_ref()
    }
}

impl ThreadedGcPool {
    pub fn join(mut self) {
        *self.is_alive.lock().unwrap() = false;
        unsafe { ManuallyDrop::take(&mut self.thread) }
            .join()
            .unwrap();
        self.is_freed = true;
        drop(self);
    }
}

impl Drop for ThreadedGcPool {
    fn drop(&mut self) {
        if !self.is_freed {
            *self.is_alive.lock().unwrap() = false;
            unsafe { ManuallyDrop::drop(&mut self.thread) };
        }
    }
}

pub struct Gc<T: ?Sized> {
    ptr: NonNull<GcInner<T>>,
    phantom: PhantomData<T>,
}

impl<T> Gc<T> {
    pub fn root(self, pool: &GcPool) -> Gc<T> {
        let grey = &mut *pool.grey.write().unwrap();
        let value = unsafe { self.ptr.as_ref() };
        value.color.set(Color::Root);
        grey.push(self.ptr.cast());
        self
    }

    pub fn unroot(self) {
        let value = unsafe { self.ptr.as_ref() };
        value.color.set(Color::Black);
    }
}

impl<T> Copy for Gc<T> {}

impl<T: fmt::Debug> fmt::Debug for Gc<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        (**self).fmt(f)
    }
}

impl<T: fmt::Display> fmt::Display for Gc<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        (**self).fmt(f)
    }
}

impl<T> Clone for Gc<T> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<T> Deref for Gc<T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        &unsafe { self.ptr.as_ref() }.value
    }
}

impl<T> DerefMut for Gc<T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut unsafe { self.ptr.as_mut() }.value
    }
}

impl<T> AsRef<T> for Gc<T> {
    fn as_ref(&self) -> &T {
        self
    }
}

impl<T> AsMut<T> for Gc<T> {
    fn as_mut(&mut self) -> &mut T {
        self
    }
}

// NOTE: #[repr(C)] is required so that `sizeof GcInner<()> == sizeof GcInner<T>`
//       the gc needs to be able to erase the type so you can allocate any type of object in it.
#[repr(C)]
pub struct GcInner<T: ?Sized> {
    color: Cell<Color>,
    vtable: GcVtable,
    value: T,
}

impl<T: ?Sized> Drop for GcInner<T> {
    fn drop(&mut self) {
        let ptr = &mut self.value as *mut T as *mut ();
        let ptr = unsafe { ptr.as_mut().unwrap() };
        (self.vtable.drop_ptr)(ptr);
    }
}

struct GcVtable {
    grey_children_ptr: fn(&(), &mut Vec<NonNull<GcInner<()>>>),
    drop_ptr: fn(&mut ()),
}

impl GcVtable {
    pub fn new<T: Trace>() -> GcVtable {
        type GreyFrom<T> = unsafe fn(&T, &mut Vec<NonNull<GcInner<()>>>);
        type GreyTo = fn(&(), &mut Vec<NonNull<GcInner<()>>>);
        let ptr = T::append_children as GreyFrom<T>;
        let grey_children_ptr = unsafe { mem::transmute::<GreyFrom<T>, GreyTo>(ptr) };

        type DropFrom<T> = unsafe fn(&mut ManuallyDrop<T>);
        type DropTo = fn(&mut ());
        let ptr = ManuallyDrop::<T>::drop as DropFrom<T>;
        let drop_ptr = unsafe { mem::transmute::<DropFrom<T>, DropTo>(ptr) };

        GcVtable {
            grey_children_ptr,
            drop_ptr,
        }
    }

    pub unsafe fn grey_children(&self, this: &(), grey: &mut Vec<NonNull<GcInner<()>>>) {
        (self.grey_children_ptr)(this, grey);
    }
}

#[derive(Clone, Copy, Debug)]
enum Color {
    White,
    Black,
    Root,
}

/// Perform a garbage-collection trace.
///
/// # Safety
/// If a pointer that is referenced by the data-structure does not get traced.
/// Then the pointer may be garbage-collected and left dangling.
pub unsafe trait Trace {
    /// Append pointers that this data-structure references to the list.
    fn append_children(&self, children: &mut Vec<NonNull<GcInner<()>>>);
}

unsafe impl<T: Trace> Trace for Gc<T> {
    fn append_children(&self, grey: &mut Vec<NonNull<GcInner<()>>>) {
        let value = unsafe { self.ptr.as_ref() };
        if matches!(value.color.get(), Color::White) {
            value.color.set(Color::Black);
            grey.push(self.ptr.cast());
            unsafe { self.ptr.as_ref() }.value.append_children(grey);
        }
    }
}

unsafe impl<T: Trace> Trace for Vec<T> {
    fn append_children(&self, children: &mut Vec<NonNull<GcInner<()>>>) {
        for value in self {
            value.append_children(children);
        }
    }
}

unsafe impl<T: Trace> Trace for [T] {
    fn append_children(&self, children: &mut Vec<NonNull<GcInner<()>>>) {
        for value in self {
            value.append_children(children);
        }
    }
}

unsafe impl<K: Trace, V: Trace, S> Trace for HashMap<K, V, S> {
    fn append_children(&self, children: &mut Vec<NonNull<GcInner<()>>>) {
        for (k, v) in self {
            k.append_children(children);
            v.append_children(children);
        }
    }
}

unsafe impl<T: Trace> Trace for Option<T> {
    fn append_children(&self, children: &mut Vec<NonNull<GcInner<()>>>) {
        if let Some(value) = self {
            value.append_children(children);
        }
    }
}

unsafe impl<T: Trace, E: Trace> Trace for Result<T, E> {
    fn append_children(&self, children: &mut Vec<NonNull<GcInner<()>>>) {
        match self {
            Ok(value) => value.append_children(children),
            Err(error) => error.append_children(children),
        }
    }
}

unsafe impl<T: Trace> Trace for &T {
    fn append_children(&self, children: &mut Vec<NonNull<GcInner<()>>>) {
        (**self).append_children(children);
    }
}

unsafe impl<T: Trace> Trace for &mut T {
    fn append_children(&self, children: &mut Vec<NonNull<GcInner<()>>>) {
        (**self).append_children(children);
    }
}

macro_rules! impl_trace_for_atom {
    ( $t:ty ) => {
        unsafe impl Trace for $t {
            fn append_children(&self, _: &mut Vec<NonNull<GcInner<()>>>) {}
        }
    };
}

impl_trace_for_atom!(u8);
impl_trace_for_atom!(u16);
impl_trace_for_atom!(u32);
impl_trace_for_atom!(u64);
impl_trace_for_atom!(u128);
impl_trace_for_atom!(i8);
impl_trace_for_atom!(i16);
impl_trace_for_atom!(i32);
impl_trace_for_atom!(i64);
impl_trace_for_atom!(i128);
impl_trace_for_atom!(f32);
impl_trace_for_atom!(f64);
impl_trace_for_atom!(bool);
impl_trace_for_atom!(String);
impl_trace_for_atom!(str);

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn cycles() {
        let p = GcPool::new();

        struct MyStruct(Option<Gc<MyStruct>>);
        unsafe impl Trace for MyStruct {
            fn append_children(&self, children: &mut Vec<NonNull<GcInner<()>>>) {
                self.0.append_children(children);
            }
        }

        let mut x = p.alloc(MyStruct(None));
        let mut y = p.alloc(MyStruct(None));
        x.0 = Some(y.clone());
        y.0 = Some(x.clone());

        assert_eq!(p.object_count(), 2);
        x.root(&p);
        unsafe {
            p.gc_and_sweep();
            p.gc_and_sweep();
        }
        assert_eq!(p.object_count(), 2);

        x.unroot();
        unsafe {
            p.gc_and_sweep();
            p.gc_and_sweep();
        }
        assert_eq!(p.object_count(), 0);
    }

    #[test]
    fn basic() {
        let p = GcPool::new();
        let list = p.alloc(vec![p.alloc(1), p.alloc(2), p.alloc(3)]).root(&p);
        assert_eq!(p.object_count(), 4);
        unsafe {
            p.gc_and_sweep();
            p.gc_and_sweep();
        }
        assert_eq!(p.object_count(), 4);

        list.unroot();
        unsafe {
            p.gc_and_sweep();
            p.gc_and_sweep();
        }
        assert_eq!(p.object_count(), 0);
    }
}
