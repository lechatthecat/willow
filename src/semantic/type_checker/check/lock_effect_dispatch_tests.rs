use super::*;

#[test]
fn local_interface_method_inventory_has_one_record_per_body() {
    for count in [16, 64, 256, 1024] {
        let source = (0..count)
            .map(|i| format!("class C{i} {{ pub fn run(self) {{}} }}"))
            .collect::<String>();
        let tokens = Lexer::new(&source).tokenize().unwrap();
        let (program, errors) = Parser::new(tokens).parse();
        assert!(errors.is_empty());
        let mut checker = TypeChecker::new();
        LOCAL_INTERFACE_METHOD_RECORDS.with(|records| records.set(0));
        // Exercise only inventory construction; declaration/type checking
        // and hierarchy dispatch have their own independently sized work.
        checker.connect_interface_lock_effects(&program);
        assert_eq!(
            LOCAL_INTERFACE_METHOD_RECORDS.with(|records| records.get()),
            count
        );
        println!("local interface methods={count} inventory_records={count}");
    }
}

/// The primary-label text of every E2604 in `source`, in report order.
fn lock_effect_labels(source: &str) -> Vec<String> {
    check_source(source)
        .into_iter()
        .filter(|diagnostic| diagnostic.code == ErrorCode::E2604)
        .map(|diagnostic| {
            diagnostic
                .labels
                .first()
                .map(|label| label.message.clone())
                .unwrap_or_default()
        })
        .collect()
}

fn assert_lock_effect_witness(source: &str, expected: &str) {
    let labels = lock_effect_labels(source);
    assert_eq!(
        labels.len(),
        1,
        "expected exactly one E2604 mentioning `{expected}`, got {labels:?}"
    );
    assert!(
        labels[0].contains(expected),
        "expected `{expected}` in {labels:?}"
    );
}

fn assert_no_lock_effect(source: &str) {
    let errors = check_source(source);
    assert!(
        errors.iter().all(|error| error.code != ErrorCode::E2604),
        "unexpected lock effect: {errors:?}"
    );
}

/// d01: the base line. The receiver's own class declares the waiting body,
/// which the old upward walk already found.
#[test]
fn d01_direct_receiver_class_body_is_reported() {
    assert_lock_effect_witness(
        r#"
class Worker {
    pub fn run(self, ch: Channel<i64>) {
        let value = ch.recv();
    }
}

async fn main() {
    let m = Mutex::new(0);
    let ch: Channel<i64> = Channel::new();
    let worker = new Worker();
    lock m as value {
        worker.run(ch);
    }
}
"#,
        "Worker::run",
    );
}

/// d02: the fix. The declared type is pure; the override that actually runs
/// waits. An upward-only resolution reports nothing here.
#[test]
fn d02_subclass_override_is_reported_through_a_base_receiver() {
    assert_lock_effect_witness(
        r#"
open class Base {
    pub open fn run(self, ch: Channel<i64>) {}
}

class Derived extends Base {
    pub override fn run(self, ch: Channel<i64>) {
        let value = ch.recv();
    }
}

async fn main() {
    let m = Mutex::new(0);
    let ch: Channel<i64> = Channel::new();
    let base: Base = new Derived();
    lock m as value {
        base.run(ch);
    }
}
"#,
        "Derived::run",
    );
}

/// d03: the union is not a blanket taint. Every reachable body being pure
/// keeps the call legal, so the widened resolution cannot be trivially
/// "report everything".
#[test]
fn d03_pure_hierarchy_stays_legal() {
    assert_no_lock_effect(
        r#"
open class Base {
    pub open fn run(self) -> i64 { return 1; }
}

class Derived extends Base {
    pub override fn run(self) -> i64 { return 2; }
}

async fn main() {
    let m = Mutex::new(0);
    let base: Base = new Derived();
    lock m as value {
        let got = base.run();
    }
}
"#,
    );
}

/// d04: `self.hook()` inside an inherited body is a virtual call too. This
/// is the exact shape of the virtual-dispatch effect analysis backend bug.
#[test]
fn d04_self_call_reaches_a_subclass_override() {
    assert_lock_effect_witness(
        r#"
open class Base {
    pub open fn hook(self, ch: Channel<i64>) {}

    pub fn run(self, ch: Channel<i64>) {
        self.hook(ch);
    }
}

class Child extends Base {
    pub override fn hook(self, ch: Channel<i64>) {
        let value = ch.recv();
    }
}

async fn main() {
    let m = Mutex::new(0);
    let ch: Channel<i64> = Channel::new();
    let base: Base = new Child();
    lock m as value {
        base.run(ch);
    }
}
"#,
        "Base::run",
    );
}

/// d05: the union walks the whole subtree, not one level.
#[test]
fn d05_deep_hierarchy_leaf_override_is_reported() {
    assert_lock_effect_witness(
        r#"
open class A {
    pub open fn run(self, ch: Channel<i64>) {}
}

open class B extends A {
    pub override fn run(self, ch: Channel<i64>) {}
}

class C extends B {
    pub override fn run(self, ch: Channel<i64>) {
        let value = ch.recv();
    }
}

async fn main() {
    let m = Mutex::new(0);
    let ch: Channel<i64> = Channel::new();
    let a: A = new C();
    lock m as value {
        a.run(ch);
    }
}
"#,
        "C::run",
    );
}

/// d06: siblings are unioned. The base-typed receiver can be either one, so
/// the waiting sibling counts.
#[test]
fn d06_waiting_sibling_is_reported_through_the_base() {
    assert_lock_effect_witness(
        r#"
open class Base {
    pub open fn run(self, ch: Channel<i64>) {}
}

class Quiet extends Base {
    pub override fn run(self, ch: Channel<i64>) {}
}

class Loud extends Base {
    pub override fn run(self, ch: Channel<i64>) {
        let value = ch.recv();
    }
}

async fn main() {
    let m = Mutex::new(0);
    let ch: Channel<i64> = Channel::new();
    let base: Base = new Quiet();
    lock m as value {
        base.run(ch);
    }
}
"#,
        "Loud::run",
    );
}

/// d07: the union is bounded below by the declared type. Naming the pure
/// sibling excludes its cousin, so the widening stays type-directed.
#[test]
fn d07_sibling_type_does_not_see_its_cousin() {
    assert_no_lock_effect(
        r#"
open class Base {
    pub open fn run(self, ch: Channel<i64>) {}
}

class Quiet extends Base {
    pub override fn run(self, ch: Channel<i64>) {}
}

class Loud extends Base {
    pub override fn run(self, ch: Channel<i64>) {
        let value = ch.recv();
    }
}

async fn main() {
    let m = Mutex::new(0);
    let ch: Channel<i64> = Channel::new();
    let quiet = new Quiet();
    lock m as value {
        quiet.run(ch);
    }
}
"#,
    );
}

/// d08: a subclass that does not override still runs the inherited body,
/// so the upward part of resolution must survive the widening.
#[test]
fn d08_non_overriding_subclass_reaches_the_inherited_body() {
    assert_lock_effect_witness(
        r#"
open class Base {
    pub open fn run(self, ch: Channel<i64>) {
        let value = ch.recv();
    }
}

class Derived extends Base {}

async fn main() {
    let m = Mutex::new(0);
    let ch: Channel<i64> = Channel::new();
    let derived = new Derived();
    lock m as value {
        derived.run(ch);
    }
}
"#,
        "Base::run",
    );
}

/// d09: an instance call can never select a static method, so a waiting
/// static of the same name must stay out of the union.
#[test]
fn d09_static_namesake_is_not_a_dispatch_target() {
    assert_no_lock_effect(
        r#"
class Worker {
    pub fn run(self) -> i64 { return 1; }

    pub static fn helper(ch: Channel<i64>) -> i64 {
        return ch.recv();
    }
}

async fn main() {
    let m = Mutex::new(0);
    let worker = new Worker();
    lock m as value {
        let got = worker.run();
    }
}
"#,
    );
}

/// d10: static calls are not virtual and keep their exact single edge.
#[test]
fn d10_static_call_still_records_its_edge() {
    assert_lock_effect_witness(
        r#"
class Worker {
    pub static fn helper(ch: Channel<i64>) -> i64 {
        return ch.recv();
    }
}

async fn main() {
    let m = Mutex::new(0);
    let ch: Channel<i64> = Channel::new();
    lock m as value {
        let got = Worker::helper(ch);
    }
}
"#,
        "Worker::helper",
    );
}

/// d11: `new C()` names an exact class, so the constructor edge is direct.
#[test]
fn d11_constructor_wait_is_reported() {
    assert_lock_effect_witness(
        r#"
class Worker {
    pub value: i64;

    pub init(self, ch: Channel<i64>) {
        self.value = ch.recv();
    }
}

async fn main() {
    let m = Mutex::new(0);
    let ch: Channel<i64> = Channel::new();
    lock m as value {
        let worker = new Worker(ch);
    }
}
"#,
        "Worker::init",
    );
}

/// d12: an interface receiver keeps its synthetic union node, which
/// `connect_interface_lock_effects` wires to the implementations. The class
/// hierarchy must not shadow that path.
#[test]
fn d12_interface_receiver_uses_the_interface_union_node() {
    assert_lock_effect_witness(
        r#"
interface Receiver extends Send {
    fn run(self, ch: Channel<i64>);
}

class Impl implements Receiver {
    pub fn run(self, ch: Channel<i64>) {
        let value = ch.recv();
    }
}

async fn main() {
    let m = Mutex::new(0);
    let ch: Channel<i64> = Channel::new();
    let receiver: Receiver = new Impl();
    lock m as value {
        receiver.run(ch);
    }
}
"#,
        "Receiver::run",
    );
}

/// d13: one call site is one diagnostic. Recording an edge per union member
/// must not multiply the error the user sees.
#[test]
fn d13_many_waiting_overrides_report_once() {
    let labels = lock_effect_labels(
        r#"
open class Base {
    pub open fn run(self, ch: Channel<i64>) {}
}

class One extends Base {
    pub override fn run(self, ch: Channel<i64>) {
        let value = ch.recv();
    }
}

class Two extends Base {
    pub override fn run(self, ch: Channel<i64>) {
        let value = ch.recv();
    }
}

class Three extends Base {
    pub override fn run(self, ch: Channel<i64>) {
        let value = ch.recv();
    }
}

async fn main() {
    let m = Mutex::new(0);
    let ch: Channel<i64> = Channel::new();
    let base: Base = new One();
    lock m as value {
        base.run(ch);
    }
}
"#,
    );
    assert_eq!(labels.len(), 1, "expected one diagnostic, got {labels:?}");
}

/// d14: a same-named method in a disjoint hierarchy is not reachable from
/// this receiver. Without the subclass test the union would be "every class
/// that declares the name".
#[test]
fn d14_unrelated_hierarchy_is_excluded() {
    assert_no_lock_effect(
        r#"
open class Base {
    pub open fn run(self, ch: Channel<i64>) {}
}

class Derived extends Base {
    pub override fn run(self, ch: Channel<i64>) {}
}

class Stranger {
    pub fn run(self, ch: Channel<i64>) {
        let value = ch.recv();
    }
}

async fn main() {
    let m = Mutex::new(0);
    let ch: Channel<i64> = Channel::new();
    let base: Base = new Derived();
    lock m as value {
        base.run(ch);
    }
}
"#,
    );
}

/// d15: a `Type::Generic` receiver resolves through its named head, not
/// through its type arguments. Classes are not generic in v1, so the shape
/// arises from a generic interface receiver.
#[test]
fn d15_generic_receiver_resolves_through_its_named_head() {
    assert_lock_effect_witness(
        r#"
interface Receiver<T> extends Send {
    fn run(self, ch: Channel<i64>);
}

class Impl implements Receiver<i64> {
    pub fn run(self, ch: Channel<i64>) {
        let value = ch.recv();
    }
}

async fn main() {
    let m = Mutex::new(0);
    let ch: Channel<i64> = Channel::new();
    let receiver: Receiver<i64> = new Impl();
    lock m as value {
        receiver.run(ch);
    }
}
"#,
        "Receiver::run",
    );
}

/// d16: the fixpoint must terminate on a self-recursive body and still
/// report its own direct wait.
#[test]
fn d16_recursive_method_terminates_and_reports() {
    assert_lock_effect_witness(
        r#"
class Worker {
    pub fn run(self, ch: Channel<i64>, n: i64) -> i64 {
        if n <= 0 {
            return ch.recv();
        }
        return self.run(ch, n - 1);
    }
}

async fn main() {
    let m = Mutex::new(0);
    let ch: Channel<i64> = Channel::new();
    let worker = new Worker();
    lock m as value {
        let got = worker.run(ch, 3);
    }
}
"#,
        "Worker::run",
    );
}

/// d17: calling an `async` override creates a Task and returns; only a
/// surrounding `await` suspends. The widened union must keep that rule.
#[test]
fn d17_async_override_does_not_propagate() {
    assert_no_lock_effect(
        r#"
open class Base {
    pub open async fn run(self, ch: Channel<i64>) {}
}

class Derived extends Base {
    pub override async fn run(self, ch: Channel<i64>) {
        let value = ch.recv();
    }
}

async fn main() {
    let m = Mutex::new(0);
    let ch: Channel<i64> = Channel::new();
    let base: Base = new Derived();
    lock m as value {
        base.run(ch);
    }
}
"#,
    );
}

/// d18: an `open` class with no subclass in the unit has itself as its only
/// dispatch target, so the widening neither loses the direct body nor
/// invents a second witness.
#[test]
fn d18_open_class_without_subclasses_reports_itself_once() {
    assert_lock_effect_witness(
        r#"
open class Base {
    pub open fn run(self, ch: Channel<i64>) {
        let value = ch.recv();
    }
}

async fn main() {
    let m = Mutex::new(0);
    let ch: Channel<i64> = Channel::new();
    let base = new Base();
    lock m as value {
        base.run(ch);
    }
}
"#,
        "Base::run",
    );
}

/// d19: the union is only consulted for calls written under a lock. The
/// same waiting override outside the critical section is legal.
#[test]
fn d19_call_outside_the_lock_is_legal() {
    assert_no_lock_effect(
        r#"
open class Base {
    pub open fn run(self, ch: Channel<i64>) {}
}

class Derived extends Base {
    pub override fn run(self, ch: Channel<i64>) {
        let value = ch.recv();
    }
}

async fn main() {
    let m = Mutex::new(0);
    let ch: Channel<i64> = Channel::new();
    let base: Base = new Derived();
    base.run(ch);
    lock m as value {
        let ignored = 1;
    }
}
"#,
    );
}

/// d20: the override's wait can be two hops away. The union feeds the same
/// transitive fixpoint as a direct edge.
#[test]
fn d20_override_reaches_its_wait_through_a_helper() {
    assert_lock_effect_witness(
        r#"
fn wait_for(ch: Channel<i64>) -> i64 {
    return ch.recv();
}

open class Base {
    pub open fn run(self, ch: Channel<i64>) {}
}

class Derived extends Base {
    pub override fn run(self, ch: Channel<i64>) {
        let value = wait_for(ch);
    }
}

async fn main() {
    let m = Mutex::new(0);
    let ch: Channel<i64> = Channel::new();
    let base: Base = new Derived();
    lock m as value {
        base.run(ch);
    }
}
"#,
        "Derived::run",
    );
}

/// d21: a class declaring an unrelated method of another name contributes
/// nothing; the union is keyed by the called name only.
#[test]
fn d21_union_is_keyed_by_the_called_name() {
    assert_no_lock_effect(
        r#"
open class Base {
    pub open fn run(self, ch: Channel<i64>) {}
}

class Derived extends Base {
    pub fn other(self, ch: Channel<i64>) {
        let value = ch.recv();
    }
}

async fn main() {
    let m = Mutex::new(0);
    let ch: Channel<i64> = Channel::new();
    let base: Base = new Derived();
    lock m as value {
        base.run(ch);
    }
}
"#,
    );
}

/// d22: the reported witness does not depend on declaration order. Both
/// spellings of the same hierarchy name the same waiting body.
#[test]
fn d22_reported_witness_is_declaration_order_independent() {
    let first = lock_effect_labels(
        r#"
open class Base {
    pub open fn run(self, ch: Channel<i64>) {}
}

class Alpha extends Base {
    pub override fn run(self, ch: Channel<i64>) {
        let value = ch.recv();
    }
}

class Beta extends Base {
    pub override fn run(self, ch: Channel<i64>) {}
}

async fn main() {
    let m = Mutex::new(0);
    let ch: Channel<i64> = Channel::new();
    let base: Base = new Beta();
    lock m as value {
        base.run(ch);
    }
}
"#,
    );
    let second = lock_effect_labels(
        r#"
open class Base {
    pub open fn run(self, ch: Channel<i64>) {}
}

class Beta extends Base {
    pub override fn run(self, ch: Channel<i64>) {}
}

class Alpha extends Base {
    pub override fn run(self, ch: Channel<i64>) {
        let value = ch.recv();
    }
}

async fn main() {
    let m = Mutex::new(0);
    let ch: Channel<i64> = Channel::new();
    let base: Base = new Beta();
    lock m as value {
        base.run(ch);
    }
}
"#,
    );
    assert_eq!(first, second, "witness depends on declaration order");
}

/// d23: when one helper reaches both a suspending and a blocking owner, the
/// blocking one is reported.
///
/// The shared fixpoint seeds the kind-specific lattice bit
/// instead of one boolean, and `EffectSummary::witness` answers from the
/// lowest set bit in the query — `MAY_BLOCK` (bit 1) before `MAY_SUSPEND`
/// (bit 2). That deliberately outranks the older "lexicographically smallest
/// reachable owner" rule, which would name `a_wait_suspend` here: blocking a
/// scheduler worker starves every other task on it, while a suspension only
/// parks this one, so the worse of the two is the one worth naming. The
/// min-owner rule still decides ties *within* a bit, which is what d22 pins.
#[test]
fn d23_blocking_witness_outranks_a_lexicographically_smaller_suspend() {
    let errors = check_source(
        r#"
class Hub {
    pub fn a_wait_suspend(self, ch: Channel<i64>) {
        let value = ch.recv();
    }

    pub fn z_wait_block(self, flag: BlockingCell<bool>) {
        let ready = flag.get();
    }

    pub fn fanout(self, ch: Channel<i64>, flag: BlockingCell<bool>) {
        self.a_wait_suspend(ch);
        self.z_wait_block(flag);
    }
}

async fn main() {
    let m = Mutex::new(0);
    let ch: Channel<i64> = Channel::new();
    let flag = BlockingCell::new(false);
    let hub = new Hub();
    lock m as value {
        hub.fanout(ch, flag);
    }
}
"#,
    );
    let lock_effects = errors
        .iter()
        .filter(|diagnostic| diagnostic.code == ErrorCode::E2604)
        .collect::<Vec<_>>();
    assert_eq!(
        lock_effects.len(),
        1,
        "expected exactly one E2604 for the one call site, got {lock_effects:?}"
    );
    let labels = lock_effects[0]
        .labels
        .iter()
        .map(|label| label.message.clone())
        .collect::<Vec<_>>();
    assert!(
        labels
            .iter()
            .any(|message| message.contains("may block the scheduler worker")),
        "expected the blocking witness, got {labels:?}"
    );
    assert!(
        labels
            .iter()
            .any(|message| message.contains("BlockingCell.get")),
        "expected the blocking operation to be named, got {labels:?}"
    );
}
