#!/bin/sh
case "$1" in
    */willow_runtime-*)
        lldb --batch -o run -o 'thread backtrace all' -- "$@"
        # Diagnostic runs must never be mistaken for a passing test gate.
        exit 1
        ;;
    *) exec "$@" ;;
esac
