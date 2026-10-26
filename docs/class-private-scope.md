# Class-scoped private members

Date: 2026-10-26. Decision: retain the existing class-scoped visibility rule.

A class member without `pub` or `prot` is private to its declaring class.
A free function or another class cannot access it, even in the same file or
module. A subclass does not gain access to its base class's private members;
`prot` is available for members intended for subclasses. Methods of the declaring
class may access private members, including through another instance of that class.
This is distinct from the module visibility of top-level declarations.

Keeping this behavior preserves class encapsulation and existing programs. Making
private members module-visible would implicitly grant access to unrelated code
whenever it shares a module. There is no visibility or ABI change in this decision.

E0501 help now states the class scope and the same-module restriction, followed
by an actionable remedy. Expose a method with `pub`, provide a public accessor,
or provide a public initializer/factory. An implicit memberwise constructor also
cannot initialize private fields from outside the declaring class. Prefer an
explicit public constructor/factory over exposing fields solely for construction.

For example, a free function cannot call `Parser.done()` when `done` is private.
[The runnable example](../example/class_private_scope.wi) keeps the implementation
private and exposes `Parser::make` and `Parser::is_done`. Run it with
`cargo run --bin willow -- run example/class_private_scope.wi`; it prints `true`.
