use super::*;

// ── Example files ───────────────────────────────────────────────────────────

fn runnable_example_cases() -> &'static [(&'static str, &'static str)] {
    &[
        (
            "example/run_queue_metrics.wi",
            "true\ntrue\ntrue\ntrue\ntrue\ntrue\ntrue\ntrue\ntrue\n",
        ),
        (
            "example/arithmetic.wi",
            "27\n15\n126\n3\n3\n54\n3\ntrue\n1024\n1\n",
        ),
        ("example/array_fast_paths.wi", "42\ntrue\ntrue\n42\n7\n"),
        ("example/array_growth.wi", "5\n55\n25\n16\n3\n"),
        (
            "example/array_push_growth.wi",
            "true\ntrue\ntrue\ntrue\ntrue\ntrue\n",
        ),
        ("example/arrays.wi", "4\n10\n40\n100\n99\n2\nbob\ntrue\n"),
        ("example/async_sleep.wi", "42\n"),
        ("example/async_sleep_ordering.wi", "1\n2\n3\n6\ntrue\n210\n"),
        ("example/async_yield.wi", "1\n2\n11\n12\n3\n"),
        ("example/async_concurrent.wi", "465\n"),
        ("example/async_preemption.wi", "42\n"),
        (
            "example/async_frame_narrowing.wi",
            "2\n1\n102\n13\n12\n3\n30\ntask\n4\n7\nhello\n6\n",
        ),
        ("example/atomics.wi", "9\n9\n100\ntrue\n"),
        (
            "example/lir_atomics.wi",
            "10\n15\n12\n19\ntrue\n48\n23\n18\n18\n100\nfalse\n50\ntrue\ntrue\nfalse\n",
        ),
        ("example/async_cooperative.wi", "1\n2\n3\n"),
        ("example/async_string_param.wi", "hello, willow\n"),
        ("example/booleans.wi", "true\nfalse\ntrue\ntrue\n"),
        ("example/classes_objects.wi", "Alice\n33\n"),
        ("example/class_hierarchy.wi", "3\n"),
        (
            "example/class_inheritance_cycle_rejected.wi",
            "Leaf: 1 2 3\nsum: 6\n",
        ),
        ("example/class.wi", "42\n"),
        (
            "example/class_method_dispatch.wi",
            "100\n220\n300\n22000\nledger\n",
        ),
        (
            "example/class_layout_declaration_order.wi",
            "6\n1\n2\n3\n1001\n2,4,8\n",
        ),
        (
            "example/class_vtable_dispatch.wi",
            "shape=4\nshape=16\nshape=16\ncircle=48\n5\ninvoice\n",
        ),
        ("example/command_line_args.wi", "0\n0\ntrue\ntrue\n"),
        (
            "example/codegen_invariants.wi",
            "2\ntrue\n2\n-1\nhello Alice\n11\n12\nBob\n12\nBob scored 12\n14\n8\n21\n-1\n12\n7\n",
        ),
        ("example/constructor_flow.wi", "zero\n0\none\n1\nmany\n2\n"),
        ("example/constructor_lambdas.wi", "8\n402\n"),
        (
            "example/map_float_keys.wi",
            "true\n1\n20\n20\n{0.0: 20}\ntrue\nfalse\nNaN cannot be used as a Map key\n1\n",
        ),
        ("example/constructor_visibility.wi", "pub\n42\n7\n"),
        ("example/constructors.wi", "John\n20\n7\n"),
        ("example/control_flow.wi", "120\n"),
        ("example/debug_source_map.wi", "12\n"),
        (
            "example/defer_panic_termination.wi",
            "body\ncleanup starts\nrecovered: cleanup failed\nafter scope\n",
        ),
        ("example/early_return.wi", "7\n0\n12\n"),
        ("example/example.wi", "50\ntrue\n"),
        ("example/fib.wi", "6765\n"),
        ("example/fib_bench.wi", "6765\n"),
        ("example/f64_parse.wi", "3.5\ntrue\nNaN\nparse failed\n"),
        ("example/floats.wi", "4\ntrue\n-4\n"),
        ("example/fn_values.wi", "20\n25\n30\n107\n104\n"),
        ("example/lambda_shadowing.wi", "101\n201\n102\n"),
        ("example/for_loops.wi", "6\n1\n2\n3\n5050\n9\n"),
        ("example/frozen_array.wi", "5\n4\n10\n"),
        ("example/frozen_map.wi", "3\n2\ntrue\n150\n"),
        ("example/frozen_map_gc.wi", "449\ntwo!\n?\n1536\n2\n"),
        (
            "example/parallel_map.wi",
            "[25, 1, 16, 4, 9]\n[10, 2, 8, 4, 6]\n",
        ),
        (
            "example/parallel_map_cancel.wi",
            "cancelled\n5997\n2997\n[15, 3, 12, 6, 9]\n320\n",
        ),
        (
            "example/lir_parallel_map.wi",
            "[25, 1, 16, 4, 9]\n[15, 11, 14, 12, 13]\n[10, 2, 8, 4, 6]\n\
             [35, 11, 26, 14, 19]\n46\n[25, 1, 16, 4, 9] [15, 11, 14, 12, 13]\n\
             []\ncancelled\n",
        ),
        ("example/gc_linked_list.wi", "6\n"),
        ("example/gc_mutator_registration.wi", "ledger\n288\n"),
        (
            "example/enum_match.wi",
            "north\nwest\n78.53975\n12\n0.0\nzero\nnonzero\nyes\nno\n",
        ),
        ("example/generic_enum_empty/main.wi", "1\n2\n"),
        ("example/unqualified_enum_pair.wi", "42\n"),
        ("example/unqualified_enum_variant.wi", "42\n1007\n-1\n"),
        (
            "example/interface_box_dynamic_dispatch.wi",
            "20\n60\n200\n90\n140\n18\n9\n",
        ),
        ("example/leibniz_pi.wi", "3.141592663589326\n"),
        ("example/locks.wi", "5\ndev\nprod\nfalse\ntrue\n"),
        ("example/blocking_cell_gc.wi", "1999000\n6\ntrue\n"),
        (
            "example/lock_match_arm.wi",
            "10\n6\n6\ncredited\ndebited\n10\n100\n20\n3\n123\n\
             draft\nfinal\n120\n",
        ),
        (
            "example/lir_local_binding_order.wi",
            "41\n10\n307\n10\nheld\nboxed\n42\nmany\n16\n5\n-1\n",
        ),
        (
            "example/lir_locks.wi",
            "true\nfalse\n809\n4\n48\n3\n9\n42\n1. inside the section\n\
             2. still holding the lock\n3. lock released, write published\n\
             4. outside the section\nok\nrecovered: inside the section\n3\n\
             111\npublished\ntrue\nbuild/done\n",
        ),
        (
            "example/scheduler_aware_lock.wi",
            "true\nfalse\n115\n1115\n2\n3\nwillow\n1. inside the section\n\
         2. still holding the lock\n3. lock released, write published\n\
         4. outside the section\n2\n",
        ),
        (
            "example/scheduler_aware_rwlock.wi",
            "true\nfalse\n7\n1000\nwillow\n",
        ),
        ("example/match_color.wi", "green\n"),
        (
            "example/method_receiver_roots.wi",
            "original!\nreplacement!\n",
        ),
        ("example/functions.wi", "25\ntrue\n"),
        ("example/hello.wi", "50"),
        ("example/hello_world.wi", "Hello, world!\n"),
        (
            "example/gc_scalable_bitmap.wi",
            "x\nxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx\n",
        ),
        ("example/compiler_scalability/main.wi", "3\n42\n"),
        ("example/iterative_scoped_traversal.wi", "value=13\ndone\n"),
        ("example/frontend_continuations.wi", "42\n"),
        ("example/current_unit_memory/main.wi", "42\n"),
        (
            "example/iterative_operator_validation.wi",
            "16\n20\ntrue\n19\n",
        ),
        ("example/import_demo/main.wi", "30\n42\n42\n99\n3\n42\n"),
        ("example/item_import_demo/main.wi", "7\n25\n"),
        ("example/interfaces.wi", "woof\n4\ntweet\n2\nwoof\ntweet\n"),
        ("example/trait_like_interfaces.wi", "3\n4\npoint\n"),
        ("example/generic_interfaces.wi", "10\nhello\nhello\nworld\n"),
        (
            "example/generic_interface_multi_instantiation.wi",
            "file\nfile\n",
        ),
        ("example/default_methods.wi", "Hello, Rex!\nBEEP Unit-7!\n"),
        (
            "example/interface_inheritance.wi",
            "Rex / Sam\n<Rex>\n<Rex>\n",
        ),
        (
            "example/interface_super_coercion.wi",
            "11\n21\n41\n21\n22\n21\n22\n21\n22\n22\n21\n22\n21\n22\n31\n11\n12\n21\n22\n",
        ),
        (
            "example/interface_diamond_widening.wi",
            "700\n7\n70\ncard\n7\n7\ncard\n",
        ),
        (
            "example/interface_downcast.wi",
            "woof\nmeow\nNemo is quiet\n",
        ),
        ("example/subclass_interface.wi", "dog\n4\npuppy\n4\n"),
        ("example/virtual_dispatch.wi", "19\n"),
        ("example/error_conversion.wi", "14\n1042\n"),
        ("example/main_result.wi", "42\n"),
        (
            "example/to_string.wi",
            "answer = 42\nok = true\npi = 3.5\np = (3, 4)\n",
        ),
        ("example/many_tasks.wi", "55\n"),
        (
            "example/match_arm_control_flow.wi",
            "negative\nzero\npositive\nempty\nbatch\nmissing\n9\neven sample\n\
             odd sample\nrising batch\n?\nno reading\n6\n7\n5\n6\n9\n",
        ),
        ("example/lambda_context.wi", "20\n12\n12\nyes\n"),
        ("example/async_reference_escape_rejected.wi", "42\n"),
        ("example/lazy_task_stack.wi", "cleanup\n210\n"),
        (
            "example/reference_width.wi",
            "reference\n4294967297\n4294967298\nreference!\n",
        ),
        (
            "example/match_binding_gc_roots.wi",
            "99\n10\n55\n9\n[note] kept!\n300\n7\n",
        ),
        (
            "example/constructor_reference_args.wi",
            "42\n22\n6\n20\nhi!/hi!\n31\n6\n",
        ),
        ("example/map_inference.wi", "42\n2\n"),
        (
            "example/map_key_value_kinds.wi",
            "16\n1\n37\ntwo\n42\ntrue\nfalse\n",
        ),
        ("example/maps.wi", "2\n31\n25\n-1\ntrue\nfalse\ntwo\n"),
        ("example/module_alias_demo/main.wi", "5\n16\n"),
        (
            "example/module_alias_spellings/main.wi",
            "high\nlow\n42\n10\ntrue\n7\ntrue\n",
        ),
        (
            "example/module_base_short_name/main.wi",
            "3\n12\n7\n5\n20\n50\n3\n12\n42\n1\n0\n",
        ),
        (
            "example/module_call_demo/main.wi",
            "5\n11\n12\nAlice: 12\n7\nBob=7\n=0\ntwo\n5\nrecovered: negative score\ntotal: 12\n",
        ),
        (
            "example/module_frame_demo/main.wi",
            "10\n3\n4\nnegative: negative reading\nempty: empty reading\n9\n\
             side: negative side\nzero: zero side\n1\n2\nbump: bumped past the limit\n6\n",
        ),
        ("example/module_class_demo/main.wi", "42\n12\n"),
        (
            "example/module_class_values/main.wi",
            "7\n12\n10\n12\n(2, 2) -> (10, 10)\norigin\n7\n12\n",
        ),
        (
            "example/module_class_inheritance_demo/main.wi",
            "1005\n6\n1005\n",
        ),
        (
            "example/module_inherited_interfaces/main.wi",
            "30\n100\n40\nparcel\nentry\n7\n21\n120\n",
        ),
        (
            "example/module_item_imports/main.wi",
            "25\n275\n0\n1100\n1100\n",
        ),
        (
            "example/module_cross_module_types/main.wi",
            "4\n12\n24\n9\nexpress\n",
        ),
        (
            "example/module_alias_identity/main.wi",
            "120\n60\n110\n60\n5\npremium\n",
        ),
        (
            "example/module_static_properties/main.wi",
            "1\n2\n8\nvault\n2\nledger\n4\ntrue\n",
        ),
        ("example/module_demo/main.wi", "12\n14\n"),
        (
            "example/module_dispatch_demo/main.wi",
            "300\n5400\n400\n50\n",
        ),
        (
            "example/module_typecheck_demo/main.wi",
            "3\n30\n6\n60\n60\n13\n",
        ),
        ("example/module_enum_demo/main.wi", "1\n2\n42\n"),
        // Option<i64> Some/None across a module, Wrap<i64> Val/Empty into
        // Result<i64, String>, a plain enum, a generic interface at i64.
        (
            "example/type_argument_arity/main.wi",
            "5\n0\n7\nempty\nnone\n2\n42\nmaybe\n1\n12\n",
        ),
        (
            "example/module_lir_bodies/main.wi",
            "12\n10\n7\nwin:rect\n1\n9\n30\n90\n12\n12\n12\n20\n13\n",
        ),
        (
            "example/module_class_visibility/main.wi",
            "7\n23\ncell:2,5\n7\n",
        ),
        ("example/module_import_scope/main.wi", "7\nsale:3\n9\n100\n"),
        (
            "example/enum_identity_aliases/main.wi",
            "3\n10\n2\n3\n2\n3\n3\nmid\nhigh\n7\n0\ntrue\nfalse\n4\ntrue\ntrue\n200\n",
        ),
        (
            "example/imported_enum_match/main.wi",
            "1\n42\n200\n8\nink\n2\n20\n",
        ),
        (
            "example/module_enum_tables/main.wi",
            "high[]\nlow[]\nloud{}\n-{}\nfar()\n.low.low.low.low\n11\n10\nloud/ok\ntrue\nlow\nloud\n",
        ),
        (
            "example/module_enum_identity/main.wi",
            "signal:high\nsignal:low\nother:off\nother:on\nother:extra\nsignal:high\nother:extra\ncarried\n",
        ),
        ("example/direct_import_demo/main.wi", "7\n1\n99\n"),
        (
            "example/direct_import_iface_enum_demo/main.wi",
            "25\n12\n3\n",
        ),
        ("example/interface_advanced_demo/main.wi", "11\n10\n42\n"),
        ("example/mutability.wi", "6\n15\ntrue\n"),
        ("example/nested_loops.wi", "30\n"),
        (
            "example/nil_guard_demo.wi",
            "42\n-7\n0\ntrue\nfalse\nfalse\n126\n99\n",
        ),
        ("example/nil_nullable.wi", "0\n10\n20\ntrue\n10\n"),
        ("example/nil_safe_chain.wi", "60\n3\n30\n-1\n120\n"),
        ("example/object_argument.wi", "42\n42\n99\n41\n"),
        (
            "example/option_result.wi",
            "true\ntrue\n10\n10\n10\n99\n20\ntrue\n2\ntrue\n42\n10\ntrue\ntrue\n8\n8\n8\n99\nsomething failed\n24\ntrue\nprefix: something failed\n8\n2\nnot even\n0\n8\n",
        ),
        (
            "example/option_result_inference.wi",
            "true\n10\ntrue\n7\n5\ntrue\n42\n-1\n",
        ),
        (
            "example/option_contextual_nested.wi",
            "-1\n-2\n42\n-2\ntrue\n",
        ),
        (
            "example/option_interface_context.wi",
            "dog:rex\nnone\nrobot:7\ndog:spot\n",
        ),
        (
            "example/option_repr_niche.wi",
            "text\nmissing\n4\ntrue\ntrue\ntrue\nmap\ntrue\nasync\n",
        ),
        (
            "example/option_absence.wi",
            "true\ntrue\nvalue: willow\nmissing\ntrue\ntrue\n",
        ),
        ("example/option_nil_migration.wi", "true\ntrue\ntrue\n"),
        ("example/prot_demo.wi", "10\n9\n20\n18\n17\n15\n14\n"),
        ("example/result_propagation.wi", "84\n-1\n52\n-1\n-1\n"),
        ("example/print_test.wi", "1230\n42\ntrue\nfalsetrue\n"),
        (
            "example/closure_enum_identity/main.wi",
            "true\ntrue\nfalse\n",
        ),
        ("example/native_stack_overflow.wi", "8\n"),
        ("example/recursion.wi", "3628800\n1024\n6\n"),
        ("example/range_value.wi", "2\n6\n4\n14\n0\n1\n2\n"),
        (
            "example/reference_args_control_flow.wi",
            "10\n15\n12\n14\n3\n9\n",
        ),
        (
            "example/references.wi",
            "11\n22\ntrue\nhi!\nhi?\nold box\nold box!\nnew box\n3\n",
        ),
        (
            "example/rust_runtime_smoke.wi",
            "rust runtime\n42\n10\n21\n0\n",
        ),
        ("example/channel_producer.wi", "10\n20\n30\n"),
        (
            "example/channel_element_inference.wi",
            "value 0;value 1;value 2;value 3;\n6 note 0;note 1;note 2;note 3;\n30\n\
             value 0;value 1;value 2;value 3;\n\
             bounded 0;bounded 1;bounded 2;bounded 3;\nab\n",
        ),
        ("example/concurrent_counts.wi", "concurrent output"),
        ("example/coop_select.wi", "100\n200\n300\n"),
        ("example/parallel_tasks.wi", "55\n144\n610\n42\nfalse\n"),
        ("example/select.wi", "0\n42\n7\n"),
        (
            "example/select_blocking.wi",
            "got 42\n0\n1\n2\nnothing ready\nrecovered: select would block forever: \
             no case can become ready and there is no default case\ndone\n",
        ),
        ("example/self_demo.wi", "10\n10\n10\n"),
        ("example/send_sync_markers.wi", "36\n81\n"),
        ("example/spawn_await.wi", "9\n16\n25\n42\n"),
        ("example/static_inheritance.wi", "base\nbase\n3\nok\n"),
        ("example/static_members.wi", "3\n25\n40\n42\n"),
        ("example/static_mut.wi", "0\n10\n42\nstart\ndone\n"),
        (
            "example/static_properties.wi",
            "1\nwillow\ntrue\n1.5\n20\n100\n",
        ),
        (
            "example/exponentiation.wi",
            "1024\n64\n512\n64\n18\n-4\n4\n1\n1\n32\n64\n0\ntrue\n\
             recovered: negative exponent in integer `**`: -3\ndone\n1024\n0.125\n\
             1.414213562373095\n3\n4\n",
        ),
        ("example/grouped_imports.wi", "42\n"),
        ("example/std_imports.wi", "1\n42\n7\n-1\n"),
        ("example/strings.wi", "Hello, Willow\nstring concat\n"),
        ("example/string_greeting.wi", "hello, willow\ntrue\n"),
        (
            "example/symbol_namespace_demo/main.wi",
            "42\n100\n10\n40\n5\n15\nhi Alice\n6\n42\n121\n1\n2\n",
        ),
        ("example/task_sharing.wi", "6\n1\n2\n"),
        ("example/ternary.wi", "1\n-1\n0\n20\n99\n15\n8\n1\n"),
        ("example/types.wi", "10\n2.5\n10\n78.53975\ntrue\n"),
        ("example/super_class.wi", "3 cats:\nann\njohn\nben\n"),
        (
            "example/gc_safety_temporaries.wi",
            "Hx!\na!b!\nv!\np!q!r!\nn!\nbad!\n7!\n",
        ),
        ("example/comments.wi", "30\n9223372036854775807\n"),
        ("example/hir_demo.wi", "55\n"),
        ("example/ternary_variants.wi", "10\n-1\n7\n1\n"),
        ("example/nested_assignment.wi", "5\n42\n0\n9\n"),
        ("example/pattern_matching.wi", "42\nmissing value\n0\n"),
        ("example/array_iteration.wi", "4\n4\n15\n"),
        ("example/break_continue.wi", "64\n27\n6\n"),
        (
            "example/select_timeout.wi",
            "no message in time\n42\ngave up on the slow worker\n",
        ),
        (
            "example/channel_temporaries.wi",
            "nothing arrived\nitem-x\nitem-x\nitem-x\n3\n",
        ),
        (
            "example/lir_gc_strings.wi",
            "[core]\nl1|r2\nabababab\ntrue\nhit:zz\nhit:none\nn=big\nn=small\ntrue\n",
        ),
        (
            "example/lir_async_await.wi",
            "42\n10\nannounced\n[core]\n41\n46\n4\n42\n1\n[willow]!\n12\n[mid]?\n41\n[loud]!\n3\nbranched\ndone\n",
        ),
        (
            "example/lir_async_recovery.wi",
            "scalar: clean\n100\nscalar: no count\n7\ntext: clean\nreplaced\n\
             text: no label\nkept\nitems: clean\n20\nitems: no items\n9\n\
             holder: clean\nBob:20\nholder: no account\nAlice:10\n\
             inner: clean\nouter: clean\n11\ninner: inner stop\nouter: clean\n10\n\
             inner: clean\nouter: outer stop\n1\n\
             loop: ok\nloop: iteration 1\nloop: ok\n2\n\
             piped: clean\n5\npiped: dropped 5\n-1\n\
             scalar: no count\n7\n3\ndone\n",
        ),
        (
            "example/lir_gc_arrays.wi",
            "[0, 2, 4, 6, 8]\n5\n20\n<head>|<item>|<item>\n[\"<head>\"]\n[4, 3, 7]\n9\n13\n",
        ),
        (
            "example/lir_gc_objects.wi",
            "7\n10\n5\nitem!:ab\n16\n14\n30\n",
        ),
        (
            "example/lir_gc_collections.wi",
            "3\n{alpha:1: 0, beta:1: 1, gamma:1: 2}\ntrue\nfalse\n\
             {1: \"one\", 2: \"two\", 3: \"three\"}\n3\nfalse\n4\n3\nabtail\n2\n4\n",
        ),
        (
            "example/lir_gc_stats.wi",
            "true\ntrue\ntrue\ntrue\ntrue\n3\ntrue\nheap: ok\n",
        ),
        (
            "example/lir_scope_roots.wi",
            "0\n0\n0\n0\n0\n0\n0\n100\n101\n0\n6\nescapes into `kept`\n\
             inner: the inner label\ninner: the inner label\nthe outer label\n",
        ),
        (
            "example/lir_enum_match.wi",
            "north\neast\nsouth\nwest\nnorth\n0\n48\n15\n0\n\
             nothing\ncircle\nrect\nlabeled plate/plate\n3\n-1\n2.5\n1\n\
             100\n200\n21\nyes\nno\nnorth star\nuntagged\n\
             going north\ngoing elsewhere\n63\neast:rect\n",
        ),
        (
            "example/lir_option_result.wi",
            "5\n-1\nsome\nnone\n2\n-1\n2\n3\nalice\nunknown\ntrue\nfalse\ntrue\n\
             bob\nunknown\nbob/bob\n?\n25\n-2\n-1\n7\n-1\nok\nnot a digit: x\n\
             true\ntrue\n1\n-1\nnot a digit: x\n7\n9\n-1\n-1\n10\n-1\nsum!sum!\n\
             err:not a digit: q\n800\n500\nalice!\n(none)\n3\n-1\nq\n(none)\n10\n\
             -1\nal\n(none)\nleft\nnegative\n21\n0\n8\n",
        ),
        (
            "example/lir_divergence.wi",
            "zero\none\nmany\nnothing\ndescribing 2\nsome (many)\n5\n5\n\
             low\nhigh\norigin\ntop\nbody\n42\n2.5\ntrue\nwillow\n\
             3 items\nleft and right\ntrue / false\n3.141593\n{literal} 9\n\
             no placeholders\n",
        ),
        (
            "example/lir_function_values.wi",
            "42\n49\n42\n102\n81\n10\n-20\n5\n100\n-10\n17\n\
             hi!!\n[[core]]\nsay hello\n<there>\n\
             40\n-1\nbig\n?\n5\n-1\n4\n99\n\
             40\n-1\n4\ne:odd\n2\nodd\n4\n0\n4\n8\n",
        ),
        (
            "example/lir_closures.wi",
            "8\n15\n5\n18\n9\narea=12\n11.25\n0.0\n1\n42\n42\n25\n14\n63\n20\n",
        ),
        (
            "example/lir_interface_boxing.wi",
            "alpha/#7\ngamma/#8\ndelta/#9\n4\nthree!#2three#4\nsolo/#11\n3\n11\nHHTT\n",
        ),
        (
            "example/lir_interface_dispatch.wi",
            "36\n40\n[square]\n[rect]\n18\n20\n10\n35\n72\n[rect]\n[square]\n",
        ),
        (
            "example/lir_class_inheritance.wi",
            "5\n25\n25\n75\nshape=9u\nshape=9u\ncircle=27u\n16\n9\n23\n4\n\
             36\n36\n6\nC:circle/circle2\nB:hi\nu\nu\nu\n",
        ),
        (
            "example/lir_class_methods.wi",
            "12\n12\n- left ops\nops:12\n30\n60\nmany\n- left pay\npay:30\n\
             - left void\nvoid:empty\nops+pay\n42\n*seal7*\n112\n24\n11\n",
        ),
        (
            "example/lir_self_statics.wi",
            "1\n2\n2\nseat 2\n12\nseat\nrow\nclosed 2\n2\n42\n4\n3\n103\nseat 9\n",
        ),
        (
            "example/lir_static_initializers.wi",
            "42\n-5\ntrue\n3\nwillow\n42\n4\n0\n3\n7\n1\n8\n43\n1\n10\n3\n5\np8\n84\n11\n",
        ),
        (
            "example/lir_static_property_bodies.wi",
            "6\n3\n20\nbig\none\nmany\n2\n5\n0\n10\n7\n9\n10\n",
        ),
        (
            "example/lir_stage5_cutover.wi",
            "42\nboom at line 33\ncleaned up 8\n15\nnot positive\n99\n1\n",
        ),
        (
            "example/interface_reference_params.wi",
            "15\n20\n15\n75\n45\n5\n25\n<name!>\n6\n1\n6\n11\n18\n105\n300\n",
        ),
        (
            "example/file_io.wi",
            "saved by willow\nmissing file handled\nfalse\nsaved asynchronously\nfalse\n",
        ),
        ("example/tcp_echo.wi", "hello from Willow\n"),
        (
            "example/lir_io.wi",
            "on disk\nfalse\ntrue\nwritten by a task\nfalse\nhello over loopback\n",
        ),
        (
            "example/lir_namespace_aliases.wi",
            "aliased on disk\nfalse\naliased by a task\nfalse\n0\n[16, 4, 9]\ntrue\n",
        ),
        (
            "example/lir_match_bodies.wi",
            "halt\nstep 6\nlabel x\n101\n12\nlabelled here!\n0\n33 20!50!\n\
             step(2)\nstep\nstep/9\nasync step 12\nok\nrecovered: bad input\n\
             nothing queued\ngot 21\n",
        ),
        (
            "example/return_paths.wi",
            "1\n-1\n0\n16\n12\n16\n12\n5\n21\n4\n6\n8\n-1\n40\n0\n4\n9\n18\n1\n",
        ),
        (
            "example/lir_defer_scopes.wi",
            "leave doubled\n6\nleave positive\nleave pick\n1\nleave pick\n0\n0\n1\n2\n3\n\
             leave first_big\n3\n0\nleave inner\n1\nleave inner\n2\nleave deep\n2\n\
             0\n100\n200\n300\n2\n\
             else\nleave merged\n2\nleave merged\n1\n\
             scoped else\n70\nleave merged_scope\n2\n70\nleave merged_scope\n1\n\
             0\n10\n20\nleave merged_loop\n1\n\
             leave parse\nleave parse\nleave scaled\n30\n\
             leave parse\nleave parse\nleave scaled\nnegative\nbye\n",
        ),
        (
            "example/lir_main_result.wi",
            "4\n70\nin range\nlevel must not be negative\n60\nnormal\ndone\n",
        ),
        (
            "example/lir_match_suspend.wi",
            "30\n60\n0\n13\n21\n0\n21\n-1\nhello, willow\nhello, stranger\n\
             6\n0\n15\n21\n-2\n-1\n42\n0\n15\n-1\ncleanup\n12\n0\n21\n",
        ),
        (
            "example/structured_tasks.wi",
            "token cancelled\nscope complete\n30\n",
        ),
        (
            "example/channel_consumer.wi",
            "consumer 1 done\n42\nconsumer 2 done\ntrue\n",
        ),
        ("example/bounded_channel.wi", "10\n6\n100\n400\n7\n"),
        ("example/channel_many_waiters.wi", "500500\ntrue\n1\n3\n"),
        (
            "example/task_status_frame.wi",
            "false\n42\nfalse\n42\ntrue\n0\ntrue\n0\n56\n",
        ),
        (
            "example/task_await.wi",
            "42\n42\ntick done\nhello!\n42\n100\n-5\n8\ncancelled\n\
             12\n30\nchoosing once\n200\n",
        ),
        (
            "example/async_defer.wi",
            "closed conn-1\n10\nclosed conn-100\ntrue\n",
        ),
        (
            "example/defer_cleanup.wi",
            "closed db\n42\nclosed db\nread failed\n0\n0\n1\n10\nbye\n",
        ),
        (
            "example/defer_result_handling.wi",
            "ignored body\nhandled body\ncleanup failed\nblock body\nblock: cleanup failed\n",
        ),
        ("example/string_compare.wi", "true\ntrue\ntrue\ntrue\n"),
        ("example/task_cancel.wi", "10\ntrue\n"),
        // fib_task(10), fib_task(15), then fib_task(20) = 6765 clamped to 1000.
        // The recursive helpers the example documents are deliberately not
        // called from its async `main` — that is the point of the example.
        ("example/task_recursion_rejected.wi", "55\n610\n1000\n"),
        ("example/task_fan_in.wi", "125250\n9\n900\ntrue\n11\n"),
        (
            "example/collections_display.wi",
            "[3, 1, 4, 1, 5]\n[\"ann\", \"ben\"]\n{ann: 87, ben: 92}\nall 5 of [3, 1, 4, 1, 5]\n",
        ),
        (
            "example/panic_format.wi",
            "5\n(3, 4)\npi ~ 3.141593\nwillow is true\n{braces} and 7\n",
        ),
        (
            "example/panic_recover_diagnostics.wi",
            concat!(
                "in-range read:\n20\n",
                "out-of-range read:\n",
                "  message: array index out of bounds: the length is 3 but the index is 9\n",
                "  file set: true\n",
                // The location of the faulting statement inside `lookup`, not
                // of the caller that invoked it (willow-s9ej.7 review).
                "  raised at line 23\n",
                "-1\n",
                "missing value:\n",
                "  message: called `Option::unwrap()` on a `None` value\n",
                "-1\n",
                "present value:\n7\n",
                "after recovery:\n  received 99\n",
            ),
        ),
        (
            "example/panic_effect_self_dispatch.wi",
            "recovered:child hook\nafter\n",
        ),
        (
            "example/shared_ast_walk.wi",
            concat!(
                "ternary <- ternary\n",
                "binary <- binary\n",
                "array-element <- array-element\n",
                "match-arm <- match-arm\n",
                "method-argument <- method-argument\n",
                "new-argument <- new-argument\n",
                "static-argument <- static-argument\n",
                "unary-operand <- unary-operand\n",
                "nested-statement <- nested-statement\n",
                "control survived with 42\n",
                "control <- clean\n",
                "5\n",
            ),
        ),
        // One shared virtual-dispatch union feeding both the may-panic and the
        // lock-effect analyses (willow-uqzx.1.2). `recovered:` proves the
        // landing pad survived a panic that only a subclass override raises;
        // the three balances prove the locked sections still run.
        (
            "example/shared_call_graph.wi",
            concat!(
                "11\n",
                "12\n",
                "recovered: BadStep has no amount\n",
                "after the recover\n",
                "103\n",
                "108\n",
                "110\n",
            ),
        ),
        // One effect lattice and one fixpoint behind may-panic, E2604, and
        // E0810 (willow-uqzx.1.3). `no panic` proves the pure chain kept its
        // proof, `recovered: division by zero` proves a seed on `ratio`
        // propagated two hops to `guarded_average`, and `111` proves the eager
        // async call left the critical section wait-free.
        //
        // The `2 / 6 / 1 / recovered: Loud has no value` run pins the call
        // graph's lexical scope stack. `6` is the real `weigh` reached again
        // after its shadow's scope closed; the recover proves the outer
        // `sample` still dispatched on its own class, which the old flat
        // local-name map got wrong in the fail-open direction.
        (
            "example/shared_effect_fixpoint.wi",
            concat!(
                "20\n",
                "5\n",
                "no panic\n",
                "recovered: division by zero\n",
                "after the recover\n",
                "2\n",
                "6\n",
                "1\n",
                "recovered: Loud has no value\n",
                "after the shadowed scopes\n",
                "55\n",
                "111\n",
            ),
        ),
        (
            "example/panic_recover_service.wi",
            concat!(
                "sync requests:\n",
                "  finished request 1\n200 body=42\n",
                "  finished request 2\n",
                "  recovered: invalid payload -7 in request 2\n500\n",
                "  finished request 3\n200 body=10\n",
                "async requests:\n",
                "200 body=20\n500 recovered: invalid payload -1 in request 5\n200 body=6\n",
            ),
        ),
        // Every intrinsic family the resolver owns, in source order
        // (willow-uqzx, catalog item 7). A lowering that goes missing changes a
        // line here instead of failing silently.
        (
            "example/intrinsic_methods.wi",
            concat!(
                // scalar toString: i64, f64, bool, String
                "42 1.5\n",
                "true willow\n",
                // Array: toString, pop, len, freeze; FrozenArray: len
                "[1, 2, 3, 4]\n4\n3\n4\n3\n",
                // Map: toString, contains, len, get, freeze
                "{ann: 87, ben: 92}\ntrue\n2\n92\n3\n",
                // FrozenMap: len, contains, get
                "2\nfalse\n87\n",
                // AtomicI64: add, sub, swap return the previous value
                "5\n8\n6\n100\n",
                // AtomicBool: load, store, swap
                "false\ntrue\nfalse\n",
                // BlockingRwCell read/write, then BlockingCell get/set
                "dev\nprod\nfalse\ntrue\n",
                // Channel: recv, recv, awaited producer
                "7\n8\n2\n",
                // Task: is_cancelled, result, cancel
                "false\n10\ntrue\n",
                // CancellationToken: a child inherits cancellation downward only
                "false\ntrue\ntoken cancelled\nfalse\n",
                // TaskScope: is_cancelled, child, add, finish, then cancel
                "false\nscope complete\n30\ntrue\nscope task cancelled\n",
            ),
        ),
    ]
}

#[test]
fn test_runnable_example_catalog_is_complete() {
    let mut expected_paths = runnable_example_cases()
        .iter()
        .map(|(path, _)| path.to_string())
        .collect::<Vec<_>>();
    expected_paths.sort();
    let actual_paths = collect_runnable_example_entries();
    assert_eq!(
        actual_paths, expected_paths,
        "every runnable non-future example entrypoint should have an output assertion"
    );
}

#[test]
fn test_array_push_growth_example() {
    let (path, expected) = runnable_example_cases()
        .iter()
        .find(|(path, _)| *path == "example/array_push_growth.wi")
        .expect("array push growth example must be cataloged");
    let (out, ok) = compile_file_and_run(path);
    assert!(ok, "{path} failed to compile or run");
    assert_eq!(out, *expected);
}

#[test]
fn test_array_fast_paths_example() {
    let (path, expected) = runnable_example_cases()
        .iter()
        .find(|(path, _)| *path == "example/array_fast_paths.wi")
        .expect("array fast paths example must be cataloged");
    let (out, ok) = compile_file_and_run(path);
    assert!(ok, "{path} failed to compile or run");
    assert_eq!(out, *expected);
}

#[test]
fn test_constructor_flow_example() {
    let (out, ok) = compile_file_and_run("example/constructor_flow.wi");
    assert!(ok, "constructor flow example failed to compile or run");
    assert_eq!(out, "zero\n0\none\n1\nmany\n2\n");
}

#[test]
fn test_runnable_example_files_compile_and_run() {
    for &(path, expected) in runnable_example_cases() {
        let (out, ok) = compile_file_and_run(path);
        assert!(ok, "{path} failed to compile or run");
        if path == "example/concurrent_counts.wi" {
            let lines = out.lines().collect::<Vec<_>>();
            assert_eq!(lines.len(), 31, "{path} output mismatch: {out}");
            assert_eq!(lines[30], "1", "{path} must print the awaited result last");
            for task in 1..=3 {
                let mut previous = None;
                for count in 1..=10 {
                    let value = (task * 100 + count).to_string();
                    let at = lines[..30]
                        .iter()
                        .position(|line| *line == value)
                        .unwrap_or_else(|| panic!("{path} missing {value}: {out}"));
                    if let Some(previous) = previous {
                        assert!(previous < at, "{path} reordered task {task}: {out}");
                    }
                    previous = Some(at);
                }
            }
        } else if path == "example/task_sharing.wi" {
            let lines = out.lines().collect::<Vec<_>>();
            assert_eq!(lines.len(), 3, "{path} output mismatch: {out}");
            assert_eq!(lines[0], "6", "{path} lost an atomic increment: {out}");
            assert!(
                matches!(&lines[1..], ["1", "2"] | ["2", "1"]),
                "{path} channel results mismatch: {out}"
            );
        } else if path == "example/coop_select.wi" {
            // Independent producers can become runnable together; select is
            // allowed to consume either ready channel first.
            assert!(
                matches!(out.as_str(), "100\n200\n300\n" | "200\n100\n300\n"),
                "{path} channel values or final sum mismatch: {out}"
            );
        } else if path == "example/async_sleep_ordering.wi" {
            // Deadline promotion does not order execution on parallel workers.
            let lines = out.lines().collect::<Vec<_>>();
            assert_eq!(lines.len(), 6, "{path} output mismatch: {out}");
            assert_eq!(&lines[3..], &["6", "true", "210"], "{path}: {out}");
            let mut workers = lines[..3].to_vec();
            workers.sort_unstable();
            assert_eq!(workers, ["1", "2", "3"], "{path}: {out}");
        } else if path == "example/async_yield.wi" {
            let lines = out.lines().collect::<Vec<_>>();
            assert_eq!(lines.len(), 5, "{path} output mismatch: {out}");
            assert_eq!(lines[4], "3", "{path} must print the awaited sum last");
            for (start, finish) in [("1", "11"), ("2", "12")] {
                let start_at = lines[..4].iter().position(|line| *line == start).unwrap();
                let finish_at = lines[..4].iter().position(|line| *line == finish).unwrap();
                assert!(start_at < finish_at, "{path} reordered task {start}: {out}");
            }
        } else {
            assert_eq!(out, expected, "{path} output mismatch");
        }
    }
}

#[test]
fn test_future_examples_are_documented_not_compiled() {
    let future_examples = collect_wi_files("example/future");
    assert!(
        future_examples.len() >= 8,
        "future example catalog should stay broad"
    );

    for path in future_examples {
        let source = fs::read_to_string(&path).unwrap_or_else(|err| {
            panic!("failed to read future example {path}: {err}");
        });

        assert!(
            source.contains("// status: future"),
            "{path} must be marked as a future example"
        );
        assert!(
            source.contains("// feature:"),
            "{path} must name the language feature it documents"
        );
    }
}

#[test]
fn test_future_example_catalog_covers_planned_features() {
    let combined = collect_wi_files("example/future")
        .iter()
        .map(|path| fs::read_to_string(path).unwrap())
        .collect::<Vec<_>>()
        .join("\n");

    // `enum`/`match` graduated from future/ when statement-position match
    // shipped (willow-zvkv, example/pattern_matching.wi).
    let required_fragments = ["import ", "class ", "extends ", "String", "[i64]", "for "];

    for fragment in required_fragments {
        assert!(
            combined.contains(fragment),
            "future examples should cover `{fragment}`"
        );
    }
}

#[test]
fn test_future_example_catalog_covers_constructor_init_diagnostics() {
    let source = fs::read_to_string("example/future/diagnostic_constructor_init_rules.wi")
        .expect("missing constructor init diagnostic example");
    assert!(source.contains("// status: future"));
    assert!(source.contains("// feature: constructor init diagnostics"));
    assert!(source.contains("static init(self)"));
    assert!(source.contains("fn init(self)"));
    assert!(source.contains("static fn init()"));
}

#[test]
fn test_future_private_member_diagnostic_example_reports_only_privacy_error() {
    let stderr = compile_file_error_stderr("example/future/diagnostic_private_member.wi");
    assert!(stderr.contains("error[E0501]"), "{stderr}");
    assert!(
        stderr.contains("field `balance` of class `Account` is private"),
        "{stderr}"
    );
    assert!(
        !stderr.contains("error[E0835]"),
        "future diagnostic example should not include stale static-call error:\n{stderr}"
    );
}

/// Both numeric forms have graduated to the runnable root example. Keep the
/// future catalog clear of the retired E2501 staging artifact.
#[test]
fn test_exponentiation_has_no_future_staging_example() {
    assert!(!Path::new("example/future/exponentiation.wi").exists());
    let source = fs::read_to_string("example/exponentiation.wi")
        .expect("missing runnable exponentiation example");
    assert!(source.contains("float_power(2.0, 0.5)"));
    assert!(source.contains("powf(16.0, 0.5)"));
}

/// Stage 5 graduates the RwLock forms to a runnable root example and removes
/// the old future/gate contract.
#[test]
fn test_scheduler_aware_rwlock_example_has_both_modes() {
    let source = fs::read_to_string("example/scheduler_aware_rwlock.wi")
        .expect("missing scheduler-aware rwlock example");
    assert!(source.contains("lock read settings as value"));
    assert!(source.contains("lock write settings as mut value"));
    assert!(source.contains("add_many(counter, 250)"));
    assert!(source.contains("gc_collect()"));
}

#[test]
fn test_example_readme_explains_runnable_and_future_examples() {
    let readme = fs::read_to_string("example/README.md").expect("missing example README");

    assert!(readme.contains("Root `*.wi` files"));
    assert!(readme.contains("future/**/*.wi"));
    assert!(readme.contains("// status: future"));
}

#[test]
fn test_release_example_build_runs() {
    let (out, ok) = compile_file_and_run_with_args("example/functions.wi", &["--release"]);
    assert!(ok, "release compilation failed");
    assert_eq!(out, "25\ntrue\n");
}
