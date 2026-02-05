# A Deeply Unsafe Garbage Collection Pool using Tri-Color Mark & Sweep in Rust

Yes, valgrind says its OK. Yes, the tests pass. No, you will not enjoy fixing the inevitable bugs that will appear if you decide to use it. Here be dragons.

No dependencies and approximately 500 lines of code.

# Examples

## Allocating Objects
Anything that is either a root pointer or indirectly referenced by a root pointer will stay. Everything else will be garbage-collected.

```rust ignore
use gcpool::{GcPool, Gc};

let pool = GcPool::new();

let my_object: Gc<i32> = pool.alloc(123);

unsafe { pool.gc_and_sweep(); }

println!("{}", my_object); // Oh No! Use-After-Free! The number was deallocated.
```

```rust
use gcpool::{GcPool, Gc};

let pool = GcPool::new();

// Set as root so it doesn't get deallocated.
let my_object: Gc<i32> = pool.alloc(123).root(&pool);

unsafe { pool.gc_and_sweep(); }

println!("{}", my_object); // All good!
```

## Incremental Garbage-Collection
```rust
use gcpool::GcPool;

let pool = GcPool::new();

// Do some stuff ...

for _ in 0..100 {
    // Advance the algorithm.
    pool.gc();
}

// Do some stuff ...

for _ in 0..100 {
    // Advance the algorithm.
    pool.gc();
}

// Now sweep!
unsafe { pool.sweep(); }
```

Run `pool.gc()` in the background on another thread.
```rust
use gcpool::GcPool;

let pool = GcPool::new_with_thread();

// Do some stuff

// Now sweep!
unsafe { pool.sweep(); }

pool.join();
```

## Cyclic References
```rust ignore
use gcpool::GcPool;

let pool = GcPool::new();

// Allocate the cyclic data-structure
let mut x = pool.alloc(None);
let y = pool.alloc(Some(x.clone()));
*x = Some(y.clone());

// There are two allocated objects in the pool
assert_eq!(pool.object_count(), 2);

unsafe { pool.gc_and_sweep(); }

// They all got deallocated!
assert_eq!(pool.object_count(), 0);
```

## Allocating Custom Structs
The garbage collector needs to know what references your struct depends on. You can provide this information with the `Trace` trait.
```rust
use std::ptr::NonNull;
use gcpool::{GcPool, GcInner, Gc, Trace};

#[derive(Debug)]
struct MyStruct {
    a: Gc<i32>,
    b: Gc<i32>,
    c: Gc<i32>,
}

unsafe impl Trace for MyStruct {
    fn append_children(&self, children: &mut Vec<NonNull<GcInner<dyn Trace>>>) {
        self.a.append_children(children); // Please don't delete a we need it!
        self.b.append_children(children); // Please don't delete b we need it!
        self.c.append_children(children); // Please don't delete c we need it!
    }
}

let pool = GcPool::new();

let my_struct = pool.alloc(MyStruct {
    a: pool.alloc(1),
    b: pool.alloc(2),
    c: pool.alloc(3),
}).root(&pool);

unsafe { pool.gc_and_sweep(); }

println!("{my_struct:?}"); // All good!
```