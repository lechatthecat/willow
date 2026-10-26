# RwLock class updates

Decision: retain the existing Send + Sync requirement for RwLock<T> and
BlockingRwCell<T>. A class field write after construction makes that class
non-Sync, including a write through an exclusive lock binding. No lock-aware
exception or new escape analysis is introduced.

A read section can copy a class reference into an outer binding. Releasing
that section does not invalidate the reference. Exclusivity of a later write
section therefore does not by itself prove exclusive access to the referenced
object. Accepting field writes only because they occur in a write section
would require a separate proof about escaping aliases and concurrent readers.

For a class whose fields remain Sync and are not reassigned after construction,
replace the protected value through the mutable write binding:

```willow
lock write knobs as mut w { w = new Knobs(w.seed + 1); }
```

This publishes a new object; an earlier reference still denotes the old object.
Whole-value replacement does not erase field writes elsewhere in the program
and does not make non-Sync field types Sync. If in-place field mutation is
needed, use Mutex<T> with a Send payload and access it through the lock. The
existing RwLock diagnostic now explicitly describes both alternatives.

The runnable [example](../../../example/rwlock_class_update.wi) demonstrates the
old snapshot, the new value, and a Mutex-protected mutable counter. These use
distinct class/field names: the existing field-write analysis conservatively
marks same-named fields when a receiver cannot be resolved (including lock
bindings). This decision retains that documented approximation as well.

The change is diagnostic guidance, not a new soundness claim for every existing
lock/alias operation. It does not change lock lowering, ownership, escape rules,
runtime scheduling, or the compiler's runtime auto-build behavior.
